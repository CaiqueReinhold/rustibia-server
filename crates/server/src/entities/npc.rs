use std::{collections::BTreeMap, sync::Arc};

use minijinja::{Environment, Error, UndefinedBehavior, Value};
use serde::de::DeserializeOwned;

use crate::entities::{
    agent::{OutfitColors, OutfitId},
    items::ItemId,
    position::Position,
    voices::Voices,
};

pub const GREET_WORDS: [&str; 2] = ["hi", "hello"];
pub const GOODBYE_WORDS: [&str; 3] = ["bye", "goodbye", "farewell"];
/// Unsayable because `handle_say` refuses control characters.
pub const WALK_AWAY_WORD: &str = "\u{1}walk_away";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlotKind {
    Int,
}

impl SlotKind {
    fn capture(self, said: &str) -> Option<Value> {
        match self {
            SlotKind::Int => said.parse::<u64>().ok().map(Value::from),
        }
    }
}

#[derive(Debug)]
enum WordToken {
    Literal(String),
    Slot { name: String, kind: SlotKind },
}

#[derive(Debug)]
pub struct Word {
    text: String,
    tokens: Vec<WordToken>,
}

#[derive(Debug)]
pub struct WordMatch {
    /// The word as listed in the file, not the text the player said.
    pub word: String,
    pub captures: Vec<(String, Value)>,
}

#[derive(Debug)]
pub struct Branch {
    pub word: String,
    pub keyword: Arc<Keyword>,
    pub vars: Vec<(String, Value)>,
}

impl Word {
    pub fn parse(text: String) -> Result<Self, &'static str> {
        let mut tokens = Vec::new();
        for token in text.split_whitespace() {
            let Some(slot) = token.strip_prefix('<') else {
                tokens.push(WordToken::Literal(token.to_string()));
                continue;
            };
            let (name, kind) = slot
                .strip_suffix('>')
                .and_then(|slot| slot.split_once(':'))
                .ok_or("has a slot that is not written <name:type>")?;
            if !is_identifier(name) {
                return Err("has a slot whose name is not an identifier");
            }
            let kind = match kind {
                "int" => SlotKind::Int,
                _ => return Err("has a slot of an unknown type"),
            };
            if tokens
                .iter()
                .any(|token| matches!(token, WordToken::Slot { name: other, .. } if other == name))
            {
                return Err("names the same slot twice in one word");
            }
            tokens.push(WordToken::Slot {
                name: name.to_string(),
                kind,
            });
        }
        if tokens.is_empty() {
            return Err("has an empty word");
        }
        Ok(Self { text, tokens })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn slots(&self) -> impl Iterator<Item = (&str, SlotKind)> {
        self.tokens.iter().filter_map(|token| match token {
            WordToken::Slot { name, kind } => Some((name.as_str(), *kind)),
            WordToken::Literal(_) => None,
        })
    }

    fn captures(&self, said: &[&str]) -> Option<Vec<(String, Value)>> {
        said.windows(self.tokens.len()).find_map(|window| {
            let mut captures = Vec::new();
            for (token, said) in self.tokens.iter().zip(window) {
                match token {
                    WordToken::Literal(literal) if literal == said => {}
                    WordToken::Literal(_) => return None,
                    WordToken::Slot { name, kind } => {
                        captures.push((name.clone(), kind.capture(said)?));
                    }
                }
            }
            Some(captures)
        })
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn capitalize(text: &str) -> String {
    let mut word_start = true;
    text.chars()
        .flat_map(|c| {
            let upper = word_start;
            word_start = c.is_whitespace();
            if upper {
                c.to_uppercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub fn environment() -> Environment<'static> {
    let mut env = Environment::empty();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_filter("capitalize", capitalize);
    env
}

#[derive(Debug)]
pub enum NpcAction {
    Engage,
    Dismiss,
    Say,
    Teleport {
        cost: String,
        item: Option<String>,
        position: String,
    },
    Deposit {
        amount: String,
    },
    Withdraw {
        amount: String,
    },
}

#[derive(Debug)]
pub enum NpcResponse {
    Say(String),
    Engage(String),
    Dismiss(String),
    Teleport {
        cost: u64,
        item: Option<ItemId>,
        to: Position,
        failed_text: String,
    },
    Deposit {
        amount: u64,
        text: String,
        failed_text: String,
    },
    Withdraw {
        amount: u64,
        text: String,
        failed_text: String,
    },
}

#[derive(Debug)]
enum Reply {
    Text(String),
    Template(String),
}

impl Reply {
    fn register(env: &mut Environment<'static>, text: String) -> Result<Self, Error> {
        if text.contains("{{") || text.contains("{%") {
            let name = env.templates().count().to_string();
            env.add_template_owned(name.clone(), text)?;
            Ok(Reply::Template(name))
        } else {
            Ok(Reply::Text(text))
        }
    }

    fn render(&self, env: &Environment, ctx: &Value) -> Result<String, Error> {
        match self {
            Reply::Text(text) => Ok(text.clone()),
            Reply::Template(name) => env
                .get_template(name)
                .expect("keyword template names are registered in their own NPC's env")
                .render(ctx),
        }
    }
}

#[derive(Debug)]
pub struct Keyword {
    words: Vec<Word>,
    condition: Option<String>,
    set: Vec<(String, String)>,
    action: NpcAction,
    text: Option<Reply>,
    failed_text: Option<Reply>,
    reset: bool,
    children: Vec<Arc<Keyword>>,
}

impl Keyword {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env: &mut Environment<'static>,
        words: Vec<Word>,
        condition: Option<String>,
        set: Vec<(String, String)>,
        action: NpcAction,
        text: Option<String>,
        failed_text: Option<String>,
        reset: bool,
        children: Vec<Arc<Keyword>>,
    ) -> Result<Self, Error> {
        let expressions = match &action {
            NpcAction::Teleport {
                cost,
                item,
                position,
            } => vec![Some(cost), item.as_ref(), Some(position)],
            NpcAction::Deposit { amount } | NpcAction::Withdraw { amount } => vec![Some(amount)],
            _ => vec![],
        };
        for expr in expressions
            .into_iter()
            .chain([condition.as_ref()])
            .flatten()
            .chain(set.iter().map(|(_, expr)| expr))
        {
            env.compile_expression_owned(expr.clone())?;
        }

        Ok(Self {
            words,
            condition,
            set,
            action,
            text: text.map(|text| Reply::register(env, text)).transpose()?,
            failed_text: failed_text
                .map(|text| Reply::register(env, text))
                .transpose()?,
            reset,
            children,
        })
    }

    pub fn words(&self) -> &[Word] {
        &self.words
    }

    pub fn resets(&self) -> bool {
        self.reset
    }

    pub fn match_word(&self, message: &str) -> Option<WordMatch> {
        let message: String = message
            .to_lowercase()
            .chars()
            .map(|c| match c {
                '\'' | '_' => c,
                c if c.is_ascii_punctuation() => ' ',
                c => c,
            })
            .collect();
        let said: Vec<&str> = message.split_whitespace().collect();
        self.words.iter().find_map(|word| {
            word.captures(&said).map(|captures| WordMatch {
                word: word.text.clone(),
                captures,
            })
        })
    }

    pub fn allows(&self, env: &Environment, ctx: &Value) -> Result<bool, Error> {
        match &self.condition {
            Some(condition) => Ok(env.compile_expression(condition)?.eval(ctx)?.is_true()),
            None => Ok(true),
        }
    }

    pub fn bindings(&self, env: &Environment, ctx: &Value) -> Result<Vec<(String, Value)>, Error> {
        self.set
            .iter()
            .map(|(name, expr)| Ok((name.clone(), env.compile_expression(expr)?.eval(ctx)?)))
            .collect()
    }

    pub fn response(&self, env: &Environment, ctx: Value) -> Result<NpcResponse, Error> {
        match &self.action {
            NpcAction::Engage => self.text(env, &ctx).map(NpcResponse::Engage),
            NpcAction::Dismiss => self.text(env, &ctx).map(NpcResponse::Dismiss),
            NpcAction::Say => self.text(env, &ctx).map(NpcResponse::Say),
            NpcAction::Teleport {
                cost,
                item,
                position,
            } => Ok(NpcResponse::Teleport {
                cost: self.eval(cost, env, &ctx)?,
                item: item
                    .as_ref()
                    .map(|item| self.eval(item, env, &ctx))
                    .transpose()?,
                to: self.eval(position, env, &ctx)?,
                failed_text: self.failed_text(env, &ctx)?,
            }),
            NpcAction::Deposit { amount } => Ok(NpcResponse::Deposit {
                amount: self.eval(amount, env, &ctx)?,
                text: self.text(env, &ctx)?,
                failed_text: self.failed_text(env, &ctx)?,
            }),
            NpcAction::Withdraw { amount } => Ok(NpcResponse::Withdraw {
                amount: self.eval(amount, env, &ctx)?,
                text: self.text(env, &ctx)?,
                failed_text: self.failed_text(env, &ctx)?,
            }),
        }
    }

    pub fn children(&self) -> &[Arc<Keyword>] {
        &self.children
    }

    fn text(&self, env: &Environment, ctx: &Value) -> Result<String, Error> {
        self.text
            .as_ref()
            .expect("every keyword but a teleport is loaded with a text")
            .render(env, ctx)
    }

    fn failed_text(&self, env: &Environment, ctx: &Value) -> Result<String, Error> {
        self.failed_text
            .as_ref()
            .expect("teleport, deposit and withdraw keywords are loaded with a failed_text")
            .render(env, ctx)
    }

    fn eval<T: DeserializeOwned>(
        &self,
        expr: &str,
        env: &Environment,
        ctx: &Value,
    ) -> Result<T, Error> {
        T::deserialize(env.compile_expression(expr)?.eval(ctx)?)
    }
}

#[derive(Debug)]
pub struct Npc {
    pub name: String,
    pub outfit: (OutfitId, OutfitColors),
    pub walk_range: u16,
    pub talk_range: u16,
    pub origin: Position,
    pub say: Voices,
    pub speed: u16,
    pub keywords: Vec<Arc<Keyword>>,
    pub arguments: BTreeMap<String, Value>,
    pub environment: Environment<'static>,
}

#[cfg(test)]
mod tests {
    use minijinja::context;

    use super::*;

    fn parse_words(words: &[&str]) -> Vec<Word> {
        words
            .iter()
            .map(|word| Word::parse(word.to_string()).unwrap())
            .collect()
    }

    fn a_keyword(env: &mut Environment<'static>, action: NpcAction, text: Option<&str>) -> Keyword {
        Keyword::new(
            env,
            parse_words(&["hi"]),
            None,
            vec![],
            action,
            text.map(str::to_string),
            None,
            false,
            vec![],
        )
        .unwrap()
    }

    fn answering(words: &[&str]) -> Keyword {
        Keyword::new(
            &mut environment(),
            parse_words(words),
            None,
            vec![],
            NpcAction::Say,
            Some("Hm?".to_string()),
            None,
            false,
            vec![],
        )
        .unwrap()
    }

    fn matched(keyword: &Keyword, message: &str) -> Option<String> {
        keyword.match_word(message).map(|found| found.word)
    }

    #[test]
    fn a_word_of_several_parts_matches_them_in_a_row() {
        let keyword = answering(&["liberty bay"]);

        assert_eq!(
            matched(&keyword, "To Liberty Bay, please"),
            Some("liberty bay".to_string())
        );
        assert_eq!(matched(&keyword, "liberty or bay"), None);
        assert_eq!(matched(&keyword, "liberty"), None);
    }

    #[test]
    fn punctuation_around_a_word_does_not_stop_it_matching() {
        let keyword = answering(&["edron", "ab'dendriel"]);

        assert_eq!(matched(&keyword, "edron?"), Some("edron".to_string()));
        assert_eq!(matched(&keyword, "thais,edron"), Some("edron".to_string()));
        assert_eq!(
            matched(&keyword, "Ab'Dendriel!"),
            Some("ab'dendriel".to_string())
        );
    }

    #[test]
    fn a_word_matches_only_whole() {
        assert_eq!(matched(&answering(&["yes"]), "yesterday"), None);
    }

    #[test]
    fn an_int_slot_captures_the_number_said_in_its_place() {
        let found = answering(&["deposit <amount:int>"])
            .match_word("I want to deposit 100, please")
            .unwrap();

        assert_eq!(found.word, "deposit <amount:int>");
        assert_eq!(
            found.captures,
            vec![("amount".to_string(), Value::from(100_u64))]
        );
    }

    #[test]
    fn a_word_that_is_only_a_slot_matches_a_bare_number() {
        let found = answering(&["<amount:int>"]).match_word("250").unwrap();

        assert_eq!(
            found.captures,
            vec![("amount".to_string(), Value::from(250_u64))]
        );
    }

    #[test]
    fn an_int_slot_does_not_match_a_word_or_a_number_too_large() {
        let keyword = answering(&["deposit <amount:int>"]);

        assert_eq!(matched(&keyword, "deposit gold"), None);
        assert_eq!(matched(&keyword, "deposit 99999999999999999999"), None);
    }

    #[test]
    fn each_listed_word_is_tried_until_one_matches() {
        let keyword = answering(&["deposit all", "deposit <amount:int>"]);

        let found = keyword.match_word("deposit 5").unwrap();

        assert_eq!(found.word, "deposit <amount:int>");
        assert_eq!(
            matched(&keyword, "deposit all"),
            Some("deposit all".to_string())
        );
    }

    #[test]
    fn a_malformed_slot_is_refused() {
        assert!(Word::parse("deposit <amount>".to_string()).is_err());
        assert!(Word::parse("deposit <amount:float>".to_string()).is_err());
        assert!(Word::parse("deposit <1st:int>".to_string()).is_err());
        assert!(Word::parse("<a:int> <a:int>".to_string()).is_err());
        assert!(Word::parse("deposit <amount:int".to_string()).is_err());
        assert!(Word::parse("   ".to_string()).is_err());
    }

    #[test]
    fn a_plain_text_comes_back_verbatim_and_a_template_is_rendered() {
        let mut env = environment();
        let plain = a_keyword(&mut env, NpcAction::Dismiss, Some("Good bye."));
        let template = a_keyword(
            &mut env,
            NpcAction::Engage,
            Some("Welcome, {{ PLAYERNAME }}."),
        );
        let ctx = context! { PLAYERNAME => "Rizael" };

        assert!(matches!(
            plain.response(&env, ctx.clone()).unwrap(),
            NpcResponse::Dismiss(text) if text == "Good bye."
        ));
        assert!(matches!(
            template.response(&env, ctx).unwrap(),
            NpcResponse::Engage(text) if text == "Welcome, Rizael."
        ));
    }

    #[test]
    fn capitalize_uppercases_the_first_letter_of_each_word() {
        let mut env = environment();
        let template = a_keyword(
            &mut env,
            NpcAction::Engage,
            Some("Welcome, {{ PLAYERNAME | capitalize }}."),
        );

        assert!(matches!(
            template.response(&env, context! { PLAYERNAME => "sir mcDoe" }).unwrap(),
            NpcResponse::Engage(text) if text == "Welcome, Sir McDoe."
        ));
    }

    #[test]
    fn an_undefined_variable_is_an_error_not_an_empty_string() {
        let mut env = environment();
        let template = a_keyword(
            &mut env,
            NpcAction::Engage,
            Some("Welcome, {{ PLAYERNAME }}."),
        );

        assert!(template.response(&env, context! {}).is_err());
    }

    #[test]
    fn a_teleport_whose_expression_does_not_compile_is_refused() {
        let mut env = environment();
        let teleport = NpcAction::Teleport {
            cost: "DESTINATIONS[".to_string(),
            item: None,
            position: "DESTINATIONS.edron".to_string(),
        };

        assert!(
            Keyword::new(
                &mut env,
                parse_words(&["yes"]),
                None,
                vec![],
                teleport,
                None,
                Some("No.".to_string()),
                false,
                vec![]
            )
            .is_err()
        );
    }
}

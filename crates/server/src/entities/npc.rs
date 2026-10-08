use std::{collections::BTreeMap, sync::Arc};

use minijinja::{Environment, Error, UndefinedBehavior, Value};
use serde::de::DeserializeOwned;

use crate::entities::{
    agent::{OutfitColors, OutfitId},
    position::Position,
    voices::Voices,
};

pub const GREET_WORDS: [&str; 2] = ["hi", "hello"];
pub const GOODBYE_WORDS: [&str; 3] = ["bye", "goodbye", "farewell"];
/// Unsayable because `handle_say` refuses control characters.
pub const WALK_AWAY_WORD: &str = "\u{1}walk_away";

pub fn environment() -> Environment<'static> {
    let mut env = Environment::empty();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env
}

#[derive(Debug)]
pub enum NpcAction {
    Engage,
    Dismiss,
    Say,
    Teleport { cost: String, position: String },
}

#[derive(Debug)]
pub enum NpcResponse {
    Say(String),
    Engage(String),
    Dismiss(String),
    Teleport {
        cost: u64,
        to: Position,
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

    fn render(&self, env: &Environment, ctx: Value) -> Result<String, Error> {
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
    words: Vec<String>,
    action: NpcAction,
    text: Option<Reply>,
    failed_text: Option<Reply>,
    reset: bool,
    children: Vec<Arc<Keyword>>,
}

impl Keyword {
    pub fn new(
        env: &mut Environment<'static>,
        words: Vec<String>,
        action: NpcAction,
        text: Option<String>,
        failed_text: Option<String>,
        reset: bool,
        children: Vec<Arc<Keyword>>,
    ) -> Result<Self, Error> {
        if let NpcAction::Teleport { cost, position } = &action {
            env.compile_expression_owned(cost.clone())?;
            env.compile_expression_owned(position.clone())?;
        }

        Ok(Self {
            words,
            action,
            text: text.map(|text| Reply::register(env, text)).transpose()?,
            failed_text: failed_text
                .map(|text| Reply::register(env, text))
                .transpose()?,
            reset,
            children,
        })
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    pub fn resets(&self) -> bool {
        self.reset
    }

    pub fn match_word(&self, message: &str) -> Option<String> {
        message
            .to_lowercase()
            .split(" ")
            .find(|part| self.words.iter().any(|word| part == word))
            .map(|word| word.to_owned())
    }

    pub fn response(&self, env: &Environment, ctx: Value) -> Result<NpcResponse, Error> {
        match &self.action {
            NpcAction::Engage => self.text(env, ctx).map(NpcResponse::Engage),
            NpcAction::Dismiss => self.text(env, ctx).map(NpcResponse::Dismiss),
            NpcAction::Say => self.text(env, ctx).map(NpcResponse::Say),
            NpcAction::Teleport { cost, position } => Ok(NpcResponse::Teleport {
                cost: self.eval(cost, env, &ctx)?,
                to: self.eval(position, env, &ctx)?,
                failed_text: self
                    .failed_text
                    .as_ref()
                    .expect("teleport keywords are loaded with a failed_text")
                    .render(env, ctx)?,
            }),
        }
    }

    pub fn children(&self) -> &[Arc<Keyword>] {
        &self.children
    }

    fn text(&self, env: &Environment, ctx: Value) -> Result<String, Error> {
        self.text
            .as_ref()
            .expect("say, engage and dismiss keywords are loaded with a text")
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

    fn a_keyword(env: &mut Environment<'static>, action: NpcAction, text: Option<&str>) -> Keyword {
        Keyword::new(
            env,
            vec!["hi".to_string()],
            action,
            text.map(str::to_string),
            None,
            false,
            vec![],
        )
        .unwrap()
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
            position: "DESTINATIONS.edron".to_string(),
        };

        assert!(
            Keyword::new(
                &mut env,
                vec!["yes".to_string()],
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

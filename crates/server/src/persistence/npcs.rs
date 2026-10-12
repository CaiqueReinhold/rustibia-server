use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::{Environment, Value};
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;

use crate::config::CONFIG;
use crate::entities::agent::{OutfitColors, OutfitId};
use crate::entities::items::{FluidType, ItemConfig, ItemFlag, ItemId};
use crate::entities::npc::{
    self, GOODBYE_WORDS, GREET_WORDS, Keyword, Npc, NpcAction, ShopOffer, SlotKind, WALK_AWAY_WORD,
    Word,
};
use crate::entities::position::Position;
use crate::entities::voices::Voices;
use crate::game::TickDelta;
use crate::persistence::items::ITEM_CONFIGS;
use crate::persistence::yaml_files_in;

pub type NpcId = String;

pub static NPCS: Lazy<Arc<HashMap<NpcId, Arc<Npc>>>> = Lazy::new(|| {
    Arc::new(load_npcs(&CONFIG.npcs_dir_path, &ITEM_CONFIGS).expect("failed to load npcs"))
});

#[derive(Error, Debug)]
pub enum NpcsLoadError {
    #[error("I/O error reading {path}: {source}")]
    ReadError {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("YAML parse error in {path}: {source}")]
    ParseError {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("{path} has no file stem to take an NPC id from")]
    UnnamedFile { path: PathBuf },
    #[error("{path} has `words: {name}`, which is not GREET, GOODBYE or WALK_AWAY")]
    UnknownTrigger { path: PathBuf, name: String },
    #[error("{path}: keyword `{keyword}` {reason}")]
    BadKeyword {
        path: PathBuf,
        keyword: String,
        reason: &'static str,
    },
    #[error("{path}: keyword `{keyword}` does not compile: {source}")]
    Template {
        path: PathBuf,
        keyword: String,
        source: minijinja::Error,
    },
    #[error("{path} has an argument named `{name}`, which the game supplies")]
    ReservedArgument { path: PathBuf, name: String },
    #[error("{path}: shop offer {id} {reason}")]
    BadOffer {
        path: PathBuf,
        id: u16,
        reason: &'static str,
    },
    #[error("{path} {reason}")]
    BadShop { path: PathBuf, reason: &'static str },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutfit {
    id: OutfitId,
    colors: OutfitColors,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVoices {
    cooldown: TickDelta,
    chance: u32,
    #[serde(default)]
    sentences: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawWords {
    Trigger(String),
    Words(Vec<String>),
}

impl RawWords {
    fn label(&self) -> String {
        match self {
            RawWords::Trigger(name) => name.clone(),
            RawWords::Words(words) => words.join(", "),
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RawAction {
    Say,
    Engage,
    Dismiss,
    Teleport,
    Deposit,
    Withdraw,
    Trade,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyword {
    words: RawWords,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    set: BTreeMap<String, String>,
    action: RawAction,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    failed_text: Option<String>,
    #[serde(default)]
    reset: bool,
    #[serde(default)]
    params: Option<BTreeMap<String, String>>,
    #[serde(default)]
    children: Vec<RawKeyword>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNpc {
    name: String,
    position: Position,
    speed: u16,
    outfit: RawOutfit,
    walk_range: u16,
    talk_range: u16,
    say: RawVoices,
    #[serde(default)]
    arguments: BTreeMap<String, Value>,
    #[serde(default)]
    shop: Vec<RawOffer>,
    keywords: Vec<RawKeyword>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOffer {
    id: ItemId,
    #[serde(default)]
    buy: Option<u32>,
    #[serde(default)]
    sell: Option<u32>,
    #[serde(default)]
    fluid: Option<String>,
}

fn convert_shop(
    path: &Path,
    raw: Vec<RawOffer>,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) -> Result<Vec<ShopOffer>, NpcsLoadError> {
    let mut offers: Vec<ShopOffer> = Vec::with_capacity(raw.len());
    for offer in raw {
        let bad = |reason| NpcsLoadError::BadOffer {
            path: path.to_path_buf(),
            id: offer.id.0,
            reason,
        };
        if offer.buy.is_none() && offer.sell.is_none() {
            return Err(bad("has neither a buy nor a sell price"));
        }
        let config = items
            .get(&offer.id)
            .ok_or_else(|| bad("is not in the item catalogue"))?;
        let fluid = match (&offer.fluid, config.has_flag(ItemFlag::LiquidContainer)) {
            (None, false) => None,
            (None, true) => Some(FluidType::None),
            (Some(name), true) => {
                Some(FluidType::from_name(name).ok_or_else(|| bad("names an unknown fluid"))?)
            }
            (Some(_), false) => return Err(bad("names a fluid but is not a liquid container")),
        };
        if offers
            .iter()
            .any(|known| known.id() == offer.id && known.fluid == fluid)
        {
            return Err(bad("is offered twice"));
        }
        offers.push(ShopOffer {
            config: Arc::clone(config),
            fluid,
            buy: offer.buy,
            sell: offer.sell,
        });
    }
    Ok(offers)
}

fn convert_action(
    action: RawAction,
    has_text: bool,
    has_failed_text: bool,
    params: Option<BTreeMap<String, String>>,
) -> Result<NpcAction, &'static str> {
    match action {
        RawAction::Teleport => {
            if has_text {
                return Err("is a teleport, which takes no text");
            }
            if !has_failed_text {
                return Err("is a teleport, which needs a failed_text");
            }
            let mut params = params.unwrap_or_default();
            let (Some(cost), Some(position)) = (params.remove("cost"), params.remove("position"))
            else {
                return Err("is a teleport, which needs a `cost` and a `position` param");
            };
            let item = params.remove("item");
            if !params.is_empty() {
                return Err("has a param a teleport does not take");
            }
            Ok(NpcAction::Teleport {
                cost,
                item,
                position,
            })
        }
        RawAction::Deposit | RawAction::Withdraw => {
            if !has_text {
                return Err("is a deposit or withdraw, which needs a text");
            }
            if !has_failed_text {
                return Err("is a deposit or withdraw, which needs a failed_text");
            }
            let mut params = params.unwrap_or_default();
            let Some(amount) = params.remove("amount") else {
                return Err("is a deposit or withdraw, which needs an `amount` param");
            };
            if !params.is_empty() {
                return Err("has a param a deposit or withdraw does not take");
            }
            Ok(match action {
                RawAction::Deposit => NpcAction::Deposit { amount },
                _ => NpcAction::Withdraw { amount },
            })
        }
        _ if !has_text => Err("needs a text"),
        _ if has_failed_text => {
            Err("takes no failed_text, which only a teleport, deposit or withdraw has")
        }
        _ if params.is_some() => Err("takes no params"),
        RawAction::Say => Ok(NpcAction::Say),
        RawAction::Engage => Ok(NpcAction::Engage),
        RawAction::Dismiss => Ok(NpcAction::Dismiss),
        RawAction::Trade => Ok(NpcAction::Trade),
    }
}

fn check_words(words: &[String]) -> Result<(), &'static str> {
    if words.is_empty() {
        return Err("has no words");
    }
    for word in words {
        if word.is_empty() {
            return Err("has an empty word");
        }
        if word.chars().any(char::is_control) {
            return Err("has a word with a control character");
        }
        if *word != word.to_lowercase() {
            return Err("has a word that is not lowercase");
        }
    }
    Ok(())
}

fn convert_keywords(
    path: &Path,
    env: &mut Environment<'static>,
    arguments: &BTreeMap<String, Value>,
    raw_keywords: Vec<RawKeyword>,
    parent: Option<&str>,
) -> Result<Vec<Arc<Keyword>>, NpcsLoadError> {
    let mut keywords = Vec::with_capacity(raw_keywords.len());
    let mut siblings = HashSet::new();
    for raw in raw_keywords {
        let label = match parent {
            Some(parent) => format!("{parent} > {}", raw.words.label()),
            None => raw.words.label(),
        };
        let bad = |reason| NpcsLoadError::BadKeyword {
            path: path.to_path_buf(),
            keyword: label.clone(),
            reason,
        };

        let words = match raw.words {
            RawWords::Trigger(name) => match name.as_str() {
                "GREET" => GREET_WORDS.map(String::from).to_vec(),
                "GOODBYE" => GOODBYE_WORDS.map(String::from).to_vec(),
                "WALK_AWAY" => vec![WALK_AWAY_WORD.to_string()],
                _ => {
                    return Err(NpcsLoadError::UnknownTrigger {
                        path: path.to_path_buf(),
                        name,
                    });
                }
            },
            RawWords::Words(words) => {
                check_words(&words).map_err(bad)?;
                words
            }
        };
        let words = words
            .into_iter()
            .map(Word::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(bad)?;
        if words.iter().any(|word| siblings.contains(word.text())) {
            return Err(bad(
                "answers a word an earlier keyword without a condition at its level already answers",
            ));
        }
        if raw.condition.is_none() {
            siblings.extend(words.iter().map(|word| word.text().to_string()));
        }
        let slots: BTreeMap<String, SlotKind> = words[0]
            .slots()
            .map(|(name, kind)| (name.to_string(), kind))
            .collect();
        if words.iter().any(|word| {
            word.slots()
                .map(|(name, kind)| (name.to_string(), kind))
                .collect::<BTreeMap<_, _>>()
                != slots
        }) {
            return Err(bad("has words that declare different slots"));
        }
        if slots.keys().any(|name| is_reserved(name, arguments)) {
            return Err(bad("has a slot named like a game variable or an argument"));
        }
        if raw
            .set
            .keys()
            .any(|name| is_reserved(name, arguments) || slots.contains_key(name))
        {
            return Err(bad(
                "sets a variable named like a game variable, an argument or one of its slots",
            ));
        }

        if raw.reset && !raw.children.is_empty() {
            return Err(bad("resets the branch, so it cannot have children"));
        }
        let action = convert_action(
            raw.action,
            raw.text.is_some(),
            raw.failed_text.is_some(),
            raw.params,
        )
        .map_err(bad)?;
        let children = convert_keywords(path, env, arguments, raw.children, Some(&label))?;
        let keyword = Keyword::new(
            env,
            words,
            raw.condition,
            raw.set.into_iter().collect(),
            action,
            raw.text,
            raw.failed_text,
            raw.reset,
            children,
        )
        .map_err(|source| NpcsLoadError::Template {
            path: path.to_path_buf(),
            keyword: label.clone(),
            source,
        })?;
        keywords.push(Arc::new(keyword));
    }
    Ok(keywords)
}

const GAME_VARIABLES: [&str; 7] = [
    "PLAYERNAME",
    "WORD",
    "PARENT_KEYWORD",
    "GOLD",
    "BALANCE",
    "uniform",
    "has_item",
];

fn is_reserved(name: &str, arguments: &BTreeMap<String, Value>) -> bool {
    GAME_VARIABLES.contains(&name) || arguments.contains_key(name)
}

pub(crate) fn parse_npc(
    path: &Path,
    contents: &str,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) -> Result<Npc, NpcsLoadError> {
    let raw: RawNpc =
        serde_yaml::from_str(contents).map_err(|source| NpcsLoadError::ParseError {
            path: path.to_path_buf(),
            source,
        })?;

    if let Some(name) = raw
        .arguments
        .keys()
        .find(|name| GAME_VARIABLES.contains(&name.as_str()))
    {
        return Err(NpcsLoadError::ReservedArgument {
            path: path.to_path_buf(),
            name: name.clone(),
        });
    }

    let mut environment = npc::environment();
    let keywords = convert_keywords(path, &mut environment, &raw.arguments, raw.keywords, None)?;
    let shop = convert_shop(path, raw.shop, items)?;
    let opens_shop = keywords.iter().any(|keyword| keyword.opens_shop());
    let bad = |reason| NpcsLoadError::BadShop {
        path: path.to_path_buf(),
        reason,
    };
    if opens_shop && shop.is_empty() {
        return Err(bad("has a trade keyword but no shop"));
    }
    if !opens_shop && !shop.is_empty() {
        return Err(bad("has a shop that no trade keyword opens"));
    }

    Ok(Npc {
        name: raw.name,
        outfit: (raw.outfit.id, raw.outfit.colors),
        walk_range: raw.walk_range,
        talk_range: raw.talk_range,
        origin: raw.position,
        say: Voices {
            cooldown: raw.say.cooldown,
            chance: raw.say.chance,
            sentences: raw.say.sentences,
        },
        speed: raw.speed,
        keywords,
        arguments: raw.arguments,
        environment,
        shop,
    })
}

/// Each `.yaml` file in `dir` is one NPC, and its file stem is the `NpcId`.
pub fn load_npcs(
    dir: impl AsRef<Path>,
    items: &HashMap<ItemId, Arc<ItemConfig>>,
) -> Result<HashMap<NpcId, Arc<Npc>>, NpcsLoadError> {
    let dir = dir.as_ref();
    let paths = yaml_files_in(dir).map_err(|source| NpcsLoadError::ReadError {
        path: dir.to_path_buf(),
        source,
    })?;

    paths
        .into_iter()
        .map(|path| {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| NpcsLoadError::UnnamedFile { path: path.clone() })?
                .to_string();
            let contents =
                fs::read_to_string(&path).map_err(|source| NpcsLoadError::ReadError {
                    path: path.clone(),
                    source,
                })?;
            Ok((id, Arc::new(parse_npc(&path, &contents, items)?)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use minijinja::Value;

    use super::*;
    use crate::entities::npc::{GOODBYE_WORDS, GREET_WORDS, NpcResponse, WALK_AWAY_WORD};
    use crate::entities::position::Position;

    const AN_NPC: &str = r#"
name: Captain Jack
position: { x: 100, y: 200, z: 7 }
speed: 100
outfit: { id: 314, colors: [0, 0, 0, 0] }
walk_range: 3
talk_range: 3
say: { cooldown: 300, chance: 50000, sentences: ["Passages!"] }
arguments:
  DESTINATIONS:
    edron: { cost: 40, position: { x: 33173, y: 31764, z: 6 } }
keywords:
  - words: GREET
    action: engage
    text: "Welcome on board, {{ PLAYERNAME }}."
  - words: GOODBYE
    action: dismiss
    text: Good bye.
  - words: WALK_AWAY
    action: dismiss
    text: Good bye then.
  - words: ["edron"]
    action: say
    text: "A passage to {{ WORD }} for {{ DESTINATIONS[WORD].cost }}?"
    children:
      - words: ["yes"]
        action: teleport
        failed_text: "No gold, {{ PLAYERNAME }}."
        params:
          cost: DESTINATIONS[PARENT_KEYWORD].cost
          position: DESTINATIONS[PARENT_KEYWORD].position
      - words: ["no"]
        action: say
        reset: true
        text: Maybe later.
  - words: ["yes"]
    action: say
    text: Yes what?
"#;

    fn parse(document: &str) -> Result<Npc, NpcsLoadError> {
        parse_npc(Path::new("test.yaml"), document, &a_catalogue())
    }

    fn a_catalogue() -> HashMap<ItemId, Arc<ItemConfig>> {
        let torch = ItemConfig::new(ItemId(9001), "torch".to_string(), None, None, [], []);
        let vial = ItemConfig::new(
            ItemId(9003),
            "vial".to_string(),
            None,
            None,
            [ItemFlag::LiquidContainer],
            [],
        );
        HashMap::from([
            (ItemId(9001), Arc::new(torch)),
            (ItemId(9003), Arc::new(vial)),
        ])
    }

    fn with_shop(shop: &str, keywords: &str) -> String {
        with_keywords(keywords).replace("keywords:\n", &format!("shop:\n{shop}keywords:\n"))
    }

    const TRADE: &str = "  - words: [\"trade\"]\n    action: trade\n    text: Look.\n";

    fn bad_shop(document: &str) -> &'static str {
        match parse(document) {
            Err(NpcsLoadError::BadShop { reason, .. }) => reason,
            other => panic!("expected BadShop, got {other:?}"),
        }
    }

    fn answering<'a>(
        keywords: impl IntoIterator<Item = &'a Arc<Keyword>>,
        word: &str,
    ) -> &'a Keyword {
        keywords
            .into_iter()
            .find(|keyword| keyword.words().iter().any(|w| w.text() == word))
            .unwrap_or_else(|| panic!("no keyword answers {word:?}"))
    }

    fn bad_keyword(document: &str) -> (String, &'static str) {
        match parse(document) {
            Err(NpcsLoadError::BadKeyword {
                keyword, reason, ..
            }) => (keyword, reason),
            other => panic!("expected BadKeyword, got {other:?}"),
        }
    }

    fn with_keywords(keywords: &str) -> String {
        format!(
            "name: N\nposition: {{ x: 1, y: 1, z: 7 }}\nspeed: 100\n\
             outfit: {{ id: 1, colors: [0, 0, 0, 0] }}\nwalk_range: 1\ntalk_range: 1\n\
             say: {{ cooldown: 1, chance: 1 }}\nkeywords:\n{keywords}"
        )
    }

    #[test]
    fn an_npc_loads_with_its_position_as_origin() {
        let npc = parse(AN_NPC).unwrap();

        assert_eq!(npc.name, "Captain Jack");
        assert_eq!(npc.origin, Position::new(100, 200, 7));
        assert_eq!(npc.speed, 100);
        assert_eq!(npc.keywords.len(), 5);
    }

    fn texts(keyword: &Keyword) -> Vec<&str> {
        keyword.words().iter().map(Word::text).collect()
    }

    #[test]
    fn the_triggers_expand_to_their_word_lists() {
        let npc = parse(AN_NPC).unwrap();

        assert_eq!(texts(answering(&npc.keywords, "hi")), GREET_WORDS);
        assert_eq!(texts(answering(&npc.keywords, "bye")), GOODBYE_WORDS);
        assert_eq!(
            texts(answering(&npc.keywords, WALK_AWAY_WORD)),
            [WALK_AWAY_WORD]
        );
    }

    #[test]
    fn a_word_may_repeat_at_different_levels() {
        let npc = parse(AN_NPC).unwrap();
        let edron = answering(&npc.keywords, "edron");

        answering(&npc.keywords, "yes");
        answering(edron.children(), "yes");
    }

    #[test]
    fn a_teleport_evaluates_to_a_cost_and_a_position() {
        let npc = parse(AN_NPC).unwrap();
        let yes = answering(answering(&npc.keywords, "edron").children(), "yes");
        let mut ctx = npc.arguments.clone();
        ctx.insert("PARENT_KEYWORD".to_string(), Value::from("edron"));
        ctx.insert("PLAYERNAME".to_string(), Value::from("Rizael"));

        match yes
            .response(&npc.environment, Value::from_serialize(&ctx))
            .unwrap()
        {
            NpcResponse::Teleport {
                cost,
                item,
                to,
                failed_text,
            } => {
                assert_eq!(cost, 40);
                assert_eq!(item, None);
                assert_eq!(to, Position::new(33173, 31764, 6));
                assert_eq!(failed_text, "No gold, Rizael.");
            }
            other => panic!("expected a teleport, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_trigger_is_refused() {
        let document = with_keywords("  - words: GREETINGS\n    action: engage\n    text: Hi.\n");

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::UnknownTrigger { name, .. }) if name == "GREETINGS"
        ));
    }

    #[test]
    fn a_teleport_without_a_position_is_refused() {
        let document = with_keywords(
            "  - words: [\"yes\"]\n    action: teleport\n    failed_text: No.\n    params: { cost: \"40\" }\n",
        );

        assert_eq!(bad_keyword(&document).0, "yes");
    }

    #[test]
    fn a_template_that_does_not_compile_is_refused_naming_its_keyword() {
        let document = with_keywords(
            "  - words: [\"edron\", \"thais\"]\n    action: say\n    text: \"{{ WORD \"\n",
        );

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::Template { keyword, .. }) if keyword == "edron, thais"
        ));
    }

    #[test]
    fn an_expression_that_does_not_compile_is_refused_naming_its_path() {
        let document = with_keywords(concat!(
            "  - words: [\"edron\"]\n    action: say\n    text: Edron?\n    children:\n",
            "      - words: [\"yes\"]\n        action: teleport\n        failed_text: No.\n",
            "        params: { cost: \"A[\", position: \"B\" }\n",
        ));

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::Template { keyword, .. }) if keyword == "edron > yes"
        ));
    }

    #[test]
    fn a_say_without_a_text_is_refused() {
        let document = with_keywords("  - words: [\"sail\"]\n    action: say\n");

        assert_eq!(bad_keyword(&document).0, "sail");
    }

    #[test]
    fn a_teleport_with_a_text_is_refused() {
        let document = with_keywords(concat!(
            "  - words: [\"yes\"]\n    action: teleport\n    text: Off you go.\n    failed_text: No.\n",
            "    params: { cost: \"40\", position: \"P\" }\n",
        ));

        assert_eq!(bad_keyword(&document).0, "yes");
    }

    #[test]
    fn a_teleport_with_an_extra_param_is_refused() {
        let document = with_keywords(concat!(
            "  - words: [\"yes\"]\n    action: teleport\n    failed_text: No.\n",
            "    params: { cost: \"40\", position: \"P\", effect: \"1\" }\n",
        ));

        assert_eq!(bad_keyword(&document).0, "yes");
    }

    #[test]
    fn params_on_a_say_are_refused() {
        let document = with_keywords(
            "  - words: [\"sail\"]\n    action: say\n    text: Where?\n    params: { cost: \"1\" }\n",
        );

        assert_eq!(bad_keyword(&document).0, "sail");
    }

    #[test]
    fn an_empty_word_list_is_refused() {
        let document = with_keywords("  - words: []\n    action: say\n    text: Hm?\n");

        bad_keyword(&document);
    }

    #[test]
    fn an_empty_word_is_refused() {
        let document =
            with_keywords("  - words: [\"sail\", \"\"]\n    action: say\n    text: Hm?\n");

        assert_eq!(bad_keyword(&document).0, "sail, ");
    }

    #[test]
    fn an_uppercase_word_is_refused() {
        let document = with_keywords("  - words: [\"Sail\"]\n    action: say\n    text: Hm?\n");

        assert_eq!(bad_keyword(&document).0, "Sail");
    }

    #[test]
    fn a_word_with_a_control_character_is_refused() {
        let document =
            with_keywords("  - words: [\"\\u0001walk_away\"]\n    action: say\n    text: Hm?\n");

        bad_keyword(&document);
    }

    #[test]
    fn a_word_may_repeat_after_a_sibling_with_a_condition() {
        let document = with_keywords(concat!(
            "  - words: [\"sail\"]\n    condition: \"true\"\n    action: say\n    text: A\n",
            "  - words: [\"sail\"]\n    action: say\n    text: B\n",
        ));

        parse(&document).unwrap();
    }

    #[test]
    fn a_condition_that_does_not_compile_is_refused() {
        let document = with_keywords(
            "  - words: [\"sail\"]\n    condition: \"has_item(\"\n    action: say\n    text: A\n",
        );

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::Template { .. })
        ));
    }

    #[test]
    fn a_word_two_siblings_answer_is_refused() {
        let document = with_keywords(concat!(
            "  - words: [\"sail\", \"travel\"]\n    action: say\n    text: Where?\n",
            "  - words: [\"travel\"]\n    action: say\n    text: Again?\n",
        ));

        assert_eq!(bad_keyword(&document).0, "travel");
    }

    #[test]
    fn a_word_with_whitespace_is_allowed() {
        let document =
            with_keywords("  - words: [\"port hope\"]\n    action: say\n    text: Hm?\n");

        parse(&document).unwrap();
    }

    #[test]
    fn an_argument_named_like_a_game_variable_is_refused() {
        let document = with_keywords("  - words: GREET\n    action: engage\n    text: Hi.\n")
            .replace("keywords:\n", "arguments:\n  WORD: 1\nkeywords:\n");

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::ReservedArgument { name, .. }) if name == "WORD"
        ));
    }

    #[test]
    fn a_teleport_without_a_failed_text_is_refused() {
        let document = with_keywords(
            "  - words: [\"yes\"]\n    action: teleport\n    params: { cost: \"1\", position: \"P\" }\n",
        );

        assert_eq!(bad_keyword(&document).0, "yes");
    }

    #[test]
    fn a_failed_text_on_a_say_is_refused() {
        let document = with_keywords(
            "  - words: [\"sail\"]\n    action: say\n    text: Where?\n    failed_text: No.\n",
        );

        assert_eq!(bad_keyword(&document).0, "sail");
    }

    #[test]
    fn a_reset_keyword_with_children_is_refused() {
        let document = with_keywords(concat!(
            "  - words: [\"edron\"]\n    action: say\n    text: Edron?\n    reset: true\n",
            "    children:\n      - words: [\"yes\"]\n        action: say\n        text: Yes.\n",
        ));

        assert_eq!(bad_keyword(&document).0, "edron");
    }

    #[test]
    fn reset_is_off_unless_set() {
        let npc = parse(AN_NPC).unwrap();
        let edron = answering(&npc.keywords, "edron");

        assert!(!edron.resets());
        assert!(answering(edron.children(), "no").resets());
    }

    #[test]
    fn a_keyword_with_slots_loads() {
        let document = with_keywords(
            "  - words: [\"deposit <amount:int>\", \"<amount:int>\"]\n    action: say\n    text: \"{{ amount }}?\"\n",
        );

        parse(&document).unwrap();
    }

    #[test]
    fn a_malformed_slot_is_refused_naming_its_keyword() {
        let document = with_keywords(
            "  - words: [\"deposit <amount:float>\"]\n    action: say\n    text: Hm?\n",
        );

        assert_eq!(bad_keyword(&document).0, "deposit <amount:float>");
    }

    #[test]
    fn words_that_declare_different_slots_are_refused() {
        let document = with_keywords(
            "  - words: [\"deposit <amount:int>\", \"deposit\"]\n    action: say\n    text: Hm?\n",
        );

        assert_eq!(bad_keyword(&document).0, "deposit <amount:int>, deposit");
    }

    #[test]
    fn a_slot_named_like_a_game_variable_is_refused() {
        let document =
            with_keywords("  - words: [\"roll <uniform:int>\"]\n    action: say\n    text: Hm?\n");

        bad_keyword(&document);
    }

    #[test]
    fn a_slot_named_like_an_argument_is_refused() {
        let document = with_keywords(
            "  - words: [\"deposit <amount:int>\"]\n    action: say\n    text: Hm?\n",
        )
        .replace("keywords:\n", "arguments:\n  amount: 1\nkeywords:\n");

        bad_keyword(&document);
    }

    #[test]
    fn set_evaluates_each_expression_against_the_context() {
        let document = with_keywords(
            "  - words: [\"count <n:int>\"]\n    action: say\n    set: { double: \"n * 2\", same: n }\n    text: Hm?\n",
        );
        let npc = parse(&document).unwrap();
        let keyword = answering(&npc.keywords, "count <n:int>");

        let set = keyword
            .bindings(&npc.environment, &minijinja::context! { n => 3 })
            .unwrap();

        assert_eq!(
            set,
            vec![
                ("double".to_string(), Value::from(6)),
                ("same".to_string(), Value::from(3)),
            ]
        );
    }

    #[test]
    fn a_set_expression_that_does_not_compile_is_refused() {
        let document = with_keywords(
            "  - words: [\"sail\"]\n    action: say\n    set: { x: \"GOLD +\" }\n    text: Hm?\n",
        );

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::Template { keyword, .. }) if keyword == "sail"
        ));
    }

    #[test]
    fn a_set_name_shadowing_a_slot_a_game_variable_or_an_argument_is_refused() {
        for set in ["amount: \"1\"", "WORD: \"1\"", "DESTINATIONS: \"1\""] {
            let document = with_keywords(&format!(
                "  - words: [\"deposit <amount:int>\"]\n    action: say\n    set: {{ {set} }}\n    text: Hm?\n"
            ))
            .replace("keywords:\n", "arguments:\n  DESTINATIONS: 1\nkeywords:\n");

            bad_keyword(&document);
        }
    }

    #[test]
    fn a_deposit_loads_with_a_text_a_failed_text_and_an_amount() {
        let document = with_keywords(
            "  - words: [\"yes\"]\n    action: deposit\n    text: Done.\n    failed_text: No.\n    params: { amount: \"40\" }\n",
        );

        parse(&document).unwrap();
    }

    #[test]
    fn a_deposit_or_withdraw_without_a_failed_text_or_an_amount_is_refused() {
        for action in ["deposit", "withdraw"] {
            let without_failed_text = with_keywords(&format!(
                "  - words: [\"yes\"]\n    action: {action}\n    text: Done.\n    params: {{ amount: \"40\" }}\n"
            ));
            let without_amount = with_keywords(&format!(
                "  - words: [\"yes\"]\n    action: {action}\n    text: Done.\n    failed_text: No.\n"
            ));

            assert_eq!(bad_keyword(&without_failed_text).0, "yes");
            assert_eq!(bad_keyword(&without_amount).0, "yes");
        }
    }

    #[test]
    fn every_shipped_npc_loads() {
        Lazy::force(&NPCS);
    }

    #[test]
    fn a_shop_loads_its_offers_with_their_items() {
        let document = with_shop("  - { id: 9001, buy: 50, sell: 10 }\n", TRADE);

        let npc = parse(&document).unwrap();

        let offer = npc.offer(ItemId(9001), 0).unwrap();
        assert_eq!(offer.config.name, "torch");
        assert_eq!((offer.buy, offer.sell), (Some(50), Some(10)));
    }

    #[test]
    fn a_liquid_container_is_offered_per_fluid_and_empty_without_one() {
        let document = with_shop(
            "  - { id: 9003, sell: 5 }\n  - { id: 9003, fluid: milk, buy: 8 }\n",
            TRADE,
        );

        let npc = parse(&document).unwrap();

        assert_eq!(
            npc.offer(ItemId(9003), 0).map(|offer| offer.fluid),
            Some(Some(FluidType::None))
        );
        assert_eq!(
            npc.offer(ItemId(9003), FluidType::Milk as u8)
                .map(|offer| offer.buy),
            Some(Some(8))
        );
    }

    #[test]
    fn a_misplaced_or_unknown_or_repeated_fluid_is_refused() {
        for shop in [
            "  - { id: 9001, fluid: milk, buy: 5 }\n",
            "  - { id: 9003, fluid: lava, buy: 5 }\n",
            "  - { id: 9003, fluid: milk, buy: 5 }\n  - { id: 9003, fluid: milk, sell: 1 }\n",
        ] {
            assert!(
                matches!(
                    parse(&with_shop(shop, TRADE)),
                    Err(NpcsLoadError::BadOffer { .. })
                ),
                "{shop}"
            );
        }
    }

    #[test]
    fn an_offer_of_an_item_not_in_the_catalogue_is_refused() {
        let document = with_shop("  - { id: 9002, buy: 5 }\n", TRADE);

        assert!(matches!(
            parse(&document),
            Err(NpcsLoadError::BadOffer { id: 9002, .. })
        ));
    }

    #[test]
    fn an_offer_with_neither_price_or_offered_twice_is_refused() {
        for shop in [
            "  - { id: 9001 }\n",
            "  - { id: 9001, buy: 5 }\n  - { id: 9001, sell: 1 }\n",
        ] {
            assert!(matches!(
                parse(&with_shop(shop, TRADE)),
                Err(NpcsLoadError::BadOffer { id: 9001, .. })
            ));
        }
    }

    #[test]
    fn a_trade_keyword_without_a_shop_is_refused() {
        bad_shop(&with_keywords(TRADE));
    }

    #[test]
    fn a_shop_no_keyword_can_open_is_refused() {
        let document = with_shop(
            "  - { id: 9001, buy: 5 }\n",
            "  - words: [\"sail\"]\n    action: say\n    text: Hm?\n",
        );

        bad_shop(&document);
    }

    #[test]
    fn a_trade_keyword_under_another_opens_the_shop() {
        let document = with_shop(
            "  - { id: 9001, buy: 5 }\n",
            concat!(
                "  - words: [\"wares\"]\n    action: say\n    text: Want to see?\n    children:\n",
                "      - words: [\"yes\"]\n        action: trade\n        text: Look.\n",
            ),
        );

        parse(&document).unwrap();
    }

    #[test]
    fn a_trade_with_a_failed_text_or_params_is_refused() {
        for extra in ["    failed_text: No.\n", "    params: { cost: \"1\" }\n"] {
            let document = with_shop("  - { id: 9001, buy: 5 }\n", &format!("{TRADE}{extra}"));

            assert_eq!(bad_keyword(&document).0, "trade");
        }
    }
}

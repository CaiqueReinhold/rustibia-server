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
use crate::entities::npc::{
    self, GOODBYE_WORDS, GREET_WORDS, Keyword, Npc, NpcAction, WALK_AWAY_WORD,
};
use crate::entities::position::Position;
use crate::entities::voices::Voices;
use crate::game::TickDelta;
use crate::persistence::yaml_files_in;

pub type NpcId = String;

pub static NPCS: Lazy<Arc<HashMap<NpcId, Arc<Npc>>>> =
    Lazy::new(|| Arc::new(load_npcs(&CONFIG.npcs_dir_path).expect("failed to load npcs")));

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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyword {
    words: RawWords,
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
    keywords: Vec<RawKeyword>,
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
            if !params.is_empty() {
                return Err("has a param a teleport does not take");
            }
            Ok(NpcAction::Teleport { cost, position })
        }
        _ if !has_text => Err("needs a text"),
        _ if has_failed_text => Err("takes no failed_text, which only a teleport has"),
        _ if params.is_some() => Err("takes no params"),
        RawAction::Say => Ok(NpcAction::Say),
        RawAction::Engage => Ok(NpcAction::Engage),
        RawAction::Dismiss => Ok(NpcAction::Dismiss),
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
        if words.iter().any(|word| siblings.contains(word)) {
            return Err(bad(
                "answers a word an earlier keyword at its level already answers",
            ));
        }
        siblings.extend(words.iter().cloned());

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
        let children = convert_keywords(path, env, raw.children, Some(&label))?;
        let keyword = Keyword::new(
            env,
            words,
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

const GAME_VARIABLES: [&str; 3] = ["PLAYERNAME", "WORD", "PARENT_KEYWORD"];

pub(crate) fn parse_npc(path: &Path, contents: &str) -> Result<Npc, NpcsLoadError> {
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
    let keywords = convert_keywords(path, &mut environment, raw.keywords, None)?;

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
    })
}

/// Each `.yaml` file in `dir` is one NPC, and its file stem is the `NpcId`.
pub fn load_npcs(dir: impl AsRef<Path>) -> Result<HashMap<NpcId, Arc<Npc>>, NpcsLoadError> {
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
            Ok((id, Arc::new(parse_npc(&path, &contents)?)))
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
        parse_npc(Path::new("test.yaml"), document)
    }

    fn answering<'a>(
        keywords: impl IntoIterator<Item = &'a Arc<Keyword>>,
        word: &str,
    ) -> &'a Keyword {
        keywords
            .into_iter()
            .find(|keyword| keyword.words().iter().any(|w| w == word))
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

    #[test]
    fn the_triggers_expand_to_their_word_lists() {
        let npc = parse(AN_NPC).unwrap();

        assert_eq!(answering(&npc.keywords, "hi").words(), GREET_WORDS);
        assert_eq!(answering(&npc.keywords, "bye").words(), GOODBYE_WORDS);
        assert_eq!(
            answering(&npc.keywords, WALK_AWAY_WORD).words(),
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
                to,
                failed_text,
            } => {
                assert_eq!(cost, 40);
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
    fn every_shipped_npc_loads() {
        Lazy::force(&NPCS);
    }
}

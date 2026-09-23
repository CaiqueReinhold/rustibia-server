use std::collections::HashMap;

use anyhow::{Context as _, Result};
use rustibia_server::entities::skills::SkillType;
use rustibia_server::entities::vocation::Vocation;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct KitConfig {
    pub character: CharacterKit,
    pub kit: Inventory,
}

#[derive(Debug, Deserialize)]
pub struct CharacterKit {
    pub vocation: Vocation,
    pub level: i16,
    pub life: i32,
    pub mana: i32,
    pub capacity: i32,
    pub start: Coords,
    pub skills: HashMap<SkillType, i16>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct Coords {
    pub x: i32,
    pub y: i32,
    pub z: i16,
}

#[derive(Debug, Deserialize)]
pub struct Inventory {
    pub backpack: u16,
    pub weapon: u16,
    pub contents: Vec<Stack>,
}

#[derive(Debug, Deserialize)]
pub struct Stack {
    pub item: u16,
    pub stacks: u8,
    pub amount: u8,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Route {
    pub name: String,
    pub waypoints: Vec<Coords>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Behaviour {
    pub engage_radius: u16,
    pub health_potion_pct: u32,
    pub heal_spell_pct: u32,
    pub mana_potion_pct: u32,
    pub attack_spell: String,
    pub heal_spell: String,
    pub loot_items_max: usize,
    pub decision_interval_ms: (u64, u64),
    pub ping_interval_ms: (u64, u64),
}

#[derive(Debug, Deserialize)]
pub struct RunConfig {
    pub routes: Vec<Route>,
    pub behaviour: Behaviour,
}

pub fn load_run_config(path: &str) -> Result<RunConfig> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {path}"))
}

impl Behaviour {
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            engage_radius: 5,
            health_potion_pct: 45,
            heal_spell_pct: 70,
            mana_potion_pct: 35,
            attack_spell: "exori flam".to_string(),
            heal_spell: "exura".to_string(),
            loot_items_max: 4,
            decision_interval_ms: (180, 320),
            ping_interval_ms: (4000, 6000),
        }
    }
}

fn parse(text: &str) -> Result<KitConfig> {
    let kit: KitConfig = serde_yaml::from_str(text)?;

    if kit.character.skills.contains_key(&SkillType::Level) {
        anyhow::bail!("`skills` may not list `level` — it is set from `character.level`");
    }

    Ok(kit)
}

pub fn load_kit(path: &str) -> Result<KitConfig> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    parse(&text).with_context(|| format!("parsing {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A_KIT: &str = r#"
character:
  vocation: sorcerer
  level: 20
  life: 500
  mana: 400
  capacity: 60000
  start: { x: 32097, y: 31103, z: 7 }
  skills: { sword: 50, shielding: 40 }
kit:
  backpack: 2854
  weapon: 3264
  contents:
    - { item: 266, stacks: 2, amount: 100 }
"#;

    #[test]
    fn a_kit_parses() {
        let kit = parse(A_KIT).unwrap();

        assert_eq!(kit.character.vocation, Vocation::Sorcerer);
        assert_eq!(kit.character.level, 20);
        assert_eq!(kit.character.skills[&SkillType::Sword], 50);
        assert_eq!(kit.kit.backpack, 2854);
        assert_eq!(kit.kit.weapon, 3264);
        assert_eq!(kit.kit.contents[0].item, 266);
    }

    #[test]
    fn level_may_not_be_listed_under_skills() {
        let with_level = A_KIT.replace("shielding: 40", "shielding: 40, level: 30");

        let err = parse(&with_level).unwrap_err();

        assert!(err.to_string().contains("level"), "{err}");
    }

    #[test]
    fn an_unknown_skill_name_is_rejected() {
        let with_typo = A_KIT.replace("sword: 50", "swrod: 50");

        assert!(parse(&with_typo).is_err());
    }

    #[test]
    fn a_run_config_parses() {
        let config: RunConfig = serde_yaml::from_str(
            r#"
routes:
  - name: town-to-rats
    waypoints: [{ x: 32097, y: 31103, z: 7 }, { x: 32110, y: 31110, z: 7 }]
behaviour:
  engage_radius: 5
  health_potion_pct: 45
  heal_spell_pct: 70
  mana_potion_pct: 35
  attack_spell: exori flam
  heal_spell: exura
  loot_items_max: 4
  decision_interval_ms: [180, 320]
  ping_interval_ms: [4000, 6000]
"#,
        )
        .unwrap();

        assert_eq!(config.routes[0].waypoints.len(), 2);
        assert_eq!(config.behaviour.attack_spell, "exori flam");
    }
}

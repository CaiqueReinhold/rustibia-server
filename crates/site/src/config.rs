use rustibia_contract::StoredItemRecord;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::domain::{equip_slot::EquipSlot, sex::Sex, vocation::Vocation};

#[derive(Debug, Deserialize, Clone)]
pub struct StartingSkill {
    pub skill_type: i16,
    pub value: i16,
    pub current_ticks: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct VocationKit {
    pub outfit_id_male: i16,
    pub outfit_id_female: i16,
    pub inventory: HashMap<EquipSlot, StoredItemRecord>,
}

impl VocationKit {
    pub fn outfit_id(&self, sex: Sex) -> i16 {
        match sex {
            Sex::Female => self.outfit_id_female,
            Sex::Male => self.outfit_id_male,
        }
    }

    pub fn stored_inventory(&self) -> HashMap<String, StoredItemRecord> {
        self.inventory
            .iter()
            .map(|(slot, item)| (slot.index().to_string(), item.clone()))
            .collect()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct VocationKits {
    pub knight: VocationKit,
    pub paladin: VocationKit,
    pub sorcerer: VocationKit,
    pub druid: VocationKit,
}

impl VocationKits {
    pub fn for_vocation(&self, vocation: Vocation) -> &VocationKit {
        match vocation {
            Vocation::Knight => &self.knight,
            Vocation::Paladin => &self.paladin,
            Vocation::Sorcerer => &self.sorcerer,
            Vocation::Druid => &self.druid,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct NewCharacterConfig {
    pub pos_x: i32,
    pub pos_y: i32,
    pub pos_z: i16,
    pub facing: i16,
    pub life: i32,
    pub mana: i32,
    pub capacity: i32,
    pub speed: i32,
    pub outfit_head: i16,
    pub outfit_body: i16,
    pub outfit_legs: i16,
    pub outfit_feet: i16,
    pub starting_skills: Vec<StartingSkill>,
    pub vocations: VocationKits,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SiteConfig {
    pub session_ttl_days: i64,
    pub auth_token_ttl_seconds: i64,
    pub min_password_length: usize,
    pub client_version: String,
    pub new_character: NewCharacterConfig,
}

impl SiteConfig {
    pub fn from_yaml(contents: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(contents)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ConfigError::Read(path.as_ref().display().to_string(), e))?;
        Ok(Self::from_yaml(&contents)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config at {0}: {1}")]
    Read(String, std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CONFIG: &str = r#"session_ttl_days: 7
auth_token_ttl_seconds: 60
min_password_length: 8
client_version: "0.3.0"
new_character:
  pos_x: 1028
  pos_y: 1028
  pos_z: 7
  facing: 2
  life: 150
  mana: 0
  capacity: 400
  speed: 100
  outfit_head: 78
  outfit_body: 69
  outfit_legs: 58
  outfit_feet: 76
  starting_skills:
    - { skill_type: 0, value: 1, current_ticks: 0 }
    - { skill_type: 1, value: 220, current_ticks: 0 }
  vocations:
    knight:
      outfit_id_male: 131
      outfit_id_female: 139
      inventory:
        head: { item_id: 3375 }
        backpack: { item_id: 2854, content: [{ item_id: 3725, amount: 10 }] }
    paladin: { outfit_id_male: 129, outfit_id_female: 137, inventory: {} }
    sorcerer: { outfit_id_male: 130, outfit_id_female: 138, inventory: {} }
    druid: { outfit_id_male: 133, outfit_id_female: 141, inventory: {} }
"#;

    #[test]
    fn parses_a_full_config() {
        let cfg = SiteConfig::from_yaml(FULL_CONFIG).unwrap();
        assert_eq!(cfg.session_ttl_days, 7);
        assert_eq!(cfg.auth_token_ttl_seconds, 60);
        assert_eq!(cfg.min_password_length, 8);
        assert_eq!(cfg.client_version, "0.3.0");
        assert_eq!(cfg.new_character.pos_x, 1028);
        assert_eq!(cfg.new_character.starting_skills.len(), 2);
    }

    #[test]
    fn rejects_a_config_without_a_client_version() {
        let without = FULL_CONFIG.replace("client_version: \"0.3.0\"\n", "");
        assert!(SiteConfig::from_yaml(&without).is_err());
    }

    #[test]
    fn the_checked_in_config_names_a_client_version() {
        let cfg = SiteConfig::load("config.yaml").unwrap();
        assert!(!cfg.client_version.is_empty());
    }

    #[test]
    fn rejects_a_config_missing_a_field() {
        assert!(SiteConfig::from_yaml("session_ttl_days: 7\n").is_err());
    }

    #[test]
    fn loads_the_checked_in_config_file() {
        let cfg = SiteConfig::load("config.yaml").unwrap();
        assert!(cfg.min_password_length >= 8);
    }

    #[test]
    fn starting_skills_only_reference_skill_types_the_game_server_knows() {
        let cfg = SiteConfig::load("config.yaml").unwrap();
        let mut seeded: Vec<i16> = cfg
            .new_character
            .starting_skills
            .iter()
            .map(|skill| skill.skill_type)
            .collect();
        seeded.sort_unstable();

        assert_eq!(
            seeded,
            (0..=6).collect::<Vec<i16>>(),
            "the game server's SkillType enum is 0 = Level, 1 = Axe, 2 = Club, 3 = Sword, \
             4 = Distance, 5 = Magic, 6 = Shielding; seeding anything else would silently \
             destroy the row on first logout, and seeding fewer leaves a skill unseeded"
        );
    }

    #[test]
    fn each_vocation_has_its_own_outfit() {
        let kits = SiteConfig::from_yaml(FULL_CONFIG)
            .unwrap()
            .new_character
            .vocations;

        assert_eq!(
            kits.for_vocation(Vocation::Knight).outfit_id(Sex::Male),
            131
        );
        assert_eq!(
            kits.for_vocation(Vocation::Paladin).outfit_id(Sex::Female),
            137
        );
        assert_eq!(
            kits.for_vocation(Vocation::Sorcerer).outfit_id(Sex::Male),
            130
        );
        assert_eq!(
            kits.for_vocation(Vocation::Druid).outfit_id(Sex::Female),
            141
        );
    }

    #[test]
    fn rejects_a_config_missing_a_vocation() {
        let without = FULL_CONFIG.replace(
            "    druid: { outfit_id_male: 133, outfit_id_female: 141, inventory: {} }\n",
            "",
        );
        assert_ne!(without, FULL_CONFIG);

        assert!(SiteConfig::from_yaml(&without).is_err());
    }

    #[test]
    fn rejects_a_kit_naming_an_unknown_slot() {
        let misnamed = FULL_CONFIG.replace("head: { item_id: 3375 }", "hat: { item_id: 3375 }");
        assert_ne!(misnamed, FULL_CONFIG);

        assert!(SiteConfig::from_yaml(&misnamed).is_err());
    }

    #[test]
    fn a_kit_is_stored_under_the_slot_index() {
        let kits = SiteConfig::from_yaml(FULL_CONFIG)
            .unwrap()
            .new_character
            .vocations;

        let stored = kits.for_vocation(Vocation::Knight).stored_inventory();

        assert_eq!(stored.len(), 2);
        assert_eq!(stored["1"].item_id, 3375);
        assert_eq!(stored["1"].amount, 1, "an omitted amount is one");
        assert_eq!(stored["3"].item_id, 2854);
        assert_eq!(stored["3"].content.as_ref().unwrap()[0].amount, 10);
    }
}

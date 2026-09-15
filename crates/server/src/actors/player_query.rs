use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    entities::{
        agent::{Agent, AgentId, AgentKey},
        inventory::InventorySlot,
        map::GameMap,
        position::Position,
        skills::SkillType,
        spells::{Spell, SpellId},
        vocation::Vocation,
    },
    game::skills::{progress_bp, total_experience},
    messages::{ServerMessage, SkillProgress, SpellListEntry},
    persistence::spells::SPELLS,
};

pub fn get_player_desc(map: &GameMap, key: AgentKey, id: AgentId) -> Option<ServerMessage> {
    let agent = map.get_agent(key)?;
    let position = map.agent_position(key)?;
    let player = agent.get_player()?;

    let slot_item = |slot: InventorySlot| player.inventory().get(&slot).map(|it| it.item_id);

    Some(ServerMessage::DescribePlayer {
        agent_id: id,
        position: position.clone(),
        facing: agent.facing(),
        name: agent.name().to_string(),
        level: player.level(),
        life: agent.life().clone(),
        mana: agent.mana().clone(),
        outfit: agent.outfit(),
        speed: agent.speed(),
        capacity: player.capacity_available(),
        inventory_head: slot_item(InventorySlot::Head),
        inventory_amulet: slot_item(InventorySlot::Amulet),
        inventory_backpack: slot_item(InventorySlot::Backpack),
        inventory_chest: slot_item(InventorySlot::Chest),
        inventory_right_hand: slot_item(InventorySlot::RightHand),
        inventory_left_hand: slot_item(InventorySlot::LeftHand),
        inventory_legs: slot_item(InventorySlot::Legs),
        inventory_feet: slot_item(InventorySlot::Feet),
        inventory_ring: slot_item(InventorySlot::Ring),
        inventory_trinket: slot_item(InventorySlot::Trinket),
    })
}

pub fn get_player_skills(map: &GameMap, key: AgentKey) -> Option<ServerMessage> {
    let player = map.get_player(key)?;

    let mut skills: Vec<(SkillType, SkillProgress)> = player
        .skills()
        .iter()
        .map(|(skill, value)| {
            (
                *skill,
                SkillProgress {
                    level: value.value,
                    percent_bp: progress_bp(player.vocation(), skill, value),
                },
            )
        })
        .collect();
    skills.sort_by_key(|(skill, _)| skill.as_id());

    Some(ServerMessage::PlayerSkills {
        experience: player
            .skills()
            .get(&SkillType::Level)
            .map(total_experience)
            .unwrap_or(0),
        skills,
    })
}

pub fn spell_list_for(vocation: Vocation, spells: &HashMap<SpellId, Arc<Spell>>) -> ServerMessage {
    let mut entries: Vec<SpellListEntry> = spells
        .values()
        .filter(|spell| spell.vocations.contains(&vocation))
        .map(|spell| SpellListEntry {
            id: spell.id,
            name: spell.name.clone(),
            words: spell.words.clone(),
            level: spell.level,
            icon: spell.icon,
            aimable: spell.is_aimable(),
            group: spell.group,
        })
        .collect();
    entries.sort_by_key(|entry| (entry.level, entry.id.0));
    ServerMessage::SpellList { spells: entries }
}

pub fn get_spell_list(map: &GameMap, key: AgentKey) -> Option<ServerMessage> {
    let player = map.get_player(key)?;
    Some(spell_list_for(player.vocation(), &SPELLS))
}

pub fn get_agent_desc(agent: &Agent, agent_id: AgentId, position: Position) -> ServerMessage {
    ServerMessage::SpawnAgent {
        agent_id,
        outfit: agent.outfit(),
        position,
        facing: agent.facing(),
        name: agent.name().to_owned(),
        life: agent.life().to_wire(),
        speed: agent.speed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::MapTile;
    use crate::entities::skills::{SkillType, SkillValue};
    use crate::messages::ServerMessage;
    use crate::persistence::test_fixtures::a_test_snapshot;

    /// Rows arrive in id order regardless of how the player's `HashMap` iterates,
    /// so the frame is reproducible and a client can rely on it.
    #[tokio::test]
    async fn skills_are_sent_in_id_order_with_the_experience_total() {
        let position = Position::new(100, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &position)
            .unwrap();
        let sword = SkillValue {
            value: 11,
            current_ticks: 27,
        };
        let player = map.get_player_mut(key).unwrap();
        let sword_bp = progress_bp(player.vocation(), &SkillType::Sword, &sword);
        player.skills_mut().insert(SkillType::Sword, sword);
        player.skills_mut().insert(
            SkillType::Level,
            SkillValue {
                value: 8,
                current_ticks: 55,
            },
        );

        let message = get_player_skills(&map, key).unwrap();

        let ServerMessage::PlayerSkills { experience, skills } = message else {
            panic!("expected PlayerSkills");
        };
        assert_eq!(experience, 4255);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].0, SkillType::Level);
        assert_eq!(skills[0].1.level, 8);
        assert_eq!(skills[1].0, SkillType::Sword);
        assert_eq!(skills[1].1.percent_bp, sword_bp);
    }

    /// A character whose `Level` row never loaded still gets a window, with a
    /// zero rather than a missing message.
    #[tokio::test]
    async fn a_missing_level_row_reports_no_experience() {
        let position = Position::new(100, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &position)
            .unwrap();
        map.get_player_mut(key).unwrap().skills_mut().clear();

        let ServerMessage::PlayerSkills { experience, skills } =
            get_player_skills(&map, key).unwrap()
        else {
            panic!("expected PlayerSkills");
        };
        assert_eq!(experience, 0);
        assert!(skills.is_empty());
    }

    fn a_heal(id: u16, level: u16, vocations: &str) -> String {
        format!(
            r#"
  - id: {id}
    name: Spell {id}
    words: words {id}
    group: healing
    cooldown_ticks: 20
    mana: 20
    level: {level}
    icon: {id}
    vocations: [{vocations}]
    effect:
      type: heal
      target:
        type: self
      base_power: 8
      level_factor: 0.2
      magic_factor: 1.4
"#
        )
    }

    #[test]
    fn a_vocation_is_listed_its_own_spells_by_level_then_id() {
        use crate::entities::vocation::Vocation;
        use crate::persistence::spells::load_spells_from_str;
        use std::collections::HashMap;

        let yaml = format!(
            "spells:{}{}{}",
            a_heal(2, 20, "druid"),
            a_heal(5, 8, "druid, sorcerer"),
            a_heal(1, 8, "sorcerer")
        );
        let spells = load_spells_from_str(&yaml, &HashMap::new()).unwrap();
        let ids = |vocation| {
            let ServerMessage::SpellList { spells } = spell_list_for(vocation, &spells) else {
                panic!("expected SpellList");
            };
            spells.iter().map(|spell| spell.id.0).collect::<Vec<_>>()
        };

        assert_eq!(ids(Vocation::Druid), vec![5, 2]);
        assert_eq!(ids(Vocation::Sorcerer), vec![1, 5]);
        assert!(ids(Vocation::Knight).is_empty());
    }

    #[test]
    fn each_listed_spell_carries_its_group() {
        use crate::entities::spells::SpellGroup;
        use crate::entities::vocation::Vocation;
        use crate::persistence::spells::load_spells_from_str;
        use std::collections::HashMap;

        let yaml = format!("spells:{}", a_heal(1, 8, "druid"));
        let spells = load_spells_from_str(&yaml, &HashMap::new()).unwrap();

        let ServerMessage::SpellList { spells } = spell_list_for(Vocation::Druid, &spells) else {
            panic!("expected SpellList");
        };
        assert_eq!(spells[0].group, SpellGroup::Healing);
    }
}

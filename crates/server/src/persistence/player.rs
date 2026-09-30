//! A player's saved state, and the shape it travels to the site in.

use std::collections::HashMap;

use rustibia_contract::{CharacterSave, Coords, Outfit, PoolValue, SkillRow, StoredItemRecord};

use crate::entities::vocation::Vocation;
use crate::entities::{
    agent::{Facing, OutfitColors, OutfitId, Pool},
    inventory::InventorySlot,
    items::Item,
    player::PlayerId,
    position::Position,
    skills::{SkillType, SkillValue},
};

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub id: PlayerId,
    pub account_id: i32,
    pub admin: bool,
    pub position: Position,
    pub origin: Position,
    pub facing: Facing,
    pub name: String,
    pub vocation: Vocation,
    pub life: Pool,
    pub mana: Pool,
    pub capacity: u32,
    pub speed: u16,
    pub outfit: (OutfitId, OutfitColors),
    pub skills: HashMap<SkillType, SkillValue>,
    pub inventory: HashMap<InventorySlot, Item>,
    pub save_version: i64,
}

pub fn to_character_save(snapshot: &PlayerSnapshot) -> CharacterSave {
    let (outfit_id, colors) = snapshot.outfit;
    CharacterSave {
        id: snapshot.id.0 as i32,
        save_version: snapshot.save_version,
        position: coords(&snapshot.position),
        origin: coords(&snapshot.origin),
        facing: snapshot.facing.as_id() as i16,
        life: PoolValue {
            current: snapshot.life.current as i32,
            maximum: snapshot.life.maximum as i32,
        },
        mana: PoolValue {
            current: snapshot.mana.current as i32,
            maximum: snapshot.mana.maximum as i32,
        },
        capacity: snapshot.capacity as i32,
        speed: snapshot.speed as i32,
        outfit: Outfit {
            id: outfit_id.0 as i16,
            head: colors.head as i16,
            body: colors.body as i16,
            legs: colors.legs as i16,
            feet: colors.feet as i16,
        },
        skills: snapshot
            .skills
            .iter()
            .map(|(skill_type, skill)| SkillRow {
                skill_type: skill_type.as_id() as i16,
                value: skill.value as i16,
                current_ticks: skill.current_ticks as i64,
            })
            .collect(),
        inventory: snapshot
            .inventory
            .iter()
            .map(|(slot, item)| (slot.as_id().to_string(), stored_item(item)))
            .collect(),
    }
}

fn coords(position: &Position) -> Coords {
    Coords {
        x: position.x as i32,
        y: position.y as i32,
        z: position.z as i16,
    }
}

fn stored_item(item: &Item) -> StoredItemRecord {
    StoredItemRecord {
        item_id: item.id().0,
        amount: item.amount,
        content: item
            .content
            .as_ref()
            .map(|children| children.iter().map(stored_item).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_save_carries_login_reads_back() {
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        use rustibia_contract::CharacterRecord;

        use crate::entities::inventory::InventorySlot;
        use crate::entities::items::{ItemAttribute, ItemConfig, ItemFlag, ItemId};
        use crate::entities::skills::SkillType;
        use crate::persistence::login::snapshot_from_record;
        use crate::persistence::test_fixtures::a_test_snapshot;

        let config = |id: u16, flags: HashSet<ItemFlag>, attributes: Vec<ItemAttribute>| {
            Arc::new(ItemConfig::new(ItemId(id), format!("item {id}"), None, None, flags, attributes))
        };
        let bag = config(1987, HashSet::from([ItemFlag::Container]), vec![ItemAttribute::Capacity(8)]);
        let coin = config(3031, HashSet::from([ItemFlag::Cumulative, ItemFlag::Take]), Vec::new());
        let mut backpack = Item::new(Arc::clone(&bag), 1);
        backpack.content = Some(Box::new(vec![Item::new(Arc::clone(&coin), 12)]));
        let mut snapshot = a_test_snapshot(7, 3);
        snapshot.save_version = 4;
        snapshot.life.current = 37;
        snapshot.inventory = HashMap::from([(InventorySlot::Backpack, backpack)]);

        let save = to_character_save(&snapshot);
        let record = CharacterRecord {
            id: save.id,
            account_id: 3,
            admin: false,
            name: snapshot.name.clone(),
            vocation: 0,
            position: save.position,
            origin: save.origin,
            facing: save.facing,
            life: save.life,
            mana: save.mana,
            capacity: save.capacity,
            speed: save.speed,
            outfit: save.outfit,
            skills: save.skills,
            inventory: save.inventory,
            save_version: save.save_version,
        };
        let configs = HashMap::from([(ItemId(1987), bag), (ItemId(3031), coin)]);
        let restored = snapshot_from_record(record, &configs).unwrap();

        assert_eq!(restored.id, snapshot.id);
        assert_eq!(restored.save_version, 4);
        assert_eq!(restored.position, snapshot.position);
        assert_eq!(restored.origin, snapshot.origin);
        assert_eq!(restored.facing, snapshot.facing);
        assert_eq!(restored.life, snapshot.life);
        assert_eq!(restored.mana, snapshot.mana);
        assert_eq!((restored.capacity, restored.speed), (snapshot.capacity, snapshot.speed));
        assert_eq!(
            restored.skills[&SkillType::Level].current_ticks,
            snapshot.skills[&SkillType::Level].current_ticks
        );
        let restored_bag = &restored.inventory[&InventorySlot::Backpack];
        assert_eq!(restored_bag.id(), ItemId(1987));
        let content = restored_bag.content.as_ref().unwrap();
        assert_eq!((content[0].id(), content[0].amount), (ItemId(3031), 12));
    }
}

use std::ops::Deref;

use crate::entities::agent::{Agent, AgentKey, Facing};
use crate::entities::conditions::Conditions;
use crate::entities::inventory::{Inventory, InventorySlot};
use crate::entities::items::{Item, ItemGuid};
#[cfg(test)]
use crate::entities::map::MapTile;
use crate::entities::map::{GameMap, MapError, RemovedItem};
use crate::entities::player::Player;
use crate::entities::position::{ItemPlacement, PlacementSite, Position};
use crate::entities::skills::{SkillType, SkillValue};
use crate::entities::spells::{Spell, SpellGroup};
use crate::entities::world_delta::{AgentFlag, WorldDelta};
use crate::game::item_movement::ItemMovementError;
use crate::game::{Tick, TickDelta};

pub struct WorldMap {
    map: GameMap,
    delta: WorldDelta,
}

impl Deref for WorldMap {
    type Target = GameMap;

    fn deref(&self) -> &GameMap {
        &self.map
    }
}

impl WorldMap {
    pub fn new(map: GameMap) -> Self {
        Self {
            map,
            delta: WorldDelta::default(),
        }
    }

    pub fn snapshot(&self) -> GameMap {
        self.map.clone()
    }

    pub fn take_delta(&mut self) -> WorldDelta {
        std::mem::take(&mut self.delta)
    }

    pub fn take_chunk_copies(&mut self) -> u64 {
        self.map.take_chunk_copies()
    }

    #[cfg(test)]
    pub fn delta(&self) -> &WorldDelta {
        &self.delta
    }

    /// Writes made through this are not recorded in the delta.
    #[cfg(test)]
    pub fn inner_mut(&mut self) -> &mut GameMap {
        &mut self.map
    }

    #[cfg(test)]
    pub fn insert_tile(&mut self, pos: Position, tile: MapTile) {
        self.delta.mark_tile(&pos);
        self.map.insert_tile(pos, tile);
    }

    pub fn place_item(
        &mut self,
        pos: &Position,
        index: Option<usize>,
        container: Option<(&ItemGuid, usize)>,
        item: Item,
    ) -> Result<&Item, MapError> {
        let placed = self.map.place_item(pos, index, container, item)?;
        match container {
            Some((guid, _)) => self.delta.mark_container(guid),
            None => self.delta.mark_tile(pos),
        }
        Ok(placed)
    }

    pub fn remove_item_from_tile(
        &mut self,
        pos: &Position,
        guid: &ItemGuid,
        amount: u8,
    ) -> Option<RemovedItem> {
        let removed = self.map.remove_item_from_tile(pos, guid, amount)?;
        match &removed.2 {
            Some((parent, _)) => self.delta.mark_container(parent),
            None => self.delta.mark_tile(pos),
        }
        Some(removed)
    }

    pub fn insert_agent(&mut self, agent: Agent, pos: &Position) -> Result<AgentKey, MapError> {
        self.map.insert_agent(agent, pos)
    }

    pub fn remove_agent(&mut self, key: AgentKey) -> Option<(Agent, Position)> {
        self.map.remove_agent(key)
    }

    pub fn move_agent(&mut self, key: AgentKey, new_pos: &Position) -> Result<(), MapError> {
        self.map.move_agent(key, new_pos)
    }

    pub fn agent_mut(&mut self, key: AgentKey) -> Option<AgentMut<'_>> {
        let agent = self.map.get_agent_mut(key)?;
        Some(AgentMut {
            agent,
            key,
            delta: &mut self.delta,
        })
    }

    pub fn player_mut(&mut self, key: AgentKey) -> Option<PlayerMut<'_>> {
        let player = self.map.get_player_mut(key)?;
        Some(PlayerMut {
            player,
            key,
            delta: &mut self.delta,
        })
    }

    pub fn stack_onto(&mut self, placement: &ItemPlacement, item: &mut Item) -> u8 {
        let container = placement.container().map(|(guid, _)| guid);
        match placement.site() {
            PlacementSite::Tile(pos) => {
                let Ok(mut items) = self.map.iter_items_mut(pos) else {
                    return 0;
                };
                let stack = match container {
                    Some(guid) => items
                        .find_map(|it| it.find_by_guid_mut(guid))
                        .and_then(|found| found.content.as_mut())
                        .and_then(|content| content.iter_mut().find(|it| it.stacks_with(item))),
                    None => items.find(|it| it.stacks_with(item)),
                };
                let moved = stack.map_or(0, |stack| stack.top_up_from(item));
                if moved > 0 {
                    match container {
                        Some(guid) => self.delta.mark_container(guid),
                        None => self.delta.mark_tile(pos),
                    }
                }
                moved
            }
            PlacementSite::Slot(slot, agent_key) => {
                self.player_mut(agent_key).map_or(0, |mut player| {
                    player.inventory_mut().stack_onto(slot, container, item)
                })
            }
        }
    }

    pub fn step_agent(
        &mut self,
        key: AgentKey,
        new_pos: &Position,
        facing: Facing,
    ) -> Result<(), MapError> {
        self.map.move_agent(key, new_pos)?;
        if let Some(agent) = self.map.get_agent_mut(key) {
            agent.set_facing(facing);
        }
        Ok(())
    }
}

pub struct AgentMut<'a> {
    agent: &'a mut Agent,
    key: AgentKey,
    delta: &'a mut WorldDelta,
}

impl Deref for AgentMut<'_> {
    type Target = Agent;

    fn deref(&self) -> &Agent {
        self.agent
    }
}

impl AgentMut<'_> {
    fn mark(&mut self, flag: AgentFlag) {
        self.delta.mark_agent(self.key, flag);
    }

    pub fn remove_life(&mut self, amount: u32) {
        self.agent.remove_life(amount);
        self.mark(AgentFlag::Life);
    }

    pub fn restore_life(&mut self, amount: u32) {
        self.agent.restore_life(amount);
        self.mark(AgentFlag::Life);
    }

    pub fn change_max_life(&mut self, amount: i32) {
        self.agent.change_max_life(amount);
        self.mark(AgentFlag::Life);
    }

    pub fn remove_mana(&mut self, amount: u32) {
        let status = self.agent.conditions().status();
        self.agent.remove_mana(amount);
        self.mark(AgentFlag::Mana);
        if self.agent.conditions().status() != status {
            self.mark(AgentFlag::Status);
        }
    }

    pub fn restore_mana(&mut self, amount: u32) {
        self.agent.restore_mana(amount);
        self.mark(AgentFlag::Mana);
    }

    pub fn change_max_mana(&mut self, amount: i32) {
        self.agent.change_max_mana(amount);
        self.mark(AgentFlag::Mana);
    }

    pub fn set_base_speed(&mut self, speed: u16) {
        self.agent.set_base_speed(speed);
        self.mark(AgentFlag::Speed);
    }

    pub fn set_facing(&mut self, facing: Facing) {
        if self.agent.facing() != facing {
            self.agent.set_facing(facing);
            self.mark(AgentFlag::Facing);
        }
    }

    pub fn conditions<R>(&mut self, f: impl FnOnce(&mut Conditions) -> R) -> R {
        let status = self.agent.conditions().status();
        let speed = self.agent.speed();
        let result = f(self.agent.conditions_mut());
        if self.agent.conditions().status() != status {
            self.mark(AgentFlag::Status);
        }
        if self.agent.speed() != speed {
            self.mark(AgentFlag::Speed);
        }
        result
    }

    pub fn stamp_walk(&mut self, until: Tick) {
        self.agent.next_walk_tick = until;
    }

    pub fn stamp_use(&mut self, until: Tick) {
        self.agent.next_use_tick = until;
    }

    pub fn stamp_auto_attack(&mut self, tick: Tick) {
        self.agent.stamp_auto_attack(tick);
    }

    pub fn stamp_spell(&mut self, tick: Tick, spell: &Spell) {
        self.agent.stamp_spell(tick, spell);
    }

    pub fn stamp_spell_group(
        &mut self,
        tick: Tick,
        group: SpellGroup,
        cooldown: Option<TickDelta>,
    ) {
        self.agent.stamp_spell_group(tick, group, cooldown);
    }

    pub fn set_target(&mut self, target: Option<AgentKey>, seq: u32) {
        self.agent.set_target(target, seq);
    }

    pub fn record_damage(&mut self, attacker: AgentKey, damage: u32) {
        self.agent.record_damage(attacker, damage);
    }

    pub fn player_mut(&mut self) -> Option<PlayerMut<'_>> {
        let player = self.agent.get_player_mut()?;
        Some(PlayerMut {
            player,
            key: self.key,
            delta: &mut *self.delta,
        })
    }
}

pub struct PlayerMut<'a> {
    player: &'a mut Player,
    key: AgentKey,
    delta: &'a mut WorldDelta,
}

impl Deref for PlayerMut<'_> {
    type Target = Player;

    fn deref(&self) -> &Player {
        self.player
    }
}

impl PlayerMut<'_> {
    pub fn skill_mut(&mut self, skill: SkillType) -> Option<&mut SkillValue> {
        let value = self.player.skills_mut().get_mut(&skill)?;
        self.delta.mark_skill(self.key, skill);
        Some(value)
    }

    pub fn set_capacity(&mut self, capacity: u32) {
        self.player.set_capacity(capacity);
        self.delta.mark_agent(self.key, AgentFlag::Capacity);
    }

    pub fn inventory_mut(&mut self) -> InventoryMut<'_> {
        InventoryMut {
            inventory: self.player.inventory_mut(),
            key: self.key,
            delta: &mut *self.delta,
        }
    }
}

pub struct InventoryMut<'a> {
    inventory: &'a mut Inventory,
    key: AgentKey,
    delta: &'a mut WorldDelta,
}

impl Deref for InventoryMut<'_> {
    type Target = Inventory;

    fn deref(&self) -> &Inventory {
        self.inventory
    }
}

impl InventoryMut<'_> {
    fn record_write(
        &mut self,
        slot: InventorySlot,
        container: Option<&ItemGuid>,
        speed_before: i16,
    ) {
        match container {
            Some(guid) => self.delta.mark_container(guid),
            None => self.delta.mark_slot(self.key, slot),
        }
        self.delta.mark_agent(self.key, AgentFlag::Capacity);
        if self.inventory.stats().speed != speed_before {
            self.delta.mark_agent(self.key, AgentFlag::Speed);
        }
    }

    pub fn insert(
        &mut self,
        slot: InventorySlot,
        container: Option<(&ItemGuid, usize)>,
        item: Item,
    ) -> Result<Option<Item>, ItemMovementError> {
        let speed = self.inventory.stats().speed;
        let displaced = self.inventory.insert(slot, container, item)?;
        self.record_write(slot, container.map(|(guid, _)| guid), speed);
        Ok(displaced)
    }

    pub fn remove(
        &mut self,
        slot: InventorySlot,
        guid: &ItemGuid,
        amount: u8,
    ) -> Option<(Item, Option<(ItemGuid, usize)>)> {
        let speed = self.inventory.stats().speed;
        let removed = self.inventory.remove(slot, guid, amount)?;
        self.record_write(slot, removed.1.as_ref().map(|(parent, _)| parent), speed);
        Some(removed)
    }

    pub fn take_slot(&mut self, slot: &InventorySlot) -> Option<Item> {
        let speed = self.inventory.stats().speed;
        let taken = self.inventory.take_slot(slot)?;
        self.record_write(*slot, None, speed);
        Some(taken)
    }

    pub(in crate::entities) fn stack_onto(
        &mut self,
        slot: InventorySlot,
        container: Option<&ItemGuid>,
        item: &mut Item,
    ) -> u8 {
        let unit_weight = item.config.attr_weight().unwrap_or(0);
        let speed = self.inventory.stats().speed;
        let Some(slot_item) = self.inventory.get_mut(&slot) else {
            return 0;
        };
        let stack = match container {
            Some(guid) => slot_item
                .find_by_guid_mut(guid)
                .and_then(|found| found.content.as_mut())
                .and_then(|content| content.iter_mut().find(|it| it.stacks_with(item))),
            None => Some(slot_item).filter(|it| it.stacks_with(item)),
        };
        let moved = stack.map_or(0, |stack| stack.top_up_from(item));
        if moved == 0 {
            return 0;
        }
        let carried = self.inventory.carried_weight();
        self.inventory
            .set_carried_weight(carried + unit_weight * moved as u32);
        self.record_write(slot, container, speed);
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::combat::CombatElement;
    use crate::entities::conditions::SpeedEffect;
    use crate::entities::inventory::InventorySlot;
    use crate::entities::items::{ItemAttribute, ItemConfig, ItemFlag, ItemId};
    use crate::entities::position::ItemPlacement;
    use crate::entities::skills::SkillType;
    use crate::game::Tick;
    use crate::persistence::test_fixtures::a_player_with_a_full_backpack;
    use crate::persistence::test_fixtures::a_test_snapshot;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn config(
        id: u16,
        flags: HashSet<ItemFlag>,
        attributes: Vec<ItemAttribute>,
    ) -> Arc<ItemConfig> {
        Arc::new(ItemConfig::new(
            ItemId(id),
            format!("item {id}"),
            None,
            None,
            flags,
            attributes,
        ))
    }

    fn a_bag() -> Item {
        Item::new(
            config(
                1987,
                HashSet::from([ItemFlag::Container]),
                vec![ItemAttribute::Capacity(8)],
            ),
            1,
        )
    }

    fn coins(amount: u8) -> Item {
        Item::new(
            config(
                2148,
                HashSet::from([ItemFlag::Cumulative, ItemFlag::Take]),
                vec![ItemAttribute::Weight(1)],
            ),
            amount,
        )
    }

    fn one_tile() -> (WorldMap, Position) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        (WorldMap::new(map), pos)
    }

    #[test]
    fn placing_on_a_tile_marks_the_tile() {
        let (mut map, pos) = one_tile();

        map.place_item(&pos, None, None, coins(1)).unwrap();

        assert!(map.delta().tile_dirty(&pos));
    }

    #[test]
    fn placing_into_a_container_marks_the_container_and_not_the_tile() {
        let (mut map, pos) = one_tile();
        let bag = a_bag();
        let guid = bag.guid;
        map.inner_mut().place_item(&pos, None, None, bag).unwrap();

        map.place_item(&pos, None, Some((&guid, 0)), coins(1))
            .unwrap();

        assert!(map.delta().container_dirty(&guid));
        assert!(!map.delta().tile_dirty(&pos));
    }

    #[test]
    fn a_failed_placement_marks_nothing() {
        let (mut map, _) = one_tile();

        assert!(
            map.place_item(&Position::new(50, 50, 7), None, None, coins(1))
                .is_err()
        );

        assert!(map.delta().is_empty());
    }

    #[test]
    fn removing_from_a_container_on_a_tile_marks_the_container() {
        let (mut map, pos) = one_tile();
        let mut bag = a_bag();
        let coin = coins(1);
        let coin_guid = coin.guid;
        bag.content.as_mut().unwrap().push(coin);
        let bag_guid = bag.guid;
        map.inner_mut().place_item(&pos, None, None, bag).unwrap();

        map.remove_item_from_tile(&pos, &coin_guid, 1).unwrap();

        assert!(map.delta().container_dirty(&bag_guid));
        assert!(!map.delta().tile_dirty(&pos));
    }

    #[test]
    fn removing_from_the_tile_marks_the_tile() {
        let (mut map, pos) = one_tile();
        let coin = coins(1);
        let guid = coin.guid;
        map.inner_mut().place_item(&pos, None, None, coin).unwrap();

        map.remove_item_from_tile(&pos, &guid, 1).unwrap();

        assert!(map.delta().tile_dirty(&pos));
    }

    #[test]
    fn a_step_sets_the_facing_and_marks_nothing() {
        let (here, there) = (Position::new(10, 10, 7), Position::new(11, 10, 7));
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());
        map.insert_tile(there.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        let mut map = WorldMap::new(map);

        map.step_agent(key, &there, Facing::East).unwrap();

        assert_eq!(map.agent_position(key), Some(&there));
        assert_eq!(map.get_agent(key).unwrap().facing(), Facing::East);
        assert!(map.delta().is_empty());
    }

    #[test]
    fn taking_the_delta_leaves_an_empty_one_behind() {
        let (mut map, pos) = one_tile();
        map.place_item(&pos, None, None, coins(1)).unwrap();

        let taken = map.take_delta();

        assert!(taken.tile_dirty(&pos));
        assert!(map.delta().is_empty());
    }

    #[test]
    fn the_snapshot_does_not_see_later_writes() {
        let (mut map, pos) = one_tile();
        let snapshot = map.snapshot();

        map.place_item(&pos, None, None, coins(1)).unwrap();

        assert_eq!(snapshot.iter_items(&pos).unwrap().count(), 0);
    }

    fn a_player_alone() -> (WorldMap, AgentKey) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        (WorldMap::new(map), key)
    }

    #[test]
    fn life_writes_mark_life() {
        let (mut map, key) = a_player_alone();

        let mut agent = map.agent_mut(key).unwrap();
        agent.remove_life(10);
        agent.restore_life(5);
        agent.change_max_life(10);

        let dirty = map.delta().agent(key);
        assert!(dirty.life());
        assert!(!dirty.mana() && !dirty.status());
    }

    #[test]
    fn mana_writes_mark_mana_only_while_no_shield_breaks() {
        let (mut map, key) = a_player_alone();

        map.agent_mut(key).unwrap().remove_mana(10);

        let dirty = map.delta().agent(key);
        assert!(dirty.mana());
        assert!(!dirty.status());
    }

    #[test]
    fn emptying_a_shielded_pool_marks_the_status_too() {
        let (mut map, key) = a_player_alone();
        map.inner_mut()
            .get_agent_mut(key)
            .unwrap()
            .conditions_mut()
            .extend_magic_shield(Tick(1000));

        map.agent_mut(key).unwrap().remove_mana(100);

        let dirty = map.delta().agent(key);
        assert!(dirty.mana() && dirty.status());
    }

    #[test]
    fn a_turn_marks_facing_and_facing_the_same_way_does_not() {
        let (mut map, key) = a_player_alone();

        map.agent_mut(key).unwrap().set_facing(Facing::South);
        assert!(!map.delta().agent(key).facing());

        map.agent_mut(key).unwrap().set_facing(Facing::North);
        assert!(map.delta().agent(key).facing());
    }

    #[test]
    fn base_speed_marks_speed() {
        let (mut map, key) = a_player_alone();

        map.agent_mut(key).unwrap().set_base_speed(300);

        assert!(map.delta().agent(key).speed());
    }

    #[test]
    fn a_condition_that_changes_status_and_speed_marks_both() {
        let (mut map, key) = a_player_alone();

        map.agent_mut(key)
            .unwrap()
            .conditions(|c| c.apply_speed(SpeedEffect::Haste, 30));

        let dirty = map.delta().agent(key);
        assert!(dirty.status() && dirty.speed());
    }

    #[test]
    fn a_condition_write_that_changes_nothing_marks_nothing() {
        let (mut map, key) = a_player_alone();

        let cured = map
            .agent_mut(key)
            .unwrap()
            .conditions(|c| c.cure(CombatElement::Fire));

        assert!(!cured);
        assert!(map.delta().is_empty());
    }

    #[test]
    fn bookkeeping_writes_mark_nothing() {
        let (mut map, key) = a_player_alone();

        let mut agent = map.agent_mut(key).unwrap();
        agent.stamp_walk(Tick(5));
        agent.stamp_use(Tick(5));
        agent.stamp_auto_attack(Tick(5));
        agent.set_target(None, 0);
        agent.record_damage(key, 3);

        assert!(map.delta().is_empty());
        assert_eq!(map.get_agent(key).unwrap().next_walk_tick, Tick(5));
        assert_eq!(map.get_agent(key).unwrap().next_use_tick, Tick(5));
    }

    fn a_player_with_a_backpack() -> (WorldMap, AgentKey) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(
                Agent::from_player(a_player_with_a_full_backpack(1, 1)),
                &pos,
            )
            .unwrap();
        (WorldMap::new(map), key)
    }

    fn backpack(map: &WorldMap, key: AgentKey) -> &Item {
        map.get_player(key)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap()
    }

    fn boots(speed: i16) -> Item {
        Item::new(
            config(3079, HashSet::new(), vec![ItemAttribute::Speed(speed)]),
            1,
        )
    }

    #[test]
    fn a_skill_write_marks_that_skill_only() {
        let (mut map, key) = a_player_alone();

        map.player_mut(key)
            .unwrap()
            .skill_mut(SkillType::Level)
            .unwrap()
            .current_ticks += 1;

        assert_eq!(
            map.delta().agent(key).skills().collect::<Vec<_>>(),
            vec![SkillType::Level]
        );
    }

    #[test]
    fn a_skill_the_player_lacks_marks_nothing() {
        let (mut map, key) = a_player_alone();

        assert!(
            map.player_mut(key)
                .unwrap()
                .skill_mut(SkillType::Axe)
                .is_none()
        );

        assert!(map.delta().is_empty());
    }

    #[test]
    fn capacity_marks_capacity() {
        let (mut map, key) = a_player_alone();

        map.player_mut(key).unwrap().set_capacity(50000);

        assert!(map.delta().agent(key).capacity());
    }

    #[test]
    fn equipping_a_slot_marks_the_slot_and_capacity() {
        let (mut map, key) = a_player_alone();

        map.player_mut(key)
            .unwrap()
            .inventory_mut()
            .insert(InventorySlot::Head, None, coins(1))
            .unwrap();

        assert_eq!(
            map.delta().slots_of(key).collect::<Vec<_>>(),
            vec![InventorySlot::Head]
        );
        assert!(map.delta().agent(key).capacity());
        assert!(!map.delta().agent(key).speed());
    }

    #[test]
    fn equipment_that_changes_speed_marks_speed() {
        let (mut map, key) = a_player_alone();

        map.player_mut(key)
            .unwrap()
            .inventory_mut()
            .insert(InventorySlot::Feet, None, boots(20))
            .unwrap();

        assert!(map.delta().agent(key).speed());
    }

    #[test]
    fn inserting_into_a_carried_container_marks_the_container_not_the_slot() {
        let (mut map, key) = a_player_with_a_backpack();
        let guid = backpack(&map, key).guid;

        map.player_mut(key)
            .unwrap()
            .inventory_mut()
            .insert(InventorySlot::Backpack, Some((&guid, 0)), coins(1))
            .unwrap();

        assert!(map.delta().container_dirty(&guid));
        assert_eq!(map.delta().slots_of(key).count(), 0);
        assert!(map.delta().agent(key).capacity());
    }

    #[test]
    fn removing_a_nested_item_marks_the_container_it_came_out_of() {
        let (mut map, key) = a_player_with_a_backpack();
        let pouch = &backpack(&map, key).content.as_ref().unwrap()[0];
        let (pouch_guid, coin_guid) = (pouch.guid, pouch.content.as_ref().unwrap()[0].guid);

        map.player_mut(key)
            .unwrap()
            .inventory_mut()
            .remove(InventorySlot::Backpack, &coin_guid, 1)
            .unwrap();

        assert!(map.delta().container_dirty(&pouch_guid));
        assert_eq!(map.delta().slots_of(key).count(), 0);
    }

    #[test]
    fn taking_a_slot_marks_the_slot() {
        let (mut map, key) = a_player_with_a_backpack();

        map.player_mut(key)
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Backpack)
            .unwrap();

        assert_eq!(
            map.delta().slots_of(key).collect::<Vec<_>>(),
            vec![InventorySlot::Backpack]
        );
    }

    #[test]
    fn a_failed_insert_marks_nothing() {
        let (mut map, key) = a_player_with_a_backpack();
        let missing = ItemGuid(u64::MAX); // minted by nothing

        let result = map.player_mut(key).unwrap().inventory_mut().insert(
            InventorySlot::Backpack,
            Some((&missing, 0)),
            coins(1),
        );

        assert!(result.is_err());
        assert!(map.delta().is_empty());
    }

    #[test]
    fn the_player_half_of_an_agent_guard_marks_the_same_agent() {
        let (mut map, key) = a_player_alone();

        let mut agent = map.agent_mut(key).unwrap();
        agent.player_mut().unwrap().set_capacity(1);
        agent.set_base_speed(300);

        let dirty = map.delta().agent(key);
        assert!(dirty.capacity() && dirty.speed());
    }

    #[test]
    fn stacking_onto_a_tile_marks_the_tile() {
        let (mut map, pos) = one_tile();
        map.inner_mut()
            .place_item(&pos, None, None, coins(10))
            .unwrap();
        let mut incoming = coins(5);

        let moved = map.stack_onto(&ItemPlacement::Map(pos.clone()), &mut incoming);

        assert_eq!((moved, incoming.amount), (5, 0));
        assert_eq!(map.get_top_item(&pos).unwrap().amount, 15);
        assert!(map.delta().tile_dirty(&pos));
    }

    #[test]
    fn stacking_into_a_container_on_a_tile_marks_the_container() {
        let (mut map, pos) = one_tile();
        let mut bag = a_bag();
        bag.content.as_mut().unwrap().push(coins(10));
        let bag_guid = bag.guid;
        map.inner_mut().place_item(&pos, None, None, bag).unwrap();
        let placement = ItemPlacement::Container {
            guid: bag_guid,
            within: Box::new(ItemPlacement::Map(pos.clone())),
            index: 0,
        };

        let moved = map.stack_onto(&placement, &mut coins(5));

        assert_eq!(moved, 5);
        assert!(map.delta().container_dirty(&bag_guid));
        assert!(!map.delta().tile_dirty(&pos));
    }

    #[test]
    fn stacking_into_a_carried_container_marks_it_and_carries_the_weight() {
        let (mut map, key) = a_player_with_a_backpack();
        let pouch_guid = backpack(&map, key).content.as_ref().unwrap()[0].guid;
        let weight = map.get_player(key).unwrap().inventory().carried_weight();
        let placement = ItemPlacement::Container {
            guid: pouch_guid,
            within: Box::new(ItemPlacement::Inventory(InventorySlot::Backpack, key)),
            index: 0,
        };

        let moved = map.stack_onto(&placement, &mut coins(5));

        assert_eq!(moved, 5);
        assert_eq!(
            map.get_player(key).unwrap().inventory().carried_weight(),
            weight + 5
        );
        assert!(map.delta().container_dirty(&pouch_guid));
        assert!(map.delta().agent(key).capacity());
    }

    #[test]
    fn nothing_to_stack_onto_moves_nothing_and_marks_nothing() {
        let (mut map, pos) = one_tile();

        let moved = map.stack_onto(&ItemPlacement::Map(pos), &mut coins(5));

        assert_eq!(moved, 0);
        assert!(map.delta().is_empty());
    }

    #[test]
    fn a_stack_tops_up_to_its_maximum_and_no_further() {
        let mut stack = coins(98);
        let mut incoming = coins(5);

        assert!(stack.stacks_with(&incoming));
        assert_eq!(stack.top_up_from(&mut incoming), 2);
        assert_eq!((stack.amount, incoming.amount), (100, 3));
        assert!(!stack.stacks_with(&incoming));
    }
}

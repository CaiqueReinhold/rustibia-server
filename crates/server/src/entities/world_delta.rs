use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;
use strum::IntoEnumIterator;

use crate::entities::agent::AgentKey;
use crate::entities::inventory::InventorySlot;
use crate::entities::items::ItemGuid;
use crate::entities::map::ChunkCoord;
use crate::entities::position::{Position, Rect};
use crate::entities::skills::SkillType;

#[derive(Clone, Copy)]
pub(in crate::entities) enum AgentFlag {
    Life = 1 << 0,
    Mana = 1 << 1,
    Speed = 1 << 2,
    Status = 1 << 3,
    Capacity = 1 << 4,
    Facing = 1 << 5,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AgentDirty {
    flags: u8,
    skills: u8,
}

fn skill_bit(skill: SkillType) -> u8 {
    1 << skill as u8
}

impl AgentDirty {
    fn has(self, flag: AgentFlag) -> bool {
        self.flags & flag as u8 != 0
    }

    pub fn life(self) -> bool {
        self.has(AgentFlag::Life)
    }

    pub fn mana(self) -> bool {
        self.has(AgentFlag::Mana)
    }

    pub fn speed(self) -> bool {
        self.has(AgentFlag::Speed)
    }

    pub fn status(self) -> bool {
        self.has(AgentFlag::Status)
    }

    pub fn capacity(self) -> bool {
        self.has(AgentFlag::Capacity)
    }

    pub fn facing(self) -> bool {
        self.has(AgentFlag::Facing)
    }

    pub fn skills(self) -> impl Iterator<Item = SkillType> {
        SkillType::iter().filter(move |skill| self.skills & skill_bit(*skill) != 0)
    }
}

#[derive(Default, Debug)]
pub struct WorldDelta {
    tiles: HashMap<ChunkCoord, SmallVec<[Position; 4]>>,
    agents: HashMap<AgentKey, AgentDirty>,
    slots: HashSet<(AgentKey, InventorySlot)>,
    containers: HashSet<ItemGuid>,
}

impl WorldDelta {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
            && self.agents.is_empty()
            && self.slots.is_empty()
            && self.containers.is_empty()
    }

    pub fn tiles_in<'a, 'b>(
        &'a self,
        rect: &'b Rect,
        floors: &'b [u8],
    ) -> impl Iterator<Item = &'a Position> + use<'a, 'b> {
        self.tiles
            .iter()
            .filter(move |(chunk, _)| chunk.overlaps(rect, floors))
            .flat_map(|(_, positions)| positions.iter())
            .filter(move |position| rect.contains(position))
    }

    pub fn agents(&self) -> impl Iterator<Item = (AgentKey, AgentDirty)> + '_ {
        self.agents.iter().map(|(key, dirty)| (*key, *dirty))
    }

    #[cfg(test)]
    pub fn agent(&self, key: AgentKey) -> AgentDirty {
        self.agents.get(&key).copied().unwrap_or_default()
    }

    pub fn slots_of(&self, key: AgentKey) -> impl Iterator<Item = InventorySlot> + '_ {
        self.slots
            .iter()
            .filter(move |(owner, _)| *owner == key)
            .map(|(_, slot)| *slot)
    }

    pub fn container_dirty(&self, guid: &ItemGuid) -> bool {
        self.containers.contains(guid)
    }

    #[cfg(test)]
    pub fn tile_dirty(&self, pos: &Position) -> bool {
        self.tiles
            .get(&ChunkCoord::from_pos(pos))
            .is_some_and(|positions| positions.contains(pos))
    }

    pub(in crate::entities) fn mark_tile(&mut self, pos: &Position) {
        let positions = self.tiles.entry(ChunkCoord::from_pos(pos)).or_default();
        if !positions.contains(pos) {
            positions.push(pos.clone());
        }
    }

    pub(in crate::entities) fn mark_agent(&mut self, key: AgentKey, flag: AgentFlag) {
        self.agents.entry(key).or_default().flags |= flag as u8;
    }

    pub(in crate::entities) fn mark_skill(&mut self, key: AgentKey, skill: SkillType) {
        self.agents.entry(key).or_default().skills |= skill_bit(skill);
    }

    pub(in crate::entities) fn mark_slot(&mut self, key: AgentKey, slot: InventorySlot) {
        self.slots.insert((key, slot));
    }

    pub(in crate::entities) fn mark_container(&mut self, guid: &ItemGuid) {
        if !self.containers.contains(guid) {
            self.containers.insert(guid.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::KeyData;

    fn key(n: u64) -> AgentKey {
        AgentKey::from(KeyData::from_ffi((1 << 32) | n))
    }

    #[test]
    fn a_tile_marked_twice_is_listed_once() {
        let mut delta = WorldDelta::default();
        let pos = Position::new(10, 10, 7);

        delta.mark_tile(&pos);
        delta.mark_tile(&pos);

        let rect = Rect::player_viewport(&pos);
        assert_eq!(delta.tiles_in(&rect, &[7]).count(), 1);
    }

    #[test]
    fn tiles_in_keeps_only_the_rect_and_the_floors_asked_for() {
        let mut delta = WorldDelta::default();
        let centre = Position::new(100, 100, 7);
        let inside = Position::new(101, 100, 7);
        let outside = Position::new(140, 100, 7);
        let other_floor = Position::new(101, 100, 10);
        for pos in [&inside, &outside, &other_floor] {
            delta.mark_tile(pos);
        }

        let rect = Rect::player_viewport(&centre);
        let found: Vec<&Position> = delta.tiles_in(&rect, &[7]).collect();

        assert_eq!(found, vec![&inside]);
    }

    #[test]
    fn agent_flags_and_skills_accumulate_independently() {
        let mut delta = WorldDelta::default();

        delta.mark_agent(key(1), AgentFlag::Life);
        delta.mark_agent(key(1), AgentFlag::Facing);
        delta.mark_skill(key(1), SkillType::Sword);
        delta.mark_agent(key(2), AgentFlag::Mana);

        let first = delta.agent(key(1));
        assert!(first.life() && first.facing());
        assert!(!first.mana() && !first.speed() && !first.status() && !first.capacity());
        assert_eq!(first.skills().collect::<Vec<_>>(), vec![SkillType::Sword]);
        assert!(delta.agent(key(2)).mana());
        assert_eq!(delta.agent(key(3)), AgentDirty::default());
    }

    #[test]
    fn slots_of_lists_only_that_agents_slots() {
        let mut delta = WorldDelta::default();

        delta.mark_slot(key(1), InventorySlot::Head);
        delta.mark_slot(key(2), InventorySlot::Feet);

        assert_eq!(
            delta.slots_of(key(1)).collect::<Vec<_>>(),
            vec![InventorySlot::Head]
        );
    }

    #[test]
    fn a_fresh_delta_is_empty_and_a_marked_one_is_not() {
        let mut delta = WorldDelta::default();
        assert!(delta.is_empty());

        delta.mark_container(&ItemGuid("bag".to_owned()));

        assert!(!delta.is_empty());
        assert!(delta.container_dirty(&ItemGuid("bag".to_owned())));
    }
}

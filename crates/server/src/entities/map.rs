use slotmap::SlotMap;
use smallvec::SmallVec;
use std::{collections::HashMap, ops::RangeInclusive, sync::Arc};
use thiserror::Error;

use crate::constants::view::MAX_VISIBLE_ITEMS;
use crate::entities::agent::{Agent, AgentKey};
use crate::entities::items::{FloorChangeDirection, Item, ItemFlag, ItemGuid};
use crate::entities::player::Player;
use crate::entities::position::{Position, Rect};

pub type RemovedItem = (Item, Option<usize>, Option<(ItemGuid, usize)>);

#[derive(Debug, Clone)]
pub struct MapTile {
    items: SmallVec<[Item; MAX_VISIBLE_ITEMS]>,
    agents: SmallVec<[AgentKey; 1]>,
}

#[derive(Error, Debug)]
pub enum MapError {
    #[error("Tile position does not exist")]
    TileDoesNotExist,
    #[error("Entity does not exist at this position")]
    EntityNotInPosition,
    #[error("Container is full")]
    ContainerIsFull,
}

const CHUNK_BITS: u16 = 4;
const CHUNK_SIDE: u16 = 1 << CHUNK_BITS;
const CHUNK_MASK: u16 = CHUNK_SIDE - 1;
const CHUNK_AREA: usize = (CHUNK_SIDE as usize) * (CHUNK_SIDE as usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::entities) struct ChunkCoord {
    cx: u16,
    cy: u16,
    z: u8,
}

impl ChunkCoord {
    pub(in crate::entities) fn from_pos(pos: &Position) -> Self {
        ChunkCoord {
            cx: pos.x >> CHUNK_BITS,
            cy: pos.y >> CHUNK_BITS,
            z: pos.z,
        }
    }

    pub(in crate::entities) fn overlaps(&self, rect: &Rect, floors: &[u8]) -> bool {
        floors.contains(&self.z)
            && ((rect.min_x() >> CHUNK_BITS)..=(rect.max_x() >> CHUNK_BITS)).contains(&self.cx)
            && ((rect.min_y() >> CHUNK_BITS)..=(rect.max_y() >> CHUNK_BITS)).contains(&self.cy)
    }
}

fn local_index_of(lx: u16, ly: u16) -> usize {
    ly as usize * CHUNK_SIDE as usize + lx as usize
}

fn local_index(pos: &Position) -> usize {
    local_index_of(pos.x & CHUNK_MASK, pos.y & CHUNK_MASK)
}

#[derive(Debug, Clone)]
struct Chunk {
    tiles: Box<[Option<MapTile>]>,
}

impl Chunk {
    fn new() -> Self {
        let tiles = (0..CHUNK_AREA)
            .map(|_| None)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Chunk { tiles }
    }
}

/// One chunk's share of a rect, in chunk-local coordinates.
struct ChunkSlice<'a> {
    chunk: Option<&'a Chunk>,
    base_x: u16,
    base_y: u16,
    z: u8,
    lx: RangeInclusive<u16>,
    ly: RangeInclusive<u16>,
}

impl<'a> ChunkSlice<'a> {
    fn tiles(self) -> impl Iterator<Item = (Position, Option<&'a MapTile>)> {
        let ChunkSlice {
            chunk,
            base_x,
            base_y,
            z,
            lx,
            ly,
        } = self;
        ly.flat_map(move |ly| {
            lx.clone().map(move |lx| {
                let tile = chunk.and_then(|chunk| chunk.tiles[local_index_of(lx, ly)].as_ref());
                (Position::new(base_x + lx, base_y + ly, z), tile)
            })
        })
    }
}

#[derive(Debug, Clone)]
pub struct GameMap {
    chunks: HashMap<ChunkCoord, Arc<Chunk>>,
    agents: SlotMap<AgentKey, Agent>,
    agent_positions: HashMap<AgentKey, Position>,
}

impl MapTile {
    pub fn new() -> Self {
        MapTile {
            items: SmallVec::new(),
            agents: SmallVec::new(),
        }
    }

    pub fn push_item(&mut self, item: Item) {
        self.items.push(item);
    }

    pub fn visible_items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().take(MAX_VISIBLE_ITEMS)
    }
}

impl GameMap {
    pub fn new() -> Self {
        GameMap {
            chunks: HashMap::new(),
            agents: SlotMap::with_key(),
            agent_positions: HashMap::new(),
        }
    }

    pub fn insert_tile(&mut self, pos: Position, tile: MapTile) {
        let coord = ChunkCoord::from_pos(&pos);
        let idx = local_index(&pos);
        let chunk = self
            .chunks
            .entry(coord)
            .or_insert_with(|| Arc::new(Chunk::new()));
        Arc::make_mut(chunk).tiles[idx] = Some(tile);
    }

    pub fn contains_tile(&self, pos: &Position) -> bool {
        self.get_tile(pos).is_ok()
    }

    fn get_tile_mut(&mut self, pos: &Position) -> Result<&mut MapTile, MapError> {
        let idx = local_index(pos);
        let chunk = self
            .chunks
            .get_mut(&ChunkCoord::from_pos(pos))
            .ok_or(MapError::TileDoesNotExist)?;
        Arc::make_mut(chunk).tiles[idx]
            .as_mut()
            .ok_or(MapError::TileDoesNotExist)
    }

    pub fn get_tile(&self, pos: &Position) -> Result<&MapTile, MapError> {
        self.chunks
            .get(&ChunkCoord::from_pos(pos))
            .and_then(|chunk| chunk.tiles[local_index(pos)].as_ref())
            .ok_or(MapError::TileDoesNotExist)
    }

    pub fn iter_items(&self, pos: &Position) -> Result<impl Iterator<Item = &Item>, MapError> {
        let tile = self.get_tile(pos)?;
        Ok(tile.items.iter())
    }

    pub fn iter_items_mut(
        &mut self,
        pos: &Position,
    ) -> Result<impl Iterator<Item = &mut Item>, MapError> {
        let tile = self.get_tile_mut(pos)?;
        Ok(tile.items.iter_mut())
    }

    pub fn insert_agent(&mut self, agent: Agent, pos: &Position) -> Result<AgentKey, MapError> {
        if !self.contains_tile(pos) {
            return Err(MapError::TileDoesNotExist);
        }
        let key = self.agents.insert(agent);
        self.get_tile_mut(pos).unwrap().agents.push(key);
        self.agent_positions.insert(key, pos.clone());
        Ok(key)
    }

    pub fn remove_agent(&mut self, key: AgentKey) -> Option<(Agent, Position)> {
        let pos = self.agent_positions.remove(&key)?;

        if let Ok(tile) = self.get_tile_mut(&pos)
            && let Some(idx) = tile.agents.iter().position(|k| *k == key)
        {
            tile.agents.remove(idx);
        }
        self.agents.remove(key).map(|agent| (agent, pos))
    }

    pub fn move_agent(&mut self, key: AgentKey, new_pos: &Position) -> Result<(), MapError> {
        let old_pos = self
            .agent_positions
            .get(&key)
            .cloned()
            .ok_or(MapError::EntityNotInPosition)?;
        let old_tile = self.get_tile_mut(&old_pos)?;
        if let Some(idx) = old_tile.agents.iter().position(|k| *k == key) {
            old_tile.agents.remove(idx);
        }
        let new_tile = self.get_tile_mut(new_pos)?;
        new_tile.agents.push(key);
        self.agent_positions.insert(key, new_pos.clone());
        Ok(())
    }

    pub fn agent_position(&self, key: AgentKey) -> Option<&Position> {
        self.agent_positions.get(&key)
    }

    pub fn get_agent(&self, key: AgentKey) -> Option<&Agent> {
        self.agents.get(key)
    }

    pub fn get_agent_mut(&mut self, key: AgentKey) -> Option<&mut Agent> {
        self.agents.get_mut(key)
    }

    pub fn get_player(&self, key: AgentKey) -> Option<&Player> {
        self.agents.get(key)?.get_player()
    }

    pub fn get_player_mut(&mut self, key: AgentKey) -> Option<&mut Player> {
        self.agents.get_mut(key)?.get_player_mut()
    }

    pub fn iter_agents_at(
        &self,
        pos: &Position,
    ) -> Result<impl Iterator<Item = &AgentKey> + '_, MapError> {
        let tile = self.get_tile(pos)?;
        Ok(tile.agents.iter())
    }

    /// Splits `rect` into the chunks it covers, each clamped to its own bounds. A chunk the map
    /// does not hold is still yielded, because a caller filling a fixed grid needs its holes.
    fn iter_chunks_in_rect<'a>(
        &'a self,
        rect: &Rect,
        z: u8,
    ) -> impl Iterator<Item = ChunkSlice<'a>> + use<'a> {
        let (x0, y0) = (rect.min_x(), rect.min_y());
        let (x1, y1) = (rect.max_x(), rect.max_y());
        let cx_range = (x0 >> CHUNK_BITS)..=(x1 >> CHUNK_BITS);
        let cy_range = (y0 >> CHUNK_BITS)..=(y1 >> CHUNK_BITS);

        cy_range
            .flat_map(move |cy| cx_range.clone().map(move |cx| (cx, cy)))
            .map(move |(cx, cy)| {
                let base_x = cx << CHUNK_BITS;
                let base_y = cy << CHUNK_BITS;
                ChunkSlice {
                    chunk: self.chunks.get(&ChunkCoord { cx, cy, z }).map(Arc::as_ref),
                    base_x,
                    base_y,
                    z,
                    lx: (x0.max(base_x) - base_x)..=(x1.min(base_x + CHUNK_MASK) - base_x),
                    ly: (y0.max(base_y) - base_y)..=(y1.min(base_y + CHUNK_MASK) - base_y),
                }
            })
    }

    pub fn iter_agents_in_rect<'a>(
        &'a self,
        rect: &Rect,
        z: u8,
    ) -> impl Iterator<Item = (AgentKey, Position)> + use<'a> {
        self.iter_chunks_in_rect(rect, z)
            .filter(|slice| slice.chunk.is_some())
            .flat_map(ChunkSlice::tiles)
            .filter_map(|(pos, tile)| Some((pos, tile?)))
            .flat_map(|(pos, tile)| tile.agents.iter().map(move |key| (*key, pos.clone())))
    }

    pub fn iter_tiles_in_rect<'a>(
        &'a self,
        rect: &Rect,
        z: u8,
    ) -> impl Iterator<Item = (Position, Option<&'a MapTile>)> + use<'a> {
        self.iter_chunks_in_rect(rect, z)
            .flat_map(ChunkSlice::tiles)
    }

    pub fn iter_agents(&self) -> impl Iterator<Item = (AgentKey, &Agent)> {
        self.agents.iter()
    }

    pub fn can_move(&self, pos: &Position, agent_key: AgentKey) -> bool {
        let tile = self.get_tile(pos);
        if tile.is_err() {
            return false;
        }
        let tile = tile.unwrap();

        let has_ground = tile
            .items
            .iter()
            .any(|i| i.config.has_flag(ItemFlag::Ground));
        if !has_ground {
            return false;
        }

        let unpass = tile
            .items
            .iter()
            .any(|i| i.config.has_flag(ItemFlag::Unpass));
        if unpass {
            return false;
        }

        if !tile.agents.is_empty() {
            return false;
        }

        if let Some(agent) = self.get_agent(agent_key)
            && agent.is_creature()
        {
            if self.get_floor_change(pos).is_some() {
                return false;
            }

            let avoid = tile
                .items
                .iter()
                .any(|i| i.config.has_flag(ItemFlag::Avoid));
            if avoid {
                return false;
            }
        }

        true
    }

    pub fn has_sight(&self, pos: &Position) -> bool {
        self.get_tile(pos)
            .ok()
            .map(|tile| {
                tile.items
                    .iter()
                    .find(|it| it.config.has_flag(ItemFlag::Unpass))
                    .is_none()
            })
            .unwrap_or(true)
    }

    pub fn tile_friction(&self, pos: &Position) -> Option<u16> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };
        tile.items
            .iter()
            .find_map(|i| i.config.attr_tile_friction())
    }

    pub fn get_floor_change(&self, pos: &Position) -> Option<FloorChangeDirection> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };
        tile.items
            .iter()
            .find_map(|it| it.config.attr_floor_change())
    }

    pub fn get_top_item(&self, pos: &Position) -> Option<&Item> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };
        tile.items.last()
    }

    pub fn get_item_at(&self, pos: &Position, index: usize) -> Option<&Item> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };
        tile.items.get(index)
    }

    pub fn can_drop_item(&self, pos: &Position) -> bool {
        let Ok(tile) = self.get_tile(pos) else {
            return false;
        };
        tile.items
            .iter()
            .any(|i| i.config.has_flag(ItemFlag::FullBank))
            && !tile
                .items
                .iter()
                .any(|i| i.config.has_flag(ItemFlag::Bottom))
    }

    pub fn remove_item_from_tile(
        &mut self,
        pos: &Position,
        guid: &ItemGuid,
        amount: u8,
    ) -> Option<RemovedItem> {
        let tile = self.get_tile_mut(pos).ok()?;

        if let Some(idx) = tile.items.iter().position(|i| i.guid == *guid) {
            let held = tile.items[idx].amount;
            return match held.cmp(&amount) {
                std::cmp::Ordering::Greater => {
                    Some((tile.items[idx].split_off(amount), Some(idx), None))
                }
                std::cmp::Ordering::Equal => Some((tile.items.remove(idx), Some(idx), None)),
                std::cmp::Ordering::Less => None,
            };
        }

        tile.items
            .iter_mut()
            .find_map(|item| item.remove_nested(guid, amount))
            .map(|(removed, parent)| (removed, None, Some(parent)))
    }

    /// Place `item` at `pos`.
    ///
    /// - `container`: if `None`, pushes directly onto the tile.
    /// - `container`: if `Some((guid, slot))`, finds that container on the tile
    ///   and inserts the item at `slot` within it.
    pub fn place_item(
        &mut self,
        pos: &Position,
        index: Option<usize>,
        container: Option<(&ItemGuid, usize)>,
        item: Item,
    ) -> Result<&Item, MapError> {
        match container {
            None => {
                let tile = self.get_tile_mut(pos)?;
                let index = index.unwrap_or(tile.items.len());
                tile.items.insert(index, item);
                Ok(&tile.items[index])
            }
            Some((target, slot)) => {
                let tile = self.get_tile_mut(pos)?;
                for existing_item in &mut tile.items {
                    if let Some(c) = existing_item.find_by_guid_mut(target) {
                        let cap = c.config.attr_capacity().unwrap();
                        if let Some(content) = &mut c.content {
                            if content.len() >= cap as usize {
                                return Err(MapError::ContainerIsFull);
                            }
                            content.insert(slot, item);
                            return Ok(&content[slot]);
                        }
                    }
                }
                Err(MapError::EntityNotInPosition)
            }
        }
    }

    pub fn get_parent_container(&self, pos: &Position, guid: &ItemGuid) -> Option<&ItemGuid> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };

        for it in tile.items.iter() {
            if let Some((parent_guid, _)) = Self::find_by_id_inner(it, guid, None) {
                return parent_guid;
            }
        }
        None
    }

    pub fn get_item_by_id(&self, pos: &Position, guid: &ItemGuid) -> Option<&Item> {
        let Ok(tile) = self.get_tile(pos) else {
            return None;
        };
        tile.items.iter().find_map(|it| it.find_by_guid(guid))
    }

    fn find_by_id_inner<'a>(
        item: &'a Item,
        guid: &ItemGuid,
        parent_guid: Option<&'a ItemGuid>,
    ) -> Option<(Option<&'a ItemGuid>, &'a Item)> {
        if item.guid == *guid {
            return Some((parent_guid, item));
        }
        if let Some(content) = &item.content {
            for inner in content {
                if let Some(found) = Self::find_by_id_inner(inner, guid, Some(&item.guid)) {
                    return Some(found);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::inventory::InventorySlot;
    use crate::entities::items::ItemId;
    use crate::entities::items::{ItemAttribute, ItemConfig};
    use crate::entities::position::Position;
    use crate::entities::skills::{SkillType, SkillValue};
    use crate::persistence::test_fixtures::{a_creature_kind, a_player_with_a_full_backpack};
    use std::collections::HashSet;

    fn new_creature() -> Agent {
        Agent::from_creature_kind(
            Arc::new(a_creature_kind("Creature")),
            Position::new(1028, 128, 7),
        )
    }

    fn map_with_one_tile(pos: &Position) -> GameMap {
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        map
    }

    fn a_stack_of(amount: u8) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2148),
                "gold coin".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Cumulative, ItemFlag::Take]),
                Vec::new(),
            )),
            amount,
        )
    }

    fn a_bag() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1987),
                "bag".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Container]),
                vec![ItemAttribute::Capacity(8)],
            )),
            1,
        )
    }

    /// The `Option<(ItemGuid, usize)>` a removal returns is the *container* the item came
    /// out of: callers re-insert into that guid to roll a failed move back, and `WorldMap`
    /// marks it as the container to refresh.
    #[test]
    fn removing_part_of_a_stack_names_the_container_it_came_from() {
        let pos = Position::new(10, 10, 7);
        let mut map = map_with_one_tile(&pos);
        let bag = a_bag();
        let bag_guid = bag.guid.clone();
        map.place_item(&pos, None, None, bag).unwrap();
        let stack = a_stack_of(50);
        let stack_guid = stack.guid.clone();
        map.place_item(&pos, None, Some((&bag_guid, 0)), stack)
            .unwrap();

        let (removed, tile_index, parent) =
            map.remove_item_from_tile(&pos, &stack_guid, 20).unwrap();

        assert_eq!(removed.amount, 20);
        assert_eq!(
            tile_index, None,
            "the stack was in the bag, not on the tile"
        );
        assert_eq!(parent, Some((bag_guid, 0)));
    }

    /// The whole-stack branch already did this; the two must not disagree.
    #[test]
    fn removing_a_whole_stack_names_the_same_container() {
        let pos = Position::new(10, 10, 7);
        let mut map = map_with_one_tile(&pos);
        let bag = a_bag();
        let bag_guid = bag.guid.clone();
        map.place_item(&pos, None, None, bag).unwrap();
        let stack = a_stack_of(20);
        let stack_guid = stack.guid.clone();
        map.place_item(&pos, None, Some((&bag_guid, 0)), stack)
            .unwrap();

        let (_, _, parent) = map.remove_item_from_tile(&pos, &stack_guid, 20).unwrap();

        assert_eq!(parent, Some((bag_guid, 0)));
    }

    #[test]
    fn removing_from_a_bag_inside_a_bag_names_the_inner_one() {
        let pos = Position::new(10, 10, 7);
        let mut map = map_with_one_tile(&pos);
        let outer = a_bag();
        let outer_guid = outer.guid.clone();
        map.place_item(&pos, None, None, outer).unwrap();
        let inner = a_bag();
        let inner_guid = inner.guid.clone();
        map.place_item(&pos, None, Some((&outer_guid, 0)), inner)
            .unwrap();
        let stack = a_stack_of(50);
        let stack_guid = stack.guid.clone();
        map.place_item(&pos, None, Some((&inner_guid, 0)), stack)
            .unwrap();

        let (removed, tile_index, parent) =
            map.remove_item_from_tile(&pos, &stack_guid, 20).unwrap();

        assert_eq!(removed.amount, 20);
        assert_eq!(tile_index, None);
        assert_eq!(parent, Some((inner_guid, 0)));
    }

    #[test]
    fn removing_more_than_a_stack_holds_removes_nothing() {
        let pos = Position::new(10, 10, 7);
        let mut map = map_with_one_tile(&pos);
        let stack = a_stack_of(5);
        let stack_guid = stack.guid.clone();
        map.place_item(&pos, None, None, stack).unwrap();

        assert!(map.remove_item_from_tile(&pos, &stack_guid, 20).is_none());
        assert_eq!(
            map.get_item_by_id(&pos, &stack_guid).unwrap().amount,
            5,
            "a refused removal must leave the stack untouched"
        );
    }

    fn map_with_players(count: u32) -> (GameMap, Vec<AgentKey>) {
        let mut map = GameMap::new();
        let mut keys = Vec::new();
        for i in 0..count {
            let pos = Position::new(100 + i as u16, 100, 7);
            map.insert_tile(pos.clone(), MapTile::new());
            keys.push(
                map.insert_agent(
                    Agent::from_player(a_player_with_a_full_backpack(i, 1)),
                    &pos,
                )
                .unwrap(),
            );
        }
        (map, keys)
    }

    #[test]
    fn a_map_clone_shares_the_inventories_no_one_wrote_to() {
        let (mut map, keys) = map_with_players(2);
        let snapshot = map.clone();

        map.get_player_mut(keys[0])
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Backpack);

        assert!(!std::ptr::eq(
            map.get_player(keys[0]).unwrap().inventory(),
            snapshot.get_player(keys[0]).unwrap().inventory()
        ));
        assert!(std::ptr::eq(
            map.get_player(keys[1]).unwrap().inventory(),
            snapshot.get_player(keys[1]).unwrap().inventory()
        ));
    }

    #[test]
    fn a_map_clone_does_not_see_later_inventory_writes() {
        let (mut map, keys) = map_with_players(1);
        let snapshot = map.clone();

        map.get_player_mut(keys[0])
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Backpack);

        assert!(
            snapshot
                .get_player(keys[0])
                .unwrap()
                .inventory()
                .get(&InventorySlot::Backpack)
                .is_some(),
            "the snapshot lost the backpack the live map removed"
        );
        assert!(
            map.get_player(keys[0])
                .unwrap()
                .inventory()
                .get(&InventorySlot::Backpack)
                .is_none()
        );
    }

    #[test]
    fn a_map_clone_does_not_see_later_player_field_writes() {
        let (mut map, keys) = map_with_players(1);
        let snapshot = map.clone();

        map.get_player_mut(keys[0]).unwrap().skills_mut().insert(
            SkillType::Level,
            SkillValue {
                value: 7,
                current_ticks: 0,
            },
        );

        assert_eq!(snapshot.get_player(keys[0]).unwrap().level(), 1);
        assert_eq!(map.get_player(keys[0]).unwrap().level(), 7);
    }

    #[test]
    #[ignore = "timing, not a pass/fail assertion"]
    fn player_write_cost_on_a_shared_map() {
        const ROUNDS: u32 = 100;
        for count in [1, 10, 50, 200] {
            let (mut map, keys) = map_with_players(count);
            let _snapshot = map.clone();
            let start = std::time::Instant::now();
            for _ in 0..ROUNDS {
                for key in &keys {
                    map.get_player_mut(*key)
                        .unwrap()
                        .skills_mut()
                        .entry(SkillType::Level)
                        .and_modify(|skill| skill.current_ticks += 1);
                }
            }
            let elapsed = start.elapsed();
            println!(
                "{count:>4} players: {:>9.1} us/round of writes",
                elapsed.as_secs_f64() * 1e6 / ROUNDS as f64
            );
        }
    }

    #[test]
    #[ignore = "timing, not a pass/fail assertion"]
    fn map_clone_cost_by_player_count() {
        const ROUNDS: u32 = 100;
        for count in [1, 10, 50, 200] {
            let (map, _) = map_with_players(count);
            let start = std::time::Instant::now();
            let clones: Vec<GameMap> = (0..ROUNDS).map(|_| map.clone()).collect();
            let elapsed = start.elapsed();
            std::hint::black_box(&clones);
            println!(
                "{count:>4} players: {:>9.1} us/clone",
                elapsed.as_secs_f64() * 1e6 / ROUNDS as f64
            );
        }
    }

    #[test]
    fn iter_agents_yields_each_inserted_agent() {
        let pos = Position::new(100, 100, 7);
        let mut map = map_with_one_tile(&pos);
        let k1 = map.insert_agent(new_creature(), &pos).unwrap();
        let k2 = map.insert_agent(new_creature(), &pos).unwrap();
        let keys: Vec<_> = map.iter_agents().map(|(k, _)| k).collect();
        assert!(keys.contains(&k1));
        assert!(keys.contains(&k2));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn insert_agent_fails_when_tile_does_not_exist() {
        let mut map = GameMap::new();
        let pos = Position::new(5, 5, 7);
        assert!(matches!(
            map.insert_agent(new_creature(), &pos),
            Err(MapError::TileDoesNotExist)
        ));
    }

    #[test]
    fn tiles_sharing_a_local_index_across_chunks_do_not_alias() {
        // (0,0) and (CHUNK_SIDE,0) map to the same local index but different chunks.
        let a = Position::new(0, 0, 7);
        let b = Position::new(CHUNK_SIDE, 0, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());

        let key = map.insert_agent(new_creature(), &a).unwrap();
        assert_eq!(
            map.iter_agents_at(&a).unwrap().copied().collect::<Vec<_>>(),
            vec![key]
        );
        // The tile in the neighbouring chunk exists but must be unaffected.
        assert_eq!(map.iter_agents_at(&b).unwrap().count(), 0);
    }

    #[test]
    fn move_agent_across_chunk_boundary() {
        let from = Position::new(CHUNK_SIDE - 1, 10, 7); // last column of chunk 0
        let to = Position::new(CHUNK_SIDE, 10, 7); // first column of chunk 1
        let mut map = GameMap::new();
        map.insert_tile(from.clone(), MapTile::new());
        map.insert_tile(to.clone(), MapTile::new());

        let key = map.insert_agent(new_creature(), &from).unwrap();
        map.move_agent(key, &to).unwrap();

        assert_eq!(map.agent_position(key), Some(&to));
        assert_eq!(map.iter_agents_at(&from).unwrap().count(), 0);
        assert_eq!(map.iter_agents_at(&to).unwrap().count(), 1);
    }

    #[test]
    fn mutation_after_clone_does_not_affect_snapshot() {
        // Validates the copy-on-write property: a published snapshot (a clone)
        // must not observe mutations made to the live map afterwards.
        let pos = Position::new(3, 3, 7);
        let mut map = map_with_one_tile(&pos);

        let snapshot = map.clone();
        map.insert_agent(new_creature(), &pos).unwrap();

        assert_eq!(snapshot.iter_agents_at(&pos).unwrap().count(), 0);
        assert_eq!(map.iter_agents_at(&pos).unwrap().count(), 1);
    }

    #[test]
    fn get_agents_at_rect_collects_across_chunks_and_excludes_outside() {
        let mut map = GameMap::new();
        let a = Position::new(2, 2, 7); // chunk (0, 0)
        let b = Position::new(18, 3, 7); // chunk (1, 0) — across a chunk boundary
        let outside = Position::new(40, 40, 7);
        for p in [&a, &b, &outside] {
            map.insert_tile(p.clone(), MapTile::new());
        }
        let ka = map.insert_agent(new_creature(), &a).unwrap();
        let kb = map.insert_agent(new_creature(), &b).unwrap();
        let ko = map.insert_agent(new_creature(), &outside).unwrap();

        let rect = Rect::new(0, 0, 20, 10);
        let found: Vec<_> = map.iter_agents_in_rect(&rect, 7).map(|(k, _)| k).collect();

        assert_eq!(found.len(), 2);
        assert!(found.contains(&ka));
        assert!(found.contains(&kb));
        assert!(!found.contains(&ko));
    }

    #[test]
    fn get_agents_at_rect_is_floor_scoped() {
        let mut map = GameMap::new();
        let pos = Position::new(5, 5, 7);
        map.insert_tile(pos.clone(), MapTile::new());
        map.insert_agent(new_creature(), &pos).unwrap();

        let rect = Rect::new(0, 0, 15, 15);
        assert_eq!(map.iter_agents_in_rect(&rect, 7).count(), 1);
        assert_eq!(map.iter_agents_in_rect(&rect, 6).count(), 0);
    }

    #[test]
    fn get_agents_at_rect_over_void_is_empty() {
        let map = GameMap::new();
        let rect = Rect::new(0, 0, 100, 100);
        assert_eq!(map.iter_agents_in_rect(&rect, 7).count(), 0);
    }

    #[test]
    fn iter_tiles_in_rect_yields_existing_tiles_with_positions() {
        let mut map = GameMap::new();
        let a = Position::new(2, 2, 7); // chunk (0, 0)
        let b = Position::new(18, 3, 7); // chunk (1, 0) — across a chunk boundary
        let outside = Position::new(40, 40, 7);
        for p in [&a, &b, &outside] {
            map.insert_tile(p.clone(), MapTile::new());
        }

        let rect = Rect::new(0, 0, 20, 10);

        // Every position in the rect is yielded exactly once, regardless of whether
        // its chunk exists — 21 columns * 11 rows.
        assert_eq!(map.iter_tiles_in_rect(&rect, 7).count(), 21 * 11);

        // The positions that actually carry a tile are exactly the in-rect ones
        // (`outside` at (40, 40) falls beyond the rect).
        let mut found: Vec<Position> = map
            .iter_tiles_in_rect(&rect, 7)
            .filter_map(|(pos, tile)| tile.map(|_| pos))
            .collect();
        found.sort();

        assert_eq!(found, vec![a, b]);
    }

    #[test]
    fn iter_tiles_in_rect_is_floor_scoped() {
        let mut map = GameMap::new();
        let pos = Position::new(5, 5, 7);
        map.insert_tile(pos.clone(), MapTile::new());

        let rect = Rect::new(0, 0, 15, 15);

        // Both floors yield the full grid of positions (16 * 16); floor scoping
        // shows up in *which* positions carry a tile, not in the yielded count.
        assert_eq!(map.iter_tiles_in_rect(&rect, 7).count(), 16 * 16);
        assert_eq!(map.iter_tiles_in_rect(&rect, 6).count(), 16 * 16);

        let some_on_7 = map
            .iter_tiles_in_rect(&rect, 7)
            .filter(|(_, tile)| tile.is_some())
            .count();
        let some_on_6 = map
            .iter_tiles_in_rect(&rect, 6)
            .filter(|(_, tile)| tile.is_some())
            .count();
        assert_eq!(some_on_7, 1);
        assert_eq!(some_on_6, 0);
    }

    #[test]
    fn get_agents_at_rect_respects_inclusive_bounds_within_a_chunk() {
        // Exercises the in-chunk clamp: max_x = 10 must include x=10 and exclude x=11.
        let mut map = GameMap::new();
        let inside = Position::new(10, 10, 7);
        let just_outside = Position::new(11, 10, 7);
        map.insert_tile(inside.clone(), MapTile::new());
        map.insert_tile(just_outside.clone(), MapTile::new());
        let ki = map.insert_agent(new_creature(), &inside).unwrap();
        map.insert_agent(new_creature(), &just_outside).unwrap();

        let rect = Rect::new(0, 0, 10, 10);
        let found: Vec<_> = map.iter_agents_in_rect(&rect, 7).map(|(k, _)| k).collect();
        assert_eq!(found, vec![ki]);
    }
}

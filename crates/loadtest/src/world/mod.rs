use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustibia_server::constants::view::PLAYER_VIEWPORT_WIDTH;
use rustibia_server::entities::agent::{AgentId, Pool};
use rustibia_server::entities::inventory::InventorySlot;
use rustibia_server::entities::items::{ContainerId, ItemConfig, ItemFlag, ItemId};
use rustibia_server::entities::position::{Direction, Position};
use rustibia_server::entities::spells::{SpellGroup, SpellId};
use rustibia_server::messages::{ItemStack, ServerMessage};

mod viewport;
use viewport::{describe_map_positions, expansion_positions};

pub type ItemCatalogue = Arc<HashMap<ItemId, Arc<ItemConfig>>>;

pub struct VisibleAgent {
    pub position: Position,
    pub name: String,
    pub life: u32,
}

#[derive(Debug, Clone)]
pub struct KnownSpell {
    pub words: String,
    pub group: SpellGroup,
}

/// A `PlayerWalkAck` strip whose length didn't match what the step should have
/// uncovered, or one that arrived without a derivable single-step direction —
/// see `vault/known-issues/viewport-geometry-gaps-left-open.md`. The floor's
/// tiles are left unwritten rather than guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripMismatch {
    WrongLength {
        floor: u8,
        expected: usize,
        actual: usize,
    },
    NotAStep {
        from: Position,
        to: Position,
    },
}

const TILE_PRUNE_INTERVAL: u32 = 64;
const TILE_MEMORY_RADIUS: i64 = 2 * PLAYER_VIEWPORT_WIDTH as i64;

pub struct World {
    pub items: ItemCatalogue,
    pub agent_id: AgentId,
    pub position: Position,
    /// The character's own name, from `DescribePlayer` — empty until then.
    pub name: String,
    pub speed: u16,
    pub life: Pool,
    pub mana: Pool,
    pub agents: HashMap<AgentId, VisibleAgent>,
    pub tiles: HashMap<Position, ItemStack>,
    pub equipment: HashMap<InventorySlot, ItemId>,
    pub containers: HashMap<ContainerId, Vec<Option<(ItemId, u8)>>>,
    pub carried: Option<ContainerId>,
    pub spells: HashMap<SpellId, KnownSpell>,
    pub strip_mismatches: u64,
    pub first_strip_mismatch: Option<StripMismatch>,
    pub target_lost_seq: Option<u32>,
    spell_cooldowns: HashMap<SpellId, Instant>,
    group_cooldowns: HashMap<SpellGroup, Instant>,
    walk_ack_count: u32,
}

impl World {
    pub fn new(items: ItemCatalogue) -> Self {
        Self {
            items,
            agent_id: AgentId(0),
            position: Position::new(0, 0, 7),
            name: String::new(),
            speed: 100,
            life: Pool {
                current: 1,
                maximum: 1,
            },
            mana: Pool {
                current: 0,
                maximum: 0,
            },
            agents: HashMap::new(),
            tiles: HashMap::new(),
            equipment: HashMap::new(),
            containers: HashMap::new(),
            carried: None,
            spells: HashMap::new(),
            strip_mismatches: 0,
            first_strip_mismatch: None,
            target_lost_seq: None,
            spell_cooldowns: HashMap::new(),
            group_cooldowns: HashMap::new(),
            walk_ack_count: 0,
        }
    }

    pub fn apply(&mut self, message: &ServerMessage) {
        match message {
            ServerMessage::DescribePlayer {
                agent_id,
                position,
                name,
                speed,
                life,
                mana,
                inventory_head,
                inventory_amulet,
                inventory_backpack,
                inventory_chest,
                inventory_right_hand,
                inventory_left_hand,
                inventory_legs,
                inventory_feet,
                inventory_ring,
                inventory_trinket,
                ..
            } => {
                self.agent_id = *agent_id;
                self.position = position.clone();
                self.name = name.clone();
                self.speed = *speed;
                self.life = life.clone();
                self.mana = mana.clone();
                self.equipment.clear();
                for (slot, item) in [
                    (InventorySlot::Head, inventory_head),
                    (InventorySlot::Amulet, inventory_amulet),
                    (InventorySlot::Backpack, inventory_backpack),
                    (InventorySlot::Chest, inventory_chest),
                    (InventorySlot::RightHand, inventory_right_hand),
                    (InventorySlot::LeftHand, inventory_left_hand),
                    (InventorySlot::Legs, inventory_legs),
                    (InventorySlot::Feet, inventory_feet),
                    (InventorySlot::Ring, inventory_ring),
                    (InventorySlot::Trinket, inventory_trinket),
                ] {
                    if let Some(id) = item {
                        self.equipment.insert(slot, *id);
                    }
                }
            }
            ServerMessage::DescribeMap {
                tiles,
                center,
                floor,
            } => {
                self.tiles.retain(|position, _| position.z != *floor);
                for (position, stack) in describe_map_positions(center, *floor).zip(tiles.iter()) {
                    if let Some(position) = position {
                        self.tiles.insert(position, *stack);
                    }
                }
            }
            ServerMessage::TileUpdated { position, items } => {
                self.tiles.insert(position.clone(), **items);
            }
            ServerMessage::PlayerWalkAck { position, tiles } => {
                self.apply_walk_ack(position, tiles);
            }
            ServerMessage::SpawnAgent {
                agent_id,
                position,
                name,
                life,
                ..
            } => {
                if *agent_id != self.agent_id {
                    self.agents.insert(
                        *agent_id,
                        VisibleAgent {
                            position: position.clone(),
                            name: name.clone(),
                            life: *life,
                        },
                    );
                }
            }
            ServerMessage::MoveAgent {
                agent_id,
                direction,
                from,
            } => {
                if *agent_id != self.agent_id
                    && let Some(agent) = self.agents.get_mut(agent_id)
                {
                    agent.position = from.clone() + *direction;
                }
            }
            ServerMessage::TeleportAgent { agent_id, position } => {
                if *agent_id == self.agent_id {
                    self.position = position.clone();
                } else if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.position = position.clone();
                }
            }
            ServerMessage::RemoveAgent { agent_id } => {
                self.agents.remove(agent_id);
            }
            ServerMessage::AgentLifeChanged {
                agent_id,
                current,
                max,
            } => {
                if *agent_id == self.agent_id {
                    self.life = Pool {
                        current: *current,
                        maximum: *max,
                    };
                } else if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.life = *current;
                }
            }
            ServerMessage::AgentManaUpdated {
                agent_id,
                current,
                max,
            } => {
                if *agent_id == self.agent_id {
                    self.mana = Pool {
                        current: *current,
                        maximum: *max,
                    };
                }
            }
            ServerMessage::IventorySlotUpdated { slot, item_id } => match item_id {
                Some(id) => {
                    self.equipment.insert(*slot, *id);
                }
                None => {
                    self.equipment.remove(slot);
                }
            },
            ServerMessage::AgentSpeedUpdated { agent_id, speed } => {
                if *agent_id == self.agent_id {
                    self.speed = *speed;
                }
            }
            ServerMessage::OpenContainer {
                container_id,
                items,
                ..
            }
            | ServerMessage::UpdateContainer {
                container_id,
                items,
            } => {
                self.containers.insert(*container_id, items.to_vec());
            }
            ServerMessage::ContainerClosed { container_id } => {
                self.containers.remove(container_id);
                if self.carried == Some(*container_id) {
                    self.carried = None;
                }
            }
            ServerMessage::SpellList { spells } => {
                self.spells.clear();
                for entry in spells {
                    self.spells.insert(
                        entry.id,
                        KnownSpell {
                            words: entry.words.clone(),
                            group: entry.group,
                        },
                    );
                }
            }
            ServerMessage::TargetLost { seq } => {
                self.target_lost_seq = Some(*seq);
            }
            _ => {}
        }
    }

    /// `SpellCast` carries no group, so the caller — which sent the cast and knows
    /// which spell it was — supplies it along with the clock the timers run on.
    pub fn apply_at(&mut self, message: &ServerMessage, group: SpellGroup, now: Instant) {
        if let ServerMessage::SpellCast {
            spell,
            spell_cooldown_ms,
            group_cooldown_ms,
        } = message
        {
            self.spell_cooldowns.insert(
                *spell,
                now + Duration::from_millis(*spell_cooldown_ms as u64),
            );
            self.group_cooldowns.insert(
                group,
                now + Duration::from_millis(*group_cooldown_ms as u64),
            );
        }
    }

    fn apply_walk_ack(&mut self, position: &Position, tiles: &[(u8, Box<[ItemStack]>)]) {
        let dx = position.x as i32 - self.position.x as i32;
        let dy = position.y as i32 - self.position.y as i32;
        let direction = Direction::from_step(dx, dy);

        match direction {
            Some(direction) => {
                for (floor, strip) in tiles {
                    let expected = expansion_positions(position, direction, *floor);
                    if expected.len() == strip.len() {
                        for (pos, tile) in expected.into_iter().zip(strip.iter()) {
                            self.tiles.insert(pos, *tile);
                        }
                    } else {
                        self.record_strip_mismatch(StripMismatch::WrongLength {
                            floor: *floor,
                            expected: expected.len(),
                            actual: strip.len(),
                        });
                    }
                }
            }
            None => {
                if !tiles.is_empty() {
                    self.record_strip_mismatch(StripMismatch::NotAStep {
                        from: self.position.clone(),
                        to: position.clone(),
                    });
                }
            }
        }

        self.position = position.clone();

        self.walk_ack_count += 1;
        if self.walk_ack_count.is_multiple_of(TILE_PRUNE_INTERVAL) {
            self.prune_distant_tiles();
        }
    }

    fn record_strip_mismatch(&mut self, mismatch: StripMismatch) {
        self.strip_mismatches += 1;
        if self.first_strip_mismatch.is_none() {
            self.first_strip_mismatch = Some(mismatch);
        }
    }

    fn prune_distant_tiles(&mut self) {
        let cx = self.position.x as i64;
        let cy = self.position.y as i64;
        self.tiles.retain(|pos, _| {
            (pos.x as i64 - cx).abs() <= TILE_MEMORY_RADIUS
                && (pos.y as i64 - cy).abs() <= TILE_MEMORY_RADIUS
        });
    }

    pub fn is_walkable(&self, position: Position) -> bool {
        let Some(stack) = self.tiles.get(&position) else {
            return false;
        };

        let mut has_ground = false;
        for (item_id, _) in stack.iter().flatten() {
            let Some(config) = self.items.get(item_id) else {
                return false;
            };
            if config.has_flag(ItemFlag::Unpass) {
                return false;
            }
            has_ground |= config.has_flag(ItemFlag::Ground);
        }

        has_ground
    }

    pub fn friction_at(&self, position: Position) -> Option<u16> {
        self.tiles
            .get(&position)?
            .iter()
            .flatten()
            .find_map(|(item_id, _)| self.items.get(item_id)?.attr_tile_friction())
    }

    /// Marks a container as the bot's own backpack, so `carried_amount`
    /// includes it and `backpack` returns it. A container is never marked
    /// implicitly: `OpenContainer` doesn't say where a container is, so a
    /// corpse the bot opened to loot must not be counted as carried until
    /// this is called on it. There is only ever one — replacing, not adding.
    pub fn mark_carried(&mut self, id: ContainerId) {
        self.carried = Some(id);
    }

    /// The bot's own backpack, once its `OpenContainer` reply has been marked
    /// with `mark_carried`. `None` before that — there is nowhere to loot to.
    pub fn backpack(&self) -> Option<ContainerId> {
        self.carried
    }

    /// Excludes two things: equipment, since `IventorySlotUpdated` gives a
    /// slot's item id with no amount; and any container not marked with
    /// `mark_carried`, since a container's contents alone don't say whether
    /// it's the bot's own or something it merely opened.
    pub fn carried_amount(&self, item: ItemId) -> u32 {
        self.containers
            .iter()
            .filter(|(id, _)| self.carried == Some(**id))
            .flat_map(|(_, stack)| stack.iter())
            .filter_map(|slot| slot.as_ref())
            .filter(|(id, _)| *id == item)
            .map(|(_, amount)| *amount as u32)
            .sum()
    }

    pub fn spell_ready(&self, id: SpellId, group: SpellGroup, now: Instant) -> bool {
        let spell_ready = self
            .spell_cooldowns
            .get(&id)
            .is_none_or(|&ready_at| now >= ready_at);
        let group_ready = self
            .group_cooldowns
            .get(&group)
            .is_none_or(|&ready_at| now >= ready_at);
        spell_ready && group_ready
    }
}

#[cfg(test)]
impl World {
    pub fn set_tile(&mut self, position: Position, items: ItemStack) {
        self.tiles.insert(position, items);
    }

    /// Writes ids 1 (ground) and 3 (wall) — `testing::catalogue`'s ids — so a
    /// `World` built over that catalogue resolves this tile as blocked rather
    /// than merely unknown.
    pub fn block(&mut self, position: Position) {
        let mut items: ItemStack = Default::default();
        items[0] = Some((ItemId(1), 1));
        items[1] = Some((ItemId(3), 1));
        self.tiles.insert(position, items);
    }

    pub fn see_creature(&mut self, id: AgentId, position: Position) {
        self.see_named_creature(id, position, "");
    }

    pub fn see_named_creature(&mut self, id: AgentId, position: Position, name: &str) {
        self.agents.insert(
            id,
            VisibleAgent {
                position,
                name: name.to_string(),
                life: 100,
            },
        );
    }

    pub fn remove_agent(&mut self, id: AgentId) {
        self.agents.remove(&id);
    }

    pub fn set_life(&mut self, current: u32, maximum: u32) {
        self.life = Pool { current, maximum };
    }

    pub fn set_mana(&mut self, current: u32, maximum: u32) {
        self.mana = Pool { current, maximum };
    }

    /// The server's `LocalIdMap` cursor wraps at index 4094, so it never mints
    /// `ContainerId(u16::MAX)`.
    pub fn carry(&mut self, item: ItemId, amount: u8) {
        const CARRIED: ContainerId = ContainerId(u16::MAX);
        self.mark_carried(CARRIED);
        let stack = self.containers.entry(CARRIED).or_default();
        stack.retain(|slot| !matches!(slot, Some((id, _)) if *id == item));
        if amount > 0 {
            stack.push(Some((item, amount)));
        }
    }

    pub fn knows_spell(&mut self, id: SpellId, words: &str, group: SpellGroup) {
        self.spells.insert(
            id,
            KnownSpell {
                words: words.to_string(),
                group,
            },
        );
    }

    pub fn start_cooldown(
        &mut self,
        id: SpellId,
        group: SpellGroup,
        now: Instant,
        spell_ms: u64,
        group_ms: u64,
    ) {
        self.spell_cooldowns
            .insert(id, now + Duration::from_millis(spell_ms));
        self.group_cooldowns
            .insert(group, now + Duration::from_millis(group_ms));
    }

    pub fn open_container(&mut self, id: ContainerId, items: &[Option<(ItemId, u8)>]) {
        self.containers.insert(id, items.to_vec());
    }

    pub fn place_self(&mut self, id: AgentId, position: Position, speed: u16) {
        self.agent_id = id;
        self.position = position;
        self.speed = speed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use rustibia_server::constants::view::{PLAYER_VIEWPORT_HEIGHT, VIEWPORT_SIZE};
    use rustibia_server::entities::agent::{Facing, OutfitColors, OutfitId};
    use rustibia_server::messages::SpellListEntry;

    fn a_world() -> World {
        World::new(Arc::new(HashMap::new()))
    }

    #[test]
    fn describe_player_seeds_the_bot() {
        let mut world = a_world();
        world.apply(&describe_player());

        assert_eq!(world.position, Position::new(100, 100, 7));
        assert_eq!(world.speed, 100);
        assert_eq!(world.life.current, 500);
        assert_eq!(world.agent_id, AgentId(1));
        assert_eq!(world.name, "Loadbot Aab");
    }

    #[test]
    fn a_walk_ack_moves_the_bot() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::PlayerWalkAck {
            position: Position::new(101, 100, 7),
            tiles: vec![],
        });

        assert_eq!(world.position, Position::new(101, 100, 7));
    }

    #[test]
    fn a_spawned_creature_is_visible_and_a_removed_one_is_not() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::SpawnAgent {
            agent_id: AgentId(2),
            outfit: (OutfitId(21), OutfitColors::new(0, 0, 0, 0)),
            position: Position::new(103, 100, 7),
            facing: Facing::North,
            name: "Rat".to_string(),
            life: 100,
            speed: 100,
        });

        assert_eq!(
            world.agents[&AgentId(2)].position,
            Position::new(103, 100, 7)
        );

        world.apply(&ServerMessage::RemoveAgent {
            agent_id: AgentId(2),
        });

        assert!(!world.agents.contains_key(&AgentId(2)));
    }

    #[test]
    fn the_bots_own_spawn_is_not_a_target() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::SpawnAgent {
            agent_id: AgentId(1),
            outfit: (OutfitId(128), OutfitColors::new(0, 0, 0, 0)),
            position: Position::new(100, 100, 7),
            facing: Facing::South,
            name: "Loadbot Aab".to_string(),
            life: 100,
            speed: 100,
        });

        assert!(!world.agents.contains_key(&AgentId(1)));
    }

    #[test]
    fn a_move_agent_lands_at_from_plus_direction() {
        let mut world = a_world();
        world.apply(&describe_player());
        world.see_creature(AgentId(2), Position::new(103, 100, 7));

        world.apply(&ServerMessage::MoveAgent {
            agent_id: AgentId(2),
            direction: Direction::East,
            from: Position::new(103, 100, 7),
        });

        assert_eq!(
            world.agents[&AgentId(2)].position,
            Position::new(104, 100, 7)
        );
    }

    #[test]
    fn own_teleport_updates_position_but_never_becomes_a_target() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::TeleportAgent {
            agent_id: AgentId(1),
            position: Position::new(200, 200, 7),
        });

        assert_eq!(world.position, Position::new(200, 200, 7));
        assert!(!world.agents.contains_key(&AgentId(1)));
    }

    #[test]
    fn agent_life_changed_updates_self_absolutely_and_others_as_a_percentage() {
        let mut world = a_world();
        world.apply(&describe_player());
        world.see_creature(AgentId(2), Position::new(103, 100, 7));

        world.apply(&ServerMessage::AgentLifeChanged {
            agent_id: AgentId(1),
            current: 250,
            max: 500,
        });
        assert_eq!(
            world.life,
            Pool {
                current: 250,
                maximum: 500
            }
        );

        world.apply(&ServerMessage::AgentLifeChanged {
            agent_id: AgentId(2),
            current: 40,
            max: 100,
        });
        assert_eq!(world.agents[&AgentId(2)].life, 40);
    }

    #[test]
    fn agent_mana_updated_only_ever_touches_the_bots_own_pool() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::AgentManaUpdated {
            agent_id: AgentId(2),
            current: 999,
            max: 999,
        });
        assert_eq!(
            world.mana,
            Pool {
                current: 400,
                maximum: 400
            }
        );

        world.apply(&ServerMessage::AgentManaUpdated {
            agent_id: AgentId(1),
            current: 100,
            max: 400,
        });
        assert_eq!(
            world.mana,
            Pool {
                current: 100,
                maximum: 400
            }
        );
    }

    // Asymmetric fixture: item id = tile index, so a transposed or offset
    // placement fails instead of accidentally matching.
    fn a_describe_map(center: Position, floor: u8) -> ServerMessage {
        let mut tiles = Box::new([[None; 8]; VIEWPORT_SIZE]);
        for (i, tile) in tiles.iter_mut().enumerate() {
            tile[0] = Some((ItemId(i as u16), 1));
        }
        ServerMessage::DescribeMap {
            tiles,
            center,
            floor,
        }
    }

    fn item_at(world: &World, position: Position) -> u16 {
        (world.tiles[&position][0].unwrap().0).0
    }

    #[test]
    fn a_describe_map_for_one_floor_does_not_erase_another() {
        let mut world = a_world();
        let center = Position::new(200, 200, 6); // the bot stands at z=6
        let center_index = ((PLAYER_VIEWPORT_HEIGHT / 2) * PLAYER_VIEWPORT_WIDTH
            + PLAYER_VIEWPORT_WIDTH / 2) as u16;

        for floor in [5_u8, 6, 7] {
            world.apply(&a_describe_map(center.clone(), floor));
        }

        for floor in [5_u8, 6, 7] {
            let offset = center.z as i32 - floor as i32;
            let position = Position::new((200 + offset) as u16, (200 + offset) as u16, floor);
            assert_eq!(item_at(&world, position), center_index);
        }
    }

    fn an_expansion_strip(floor: u8, len: usize) -> Vec<(u8, Box<[ItemStack]>)> {
        let tiles: Vec<ItemStack> = (0..len)
            .map(|i| {
                let mut tile: ItemStack = Default::default();
                tile[0] = Some((ItemId(i as u16), 1));
                tile
            })
            .collect();
        vec![(floor, tiles.into_boxed_slice())]
    }

    #[test]
    fn a_matching_length_strip_is_written_into_tiles() {
        let mut world = a_world();
        world.apply(&describe_player()); // position (100, 100, 7)

        world.apply(&ServerMessage::PlayerWalkAck {
            position: Position::new(101, 100, 7),
            tiles: an_expansion_strip(7, PLAYER_VIEWPORT_HEIGHT),
        });

        // The strip is centred on the ack's new position (101, 100), not the one
        // the bot walked from — the east step moves x_end with it.
        let x_end = 101 + (PLAYER_VIEWPORT_WIDTH / 2) as u16;
        let y_start = 100 - (PLAYER_VIEWPORT_HEIGHT / 2) as u16;
        assert_eq!(item_at(&world, Position::new(x_end, y_start, 7)), 0);
    }

    #[test]
    fn a_strip_of_the_wrong_length_is_recorded_as_a_mismatch_not_guessed_at() {
        let mut world = a_world();
        world.apply(&describe_player());

        world.apply(&ServerMessage::PlayerWalkAck {
            position: Position::new(101, 100, 7),
            tiles: an_expansion_strip(7, 3), // East expects PLAYER_VIEWPORT_HEIGHT (15)
        });

        assert_eq!(world.strip_mismatches, 1);
        assert_eq!(
            world.first_strip_mismatch,
            Some(StripMismatch::WrongLength {
                floor: 7,
                expected: PLAYER_VIEWPORT_HEIGHT,
                actual: 3,
            })
        );
        assert_eq!(world.position, Position::new(101, 100, 7));
    }

    #[test]
    fn tiles_far_behind_the_bot_are_pruned_every_64th_walk_ack() {
        let mut world = a_world();
        world.place_self(AgentId(1), Position::new(1000, 1000, 7), 100);

        let radius = (2 * PLAYER_VIEWPORT_WIDTH) as u16;
        let far = Position::new(1000 - radius - 1, 1000, 7);
        let near = Position::new(1000 + 1, 1000, 7);
        world.set_tile(far.clone(), Default::default());
        world.set_tile(near.clone(), Default::default());

        for i in 0..64 {
            world.apply(&ServerMessage::PlayerWalkAck {
                position: Position::new(1000, 1000, 7),
                tiles: vec![],
            });
            if i == 62 {
                assert!(world.tiles.contains_key(&far), "pruned before the 64th ack");
            }
        }

        assert!(!world.tiles.contains_key(&far));
        assert!(world.tiles.contains_key(&near));
    }

    #[test]
    fn a_tile_with_a_blocking_item_is_not_walkable() {
        let mut world = World::new(catalogue());
        world.set_tile(Position::new(1, 1, 7), stack(&[ItemId(1), ItemId(3)]));

        assert!(!world.is_walkable(Position::new(1, 1, 7)));
    }

    #[test]
    fn a_tile_with_ground_alone_is_walkable_and_carries_its_friction() {
        let mut world = World::new(catalogue());
        world.set_tile(Position::new(1, 1, 7), stack(&[ItemId(2)]));

        assert!(world.is_walkable(Position::new(1, 1, 7)));
        assert_eq!(world.friction_at(Position::new(1, 1, 7)), Some(260));
    }

    #[test]
    fn an_unseen_tile_is_not_walkable() {
        let world = World::new(catalogue());

        assert!(!world.is_walkable(Position::new(9, 9, 7)));
    }

    #[test]
    fn an_unresolvable_item_makes_a_tile_unwalkable() {
        let mut world = World::new(catalogue());
        world.set_tile(Position::new(1, 1, 7), stack(&[ItemId(2), ItemId(999)]));

        assert!(!world.is_walkable(Position::new(1, 1, 7)));
    }

    #[test]
    fn block_makes_a_tile_unwalkable() {
        let mut world = World::new(catalogue());
        world.block(Position::new(5, 5, 7));

        assert!(!world.is_walkable(Position::new(5, 5, 7)));
    }

    #[test]
    fn a_container_update_tracks_what_is_left_in_the_bag() {
        let mut world = World::new(catalogue());
        world.apply(&ServerMessage::OpenContainer {
            container_id: ContainerId(0),
            capacity: 20,
            has_parent: false,
            title: "backpack".to_string(),
            items: vec![Some((ItemId(266), 3)), Some((ItemId(268), 1))].into_boxed_slice(),
        });
        world.mark_carried(ContainerId(0));

        assert_eq!(world.carried_amount(ItemId(266)), 3);
        assert_eq!(world.carried_amount(ItemId(268)), 1);
        assert_eq!(world.carried_amount(ItemId(999)), 0);

        world.apply(&ServerMessage::UpdateContainer {
            container_id: ContainerId(0),
            items: vec![Some((ItemId(266), 2))].into_boxed_slice(),
        });
        assert_eq!(world.carried_amount(ItemId(266)), 2);
        assert_eq!(world.carried_amount(ItemId(268)), 0);
    }

    #[test]
    fn an_opened_container_does_not_count_until_marked_as_carried() {
        let mut world = World::new(catalogue());
        world.apply(&ServerMessage::OpenContainer {
            container_id: ContainerId(1),
            capacity: 20,
            has_parent: false,
            title: "a corpse".to_string(),
            items: vec![Some((ItemId(266), 5))].into_boxed_slice(),
        });

        assert_eq!(world.carried_amount(ItemId(266)), 0);

        world.mark_carried(ContainerId(1));
        assert_eq!(world.carried_amount(ItemId(266)), 5);
    }

    #[test]
    fn closing_a_carried_container_forgets_it() {
        let mut world = World::new(catalogue());
        world.apply(&ServerMessage::OpenContainer {
            container_id: ContainerId(1),
            capacity: 20,
            has_parent: false,
            title: "backpack".to_string(),
            items: vec![Some((ItemId(266), 5))].into_boxed_slice(),
        });
        world.mark_carried(ContainerId(1));
        assert_eq!(world.carried_amount(ItemId(266)), 5);

        world.apply(&ServerMessage::ContainerClosed {
            container_id: ContainerId(1),
        });

        assert_eq!(world.carried_amount(ItemId(266)), 0);
        assert!(!world.containers.contains_key(&ContainerId(1)));
        assert_eq!(world.carried, None);
    }

    #[test]
    fn equipped_items_do_not_count_toward_carried_amount() {
        let mut world = World::new(catalogue());
        world.apply(&describe_player()); // equips a backpack (2854) and a weapon (3264)

        assert_eq!(world.carried_amount(ItemId(2854)), 0);
        assert_eq!(world.carried_amount(ItemId(3264)), 0);
    }

    #[test]
    fn inventory_slot_updated_tracks_equipment_with_none_clearing_the_slot() {
        let mut world = a_world();
        world.apply(&describe_player()); // right hand starts as Some(ItemId(3264))

        world.apply(&ServerMessage::IventorySlotUpdated {
            slot: InventorySlot::RightHand,
            item_id: Some(ItemId(3265)),
        });
        assert_eq!(world.equipment[&InventorySlot::RightHand], ItemId(3265));

        world.apply(&ServerMessage::IventorySlotUpdated {
            slot: InventorySlot::RightHand,
            item_id: None,
        });
        assert!(!world.equipment.contains_key(&InventorySlot::RightHand));
    }

    #[test]
    fn the_carry_fixture_can_empty_a_tracked_stack() {
        let mut world = World::new(catalogue());
        world.carry(ItemId(266), 5);
        assert_eq!(world.carried_amount(ItemId(266)), 5);

        world.carry(ItemId(266), 0);
        assert_eq!(world.carried_amount(ItemId(266)), 0);
    }

    #[test]
    fn a_cast_reply_starts_both_cooldowns() {
        let mut world = World::new(catalogue());
        let now = Instant::now();

        world.apply_at(
            &ServerMessage::SpellCast {
                spell: SpellId(1),
                spell_cooldown_ms: 1000,
                group_cooldown_ms: 2000,
            },
            SpellGroup::Healing,
            now,
        );

        assert!(!world.spell_ready(SpellId(1), SpellGroup::Healing, now));
        assert!(!world.spell_ready(
            SpellId(2),
            SpellGroup::Healing,
            now + Duration::from_millis(1500)
        ));
        assert!(world.spell_ready(
            SpellId(1),
            SpellGroup::Healing,
            now + Duration::from_millis(2500)
        ));
    }

    #[test]
    fn spell_list_populates_known_spells() {
        let mut world = a_world();
        world.apply(&ServerMessage::SpellList {
            spells: vec![SpellListEntry {
                id: SpellId(7),
                name: "Light Healing".to_string(),
                words: "exura".to_string(),
                level: 8,
                icon: 1,
                aimable: false,
                group: SpellGroup::Healing,
            }],
        });

        assert_eq!(world.spells[&SpellId(7)].words, "exura");
        assert_eq!(world.spells[&SpellId(7)].group, SpellGroup::Healing);
    }
}

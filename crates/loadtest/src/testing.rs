use std::collections::HashMap;
use std::sync::Arc;

use rustibia_server::constants::view::VIEWPORT_SIZE;
use rustibia_server::entities::agent::{AgentId, Facing, OutfitColors, OutfitId, Pool};
use rustibia_server::entities::items::{ItemAttribute, ItemConfig, ItemFlag, ItemId};
use rustibia_server::entities::position::Position;
use rustibia_server::messages::{ItemStack, ServerMessage, SpellListEntry};
use tokio::sync::mpsc::Receiver;

use crate::metrics::Event;
use crate::world::{ItemCatalogue, World};

/// Pops every `Event` a bot has sent so far, for asserting on what a bot
/// reported without racing its background task for a channel read.
pub fn drain(rx: &mut Receiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

pub fn describe_player() -> ServerMessage {
    ServerMessage::DescribePlayer {
        agent_id: AgentId(1),
        position: Position::new(100, 100, 7),
        facing: Facing::South,
        name: "Loadbot Aab".to_string(),
        level: 20,
        life: Pool {
            current: 500,
            maximum: 500,
        },
        mana: Pool {
            current: 400,
            maximum: 400,
        },
        outfit: (OutfitId(128), OutfitColors::new(78, 69, 58, 76)),
        speed: 100,
        capacity: 60000,
        inventory_head: None,
        inventory_amulet: None,
        inventory_backpack: Some(ItemId(2854)),
        inventory_chest: None,
        inventory_right_hand: Some(ItemId(3264)),
        inventory_left_hand: None,
        inventory_legs: None,
        inventory_feet: None,
        inventory_ring: None,
        inventory_trinket: None,
    }
}

fn empty_describe_map(center: Position, floor: u8) -> ServerMessage {
    let tiles: Box<[ItemStack; VIEWPORT_SIZE]> =
        Box::new(std::array::from_fn(|_| ItemStack::default()));
    ServerMessage::DescribeMap {
        tiles,
        center,
        floor,
    }
}

/// A fully-walkable floor centred on `center`, for a test that needs the bot
/// to actually issue a `MovePlayer` — sent as a follow-up `DescribeMap` after
/// `login_burst`, which stays inert on its own so it doesn't perturb every
/// other `bot` test.
pub fn a_walkable_map(center: Position, floor: u8) -> ServerMessage {
    let tiles: Box<[ItemStack; VIEWPORT_SIZE]> =
        Box::new(std::array::from_fn(|_| stack(&[ItemId(1)])));
    ServerMessage::DescribeMap {
        tiles,
        center,
        floor,
    }
}

/// Replays the server's real login burst (`session/view.rs::player_spawned`)
/// in the order it actually sends it: one `DescribeMap`, then any
/// `SpawnAgent`s already in view, then `DescribePlayer`, `PlayerSkills`,
/// `SpellList`, `PlayerStatus`. `DescribePlayer` first, as a fixture used to
/// send it, is an order the server never produces.
pub fn login_burst() -> Vec<ServerMessage> {
    login_burst_with(vec![], vec![])
}

/// `login_burst` with extra `SpawnAgent`s already in view (before
/// `DescribePlayer`, as the real burst orders them) and the `SpellList` a
/// test needs the bot to know.
pub fn login_burst_with(
    spawns: Vec<ServerMessage>,
    spells: Vec<SpellListEntry>,
) -> Vec<ServerMessage> {
    let mut burst = vec![empty_describe_map(Position::new(100, 100, 7), 7)];
    burst.extend(spawns);
    burst.push(describe_player());
    burst.push(ServerMessage::PlayerSkills {
        experience: 0,
        skills: vec![],
    });
    burst.push(ServerMessage::SpellList { spells });
    burst.push(ServerMessage::PlayerStatus { status: 0 });
    burst
}

pub fn a_ground(id: u16, friction: u16) -> ItemConfig {
    ItemConfig::new(
        ItemId(id),
        "ground".to_string(),
        None,
        None,
        [ItemFlag::Ground],
        [ItemAttribute::TileFriction(friction)],
    )
}

pub fn a_wall(id: u16) -> ItemConfig {
    ItemConfig::new(
        ItemId(id),
        "wall".to_string(),
        None,
        None,
        [ItemFlag::Unpass],
        [],
    )
}

pub fn a_container(id: u16, capacity: u8) -> ItemConfig {
    ItemConfig::new(
        ItemId(id),
        "container".to_string(),
        None,
        None,
        [ItemFlag::Container],
        [ItemAttribute::Capacity(capacity)],
    )
}

pub fn a_stackable(id: u16) -> ItemConfig {
    ItemConfig::new(
        ItemId(id),
        "stackable".to_string(),
        None,
        None,
        [ItemFlag::Cumulative],
        [],
    )
}

pub fn stack(ids: &[ItemId]) -> ItemStack {
    let mut stack: ItemStack = Default::default();
    for (slot, id) in ids.iter().enumerate() {
        stack[slot] = Some((*id, 1));
    }
    stack
}

/// Catalogue item ids, not ones from the shipped catalogue — this module's
/// `catalogue()` is the only source of truth for what they name.
pub const CORPSE_ID: u16 = 4;
pub const STACKABLE_ID: u16 = 5;

pub fn catalogue() -> ItemCatalogue {
    Arc::new(HashMap::from([
        (ItemId(1), Arc::new(a_ground(1, 150))),
        (ItemId(2), Arc::new(a_ground(2, 260))),
        (ItemId(3), Arc::new(a_wall(3))),
        (ItemId(CORPSE_ID), Arc::new(a_container(CORPSE_ID, 20))),
        (ItemId(STACKABLE_ID), Arc::new(a_stackable(STACKABLE_ID))),
    ]))
}

/// A 5x5 patch of walkable ground centred on the bot, so a step in any
/// direction is legal unless a test makes it otherwise.
pub fn a_world_on_open_ground() -> World {
    let mut world = World::new(catalogue());
    world.set_life(500, 500);
    world.set_mana(400, 400);
    for dx in -2i32..=2 {
        for dy in -2i32..=2 {
            world.set_tile(
                Position::new((100 + dx) as u16, (100 + dy) as u16, 7),
                stack(&[ItemId(1)]),
            );
        }
    }
    world.place_self(AgentId(1), Position::new(100, 100, 7), 100);
    world
}

/// Open ground with one creature standing next to the bot, and a target
/// already held — the state every Fight test starts from.
pub fn engaged_world() -> World {
    let mut world = a_world_on_open_ground();
    world.see_creature(AgentId(3), Position::new(101, 100, 7));
    world
}

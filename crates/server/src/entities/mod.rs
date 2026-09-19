pub mod agent;
pub mod chat;
pub mod combat;
pub mod conditions;
pub mod creature;
pub mod effects;
pub mod healing;
pub mod inventory;
pub mod items;
pub mod map;
pub mod player;
pub mod position;
pub mod skills;
pub mod spells;
pub mod support;
pub mod targeting;
pub mod vocation;
pub mod world_delta;
pub mod world_map;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Bounds {
    pub min: u32,
    pub max: u32,
}

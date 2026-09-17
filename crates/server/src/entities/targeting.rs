use std::sync::Arc;

use crate::entities::{agent::AgentKey, effects::AreaShape, position::Position};

#[derive(Debug, Clone)]
pub enum TargetMode {
    Caster,
    /// The caster's own combat target, refused beyond `range`.
    Target { range: u16 },
    Area {
        origin: AreaOrigin,
        shape: Arc<AreaShape>,
    },
}

#[derive(Debug, Clone)]
pub enum AreaOrigin {
    Caster,
    Target,
}

#[derive(Debug, Clone)]
pub enum AreaTarget {
    None,
    Agent(AgentKey),
    Position(Position),
}

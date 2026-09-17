use std::sync::Arc;

use crate::entities::{agent::AgentKey, effects::AreaShape, position::Position};

#[derive(Debug, Clone)]
pub enum TargetMode {
    Caster,
    /// The caster's own combat target, refused beyond `range`.
    Target {
        range: u16,
    },
    Area {
        origin: AreaOrigin,
        shape: Arc<AreaShape>,
    },
    /// The player the cast names, refused beyond `range`.
    Named {
        range: u16,
    },
}

/// Which agents a resolved cast may reach: a heal must not land on what a wave is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFilter {
    Any,
    Players,
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

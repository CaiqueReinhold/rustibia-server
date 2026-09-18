pub mod admin;
pub mod chat;
pub mod combat;
pub mod conditions;
pub mod config;
pub mod creature_abilities;
pub mod creature_behavior;
pub mod damage;
pub mod death;
pub mod description;
pub mod events;
pub mod experience;
pub mod healing;
pub mod item_action;
pub mod item_movement;
pub mod item_multi_action;
pub mod map_query;
pub mod movement;
pub mod pathfinding;
pub mod random;
pub mod skills;
pub mod spells;
pub mod support;
pub mod systems;
pub mod targeting;

use serde::Deserialize;

use crate::actors::world::ScheduledCommand;
use crate::entities::map::GameMap;
use crate::game::events::BroadcastMessage;
use crate::game::random::Rolls;

/// A point on the 50 ms game clock: *when* something happens.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Tick(pub u64);

/// A span of ticks: a cooldown, a walk duration, a respawn delay, a path cost.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Default, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct TickDelta(pub u64);

impl Tick {
    pub fn saturating_sub(self, other: Tick) -> TickDelta {
        TickDelta(self.0.saturating_sub(other.0))
    }
}

impl TickDelta {
    pub fn saturating_sub(self, other: TickDelta) -> TickDelta {
        TickDelta(self.0.saturating_sub(other.0))
    }
}

impl std::ops::Add<TickDelta> for Tick {
    type Output = Tick;

    fn add(self, delta: TickDelta) -> Tick {
        Tick(self.0 + delta.0)
    }
}

impl std::ops::AddAssign<TickDelta> for Tick {
    fn add_assign(&mut self, delta: TickDelta) {
        self.0 += delta.0;
    }
}

impl std::ops::Sub<Tick> for Tick {
    type Output = TickDelta;

    fn sub(self, other: Tick) -> TickDelta {
        TickDelta(self.0 - other.0)
    }
}

impl std::ops::Add for TickDelta {
    type Output = TickDelta;

    fn add(self, other: TickDelta) -> TickDelta {
        TickDelta(self.0 + other.0)
    }
}

impl std::ops::AddAssign for TickDelta {
    fn add_assign(&mut self, other: TickDelta) {
        self.0 += other.0;
    }
}

impl std::ops::Mul<u64> for TickDelta {
    type Output = TickDelta;

    fn mul(self, factor: u64) -> TickDelta {
        TickDelta(self.0 * factor)
    }
}

impl std::fmt::Display for Tick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for TickDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub struct TickCtx<'a> {
    pub map: &'a mut GameMap,
    pub events: &'a mut Vec<BroadcastMessage>,
    pub scheduled: &'a mut Vec<ScheduledCommand>,
    pub roll: &'a mut Rolls,
    pub tick: Tick,
}

#[derive(Clone, Copy)]
pub struct Mark {
    events: usize,
    scheduled: usize,
}

impl TickCtx<'_> {
    pub fn mark(&self) -> Mark {
        Mark {
            events: self.events.len(),
            scheduled: self.scheduled.len(),
        }
    }

    /// Discards everything reported since `mark`.
    pub fn rollback_to(&mut self, mark: Mark) {
        self.events.truncate(mark.events);
        self.scheduled.truncate(mark.scheduled);
    }
}

#[cfg(test)]
pub struct TestHarness {
    pub events: Vec<BroadcastMessage>,
    pub scheduled: Vec<ScheduledCommand>,
    pub roll: Rolls,
    pub tick: Tick,
}

#[cfg(test)]
impl TestHarness {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            scheduled: Vec::new(),
            roll: Rolls::new(0),
            tick: Tick(0),
        }
    }

    pub fn seeded(seed: u64) -> Self {
        Self {
            roll: Rolls::new(seed),
            ..Self::new()
        }
    }

    pub fn ctx<'a>(&'a mut self, map: &'a mut GameMap) -> TickCtx<'a> {
        TickCtx {
            map,
            events: &mut self.events,
            scheduled: &mut self.scheduled,
            roll: &mut self.roll,
            tick: self.tick,
        }
    }
}

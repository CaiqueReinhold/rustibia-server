use smallvec::SmallVec;

use crate::{
    entities::{Bounds, agent::AgentKey, combat::CombatElement},
    game::{Tick, TickDelta, config::GAME_CONFIG},
};

const LOGOUT_BLOCK_BIT: u32 = 1;
const HUNGRY_BIT: u32 = 1 << 1;
const PARALYZED_BIT: u32 = 1 << 2;
const BURNING_BIT: u32 = 1 << 3;
const POISONED_BIT: u32 = 1 << 4;
const ELECTRIFIED_BIT: u32 = 1 << 5;

#[derive(Debug, Clone)]
pub struct ConditionSpec {
    pub element: CombatElement,
    pub damage: Bounds,
    pub interval: TickDelta,
    pub schedule: SpecSchedule,
}

#[derive(Debug, Clone)]
pub enum SpecSchedule {
    Decaying { start: Option<u32> },
    Flat { count: u32 },
}

#[derive(Debug, PartialEq)]
pub struct DamageHit {
    pub value: u32,
    pub source: Option<AgentKey>,
    pub interval: TickDelta,
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
enum Schedule {
    Decaying {
        total: u32,
        start: u32,
        value: u32,
        dealt: u32,
    },
    Flat {
        value: u32,
        remaining: u32,
    },
}

#[derive(Debug, Clone)]
pub struct DamageOverTime {
    element: CombatElement,
    source: Option<AgentKey>,
    interval: TickDelta,
    generation: u32,
    schedule: Schedule,
}

impl DamageOverTime {
    pub fn decaying(
        element: CombatElement,
        source: Option<AgentKey>,
        interval: TickDelta,
        total: u32,
        start: Option<u32>,
    ) -> Self {
        let start = start
            .unwrap_or_else(|| total.div_ceil(20))
            .clamp(1, total.max(1));
        Self::new(
            element,
            source,
            interval,
            Schedule::Decaying {
                total,
                start,
                value: start,
                dealt: 0,
            },
        )
    }

    pub fn flat(
        element: CombatElement,
        source: Option<AgentKey>,
        interval: TickDelta,
        value: u32,
        count: u32,
    ) -> Self {
        Self::new(
            element,
            source,
            interval,
            Schedule::Flat {
                value,
                remaining: count,
            },
        )
    }

    fn new(
        element: CombatElement,
        source: Option<AgentKey>,
        interval: TickDelta,
        schedule: Schedule,
    ) -> Self {
        Self {
            element,
            source,
            interval,
            generation: 0,
            schedule,
        }
    }

    pub fn element(&self) -> CombatElement {
        self.element
    }

    pub fn remaining(&self) -> u32 {
        match self.schedule {
            Schedule::Decaying { total, dealt, .. } => total.saturating_sub(dealt),
            Schedule::Flat { value, remaining } => value * remaining,
        }
    }

    pub fn next_hit(&mut self) -> Option<u32> {
        match &mut self.schedule {
            Schedule::Decaying {
                total,
                start,
                value,
                dealt,
            } => {
                if *value == 0 || *dealt >= *total {
                    return None;
                }
                let hit = *value;
                *dealt += hit;

                // TFS's damage list: the hit falls from `start` to 1, each value repeated
                // while the running total stays nearer a straight line to `total` than one
                // more of it would put it.
                let n = *start + 1 - *value;
                let med = (n * *total) / *start;
                let with_another = (1.0 - (*dealt + *value) as f64 / med as f64).abs();
                let as_is = (1.0 - *dealt as f64 / med as f64).abs();
                if with_another >= as_is {
                    *value -= 1;
                }
                Some(hit)
            }
            Schedule::Flat { value, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                Some(*value)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Condition {
    Fed {
        until: Tick,
        generation: u32,
    },
    LogoutBlock {
        until: Tick,
    },
    DamageOverTime(Box<DamageOverTime>),
    Paralized {
        strength: f32,
        until: Tick,
        generation: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Conditions {
    conditions: SmallVec<[Condition; 3]>,
    next_generation: u32,
}

impl Conditions {
    pub fn new() -> Self {
        Self {
            conditions: SmallVec::new(),
            next_generation: 0,
        }
    }

    pub fn fed(&self) -> Option<(Tick, u32)> {
        self.conditions
            .iter()
            .find(|c| matches!(c, Condition::Fed { .. }))
            .and_then(|c| match c {
                Condition::Fed { until, generation } => Some((*until, *generation)),
                _ => None,
            })
    }

    pub fn can_feed(&self, current_tick: Tick, delta: TickDelta) -> bool {
        let remaining = self
            .fed()
            .map(|(until, _)| until.saturating_sub(current_tick))
            .unwrap_or(TickDelta(0));
        remaining + delta <= GAME_CONFIG.max_fed_ticks
    }

    pub fn add_fed_ticks(&mut self, current_tick: Tick, delta: TickDelta) -> u32 {
        let live = self
            .conditions
            .iter_mut()
            .find_map(|condition| match condition {
                Condition::Fed { until, generation } if *until > current_tick => {
                    Some((until, generation))
                }
                _ => None,
            });

        if let Some((until, generation)) = live {
            *until += delta;
            return *generation;
        }

        self.remove_fed();
        let generation = self.next_generation;
        self.conditions.push(Condition::Fed {
            until: current_tick + delta,
            generation,
        });
        self.next_generation += 1;
        generation
    }

    pub fn remove_fed(&mut self) {
        self.conditions
            .retain(|c| !matches!(c, Condition::Fed { .. }));
    }

    pub fn reset_logout_block(&mut self, current_tick: Tick) {
        let condition = self
            .conditions
            .iter_mut()
            .find(|c| matches!(c, Condition::LogoutBlock { .. }));

        if let Some(Condition::LogoutBlock { until }) = condition {
            *until = current_tick + GAME_CONFIG.logout_block_ticks;
        } else {
            self.conditions.push(Condition::LogoutBlock {
                until: current_tick + GAME_CONFIG.logout_block_ticks,
            });
        }
    }

    pub fn is_logout_blocked(&self, current_tick: Tick) -> bool {
        self.conditions
            .iter()
            .find(|c| match c {
                Condition::LogoutBlock { until } => *until > current_tick,
                _ => false,
            })
            .is_some()
    }

    pub fn add_paralysis(&mut self, until: Tick, strength: f32) {
        self.remove_paralysis();
        self.conditions.push(Condition::Paralized {
            strength,
            until,
            generation: self.next_generation,
        });
        self.next_generation += 1;
    }

    pub fn remove_paralysis(&mut self) {
        self.conditions
            .retain(|c| !matches!(c, Condition::Paralized { .. }));
    }

    pub fn get_paralysis_strength(&self) -> Option<f32> {
        self.conditions.iter().find_map(|c| match c {
            Condition::Paralized { strength, .. } => Some(*strength),
            _ => None,
        })
    }

    pub fn apply_damage_over_time(&mut self, mut incoming: DamageOverTime) -> Option<u32> {
        let generation = self.next_generation;
        let existing = self
            .conditions
            .iter_mut()
            .find_map(|condition| match condition {
                Condition::DamageOverTime(current) if current.element() == incoming.element() => {
                    Some(current)
                }
                _ => None,
            });

        match existing {
            Some(current) => {
                if incoming.remaining() <= current.remaining() {
                    return None;
                }
                incoming.generation = generation;
                **current = incoming;
            }
            None => {
                incoming.generation = generation;
                self.conditions
                    .push(Condition::DamageOverTime(Box::new(incoming)));
            }
        }
        self.next_generation += 1;
        Some(generation)
    }

    pub fn next_damage_hit(
        &mut self,
        element: CombatElement,
        generation: u32,
    ) -> Option<DamageHit> {
        let index = self.conditions.iter().position(|condition| {
            matches!(
                condition,
                Condition::DamageOverTime(dot)
                    if dot.element() == element && dot.generation == generation
            )
        })?;
        let Condition::DamageOverTime(dot) = &mut self.conditions[index] else {
            return None;
        };
        let value = dot.next_hit()?;
        let hit = DamageHit {
            value,
            source: dot.source,
            interval: dot.interval,
            exhausted: dot.remaining() == 0,
        };
        if hit.exhausted {
            self.conditions.remove(index);
        }
        Some(hit)
    }

    pub fn to_wire(&self, current_tick: Tick) -> u32 {
        let mut status = 0;

        if self.is_logout_blocked(current_tick) {
            status |= LOGOUT_BLOCK_BIT;
        }

        if !self.fed().is_some_and(|(until, _)| until > current_tick) {
            status |= HUNGRY_BIT;
        }

        if self.get_paralysis_strength().is_some() {
            status |= PARALYZED_BIT;
        }

        for condition in &self.conditions {
            if let Condition::DamageOverTime(dot) = condition {
                status |= match dot.element() {
                    CombatElement::Fire => BURNING_BIT,
                    CombatElement::Earth => POISONED_BIT,
                    CombatElement::Energy => ELECTRIFIED_BIT,
                    _ => 0,
                };
            }
        }

        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poison(total: u32) -> DamageOverTime {
        DamageOverTime::decaying(CombatElement::Earth, None, TickDelta(80), total, None)
    }

    #[test]
    fn a_decaying_schedule_matches_the_reference_sequence() {
        let mut dot = poison(100);
        let mut hits = Vec::new();
        while let Some(hit) = dot.next_hit() {
            hits.push(hit);
        }

        assert_eq!(&hits[..9], &[5, 5, 5, 5, 4, 4, 4, 4, 4]);
        assert_eq!(hits.len(), 46);
        assert_eq!(hits.iter().sum::<u32>(), 100);
    }

    #[test]
    fn a_flat_schedule_deals_its_count_unchanged() {
        let mut dot = DamageOverTime::flat(CombatElement::Fire, None, TickDelta(200), 10, 7);

        assert_eq!(dot.remaining(), 70);

        let mut hits = Vec::new();
        while let Some(hit) = dot.next_hit() {
            hits.push(hit);
        }

        assert_eq!(hits, [10, 10, 10, 10, 10, 10, 10]);
    }

    /// TFS caps an authored `start` at the attack's `max` and lets the list overshoot the
    /// rolled total; here the roll is the budget, so that `remaining` cannot disagree with
    /// what the schedule still deals.
    #[test]
    fn an_authored_start_never_spends_more_than_the_roll() {
        let mut dot =
            DamageOverTime::decaying(CombatElement::Fire, None, TickDelta(180), 3, Some(50));
        let mut dealt = 0;
        while let Some(hit) = dot.next_hit() {
            dealt += hit;
        }

        assert_eq!(dealt, 3);
        assert_eq!(dot.remaining(), 0);
    }

    #[test]
    fn only_a_stronger_condition_of_the_same_element_replaces_what_is_left() {
        let mut conditions = Conditions::new();
        let first = conditions.apply_damage_over_time(poison(100)).unwrap();

        assert!(conditions.apply_damage_over_time(poison(20)).is_none());

        let second = conditions.apply_damage_over_time(poison(500)).unwrap();

        assert_ne!(first, second);
        assert!(
            conditions
                .next_damage_hit(CombatElement::Earth, first)
                .is_none()
        );
        assert!(
            conditions
                .next_damage_hit(CombatElement::Earth, second)
                .is_some()
        );
    }

    #[test]
    fn hungry_is_the_absence_of_fed() {
        let mut conditions = Conditions::new();
        assert_eq!(conditions.to_wire(Tick(0)) & HUNGRY_BIT, HUNGRY_BIT);

        conditions.add_fed_ticks(Tick(0), TickDelta(100));
        assert_eq!(conditions.to_wire(Tick(0)) & HUNGRY_BIT, 0);
        assert_eq!(conditions.to_wire(Tick(100)) & HUNGRY_BIT, HUNGRY_BIT);
    }

    #[test]
    fn a_damage_condition_shows_its_element() {
        let mut conditions = Conditions::new();
        conditions.apply_damage_over_time(DamageOverTime::decaying(
            CombatElement::Fire,
            None,
            TickDelta(180),
            100,
            None,
        ));

        assert_eq!(conditions.to_wire(Tick(0)) & BURNING_BIT, BURNING_BIT);
        assert_eq!(conditions.to_wire(Tick(0)) & POISONED_BIT, 0);
    }

    #[test]
    fn a_full_stomach_refuses_another_meal() {
        let mut conditions = Conditions::new();
        conditions.add_fed_ticks(Tick(0), GAME_CONFIG.max_fed_ticks);

        assert!(!conditions.can_feed(Tick(0), TickDelta(240)));
        assert!(conditions.can_feed(Tick(0) + GAME_CONFIG.max_fed_ticks, TickDelta(240)));
    }

    #[test]
    fn eating_after_the_food_ran_out_starts_a_new_chain() {
        let mut conditions = Conditions::new();
        let first = conditions.add_fed_ticks(Tick(0), TickDelta(100));
        let second = conditions.add_fed_ticks(Tick(100), TickDelta(100));

        assert_ne!(first, second);
        assert_eq!(conditions.fed().unwrap().0, Tick(200));
    }
}

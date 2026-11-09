use serde::Deserialize;

use crate::entities::Bounds;
use crate::entities::combat::CombatElement;
use crate::entities::conditions::{ConditionSpec, DamageStep, SpecSchedule};
use crate::game::TickDelta;

#[derive(Deserialize)]
pub(crate) struct RawBounds {
    min: u32,
    max: u32,
}

impl From<RawBounds> for Bounds {
    fn from(raw: RawBounds) -> Self {
        Bounds {
            min: raw.min,
            max: raw.max,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RawConditionSpec {
    Decaying {
        element: CombatElement,
        damage: RawBounds,
        interval: TickDelta,
        #[serde(default)]
        start: Option<u32>,
    },
    Flat {
        element: CombatElement,
        damage: RawBounds,
        interval: TickDelta,
        count: u32,
    },
    Stepped {
        element: CombatElement,
        interval: TickDelta,
        steps: Vec<RawDamageStep>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDamageStep {
    count: u32,
    damage: u32,
}

impl RawConditionSpec {
    pub(crate) fn into_spec(self, delayed: bool) -> ConditionSpec {
        let (element, interval, schedule) = match self {
            RawConditionSpec::Decaying {
                element,
                damage,
                interval,
                start,
            } => (
                element,
                interval,
                SpecSchedule::Decaying {
                    damage: damage.into(),
                    start,
                },
            ),
            RawConditionSpec::Flat {
                element,
                damage,
                interval,
                count,
            } => (
                element,
                interval,
                SpecSchedule::Flat {
                    damage: damage.into(),
                    count,
                },
            ),
            RawConditionSpec::Stepped {
                element,
                interval,
                steps,
            } => (
                element,
                interval,
                SpecSchedule::Stepped(
                    steps
                        .into_iter()
                        .map(|step| DamageStep {
                            count: step.count,
                            value: step.damage,
                        })
                        .collect(),
                ),
            ),
        };
        ConditionSpec {
            element,
            interval,
            schedule,
            delayed,
        }
    }
}

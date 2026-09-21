use serde::Deserialize;

use crate::entities::Bounds;
use crate::entities::combat::CombatElement;
use crate::entities::conditions::{ConditionSpec, SpecSchedule};
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
}

impl RawConditionSpec {
    pub(crate) fn into_spec(self, delayed: bool) -> ConditionSpec {
        let (element, damage, interval, schedule) = match self {
            RawConditionSpec::Decaying {
                element,
                damage,
                interval,
                start,
            } => (element, damage, interval, SpecSchedule::Decaying { start }),
            RawConditionSpec::Flat {
                element,
                damage,
                interval,
                count,
            } => (element, damage, interval, SpecSchedule::Flat { count }),
        };
        ConditionSpec {
            element,
            damage: damage.into(),
            interval,
            schedule,
            delayed,
        }
    }
}

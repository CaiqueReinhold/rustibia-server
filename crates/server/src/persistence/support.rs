use serde::Deserialize;
use thiserror::Error;

use crate::{
    entities::{
        combat::CombatElement,
        conditions::SpeedEffect,
        support::{SpeedFormula, SpeedTerm, SupportEffect},
    },
    game::TickDelta,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpeedTerm {
    #[serde(default)]
    factor: f32,
    #[serde(default)]
    flat: i16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpeedFormula {
    min: RawSpeedTerm,
    max: RawSpeedTerm,
}

impl From<RawSpeedTerm> for SpeedTerm {
    fn from(raw: RawSpeedTerm) -> Self {
        SpeedTerm {
            factor: raw.factor,
            flat: raw.flat,
        }
    }
}

impl From<RawSpeedFormula> for SpeedFormula {
    fn from(raw: RawSpeedFormula) -> Self {
        SpeedFormula {
            min: raw.min.into(),
            max: raw.max.into(),
        }
    }
}

#[derive(Error, Debug)]
pub enum SupportError {
    #[error("a `{kind}` needs `{field}`")]
    Missing {
        kind: &'static str,
        field: &'static str,
    },
    #[error("a `{kind}` does not take `{field}`")]
    Unexpected {
        kind: &'static str,
        field: &'static str,
    },
}

#[derive(Copy, Clone)]
pub enum SupportKind {
    Haste,
    Paralyse,
    MagicShield,
    Cure,
}

impl SupportKind {
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "haste" => Some(Self::Haste),
            "paralyse" => Some(Self::Paralyse),
            "magic_shield" => Some(Self::MagicShield),
            "cure" => Some(Self::Cure),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Haste => "haste",
            Self::Paralyse => "paralyse",
            Self::MagicShield => "magic_shield",
            Self::Cure => "cure",
        }
    }
}

pub struct RawSupportFields {
    pub speed: Option<RawSpeedFormula>,
    pub duration_ticks: Option<TickDelta>,
    pub element: Option<CombatElement>,
}

pub fn build_support(
    kind: SupportKind,
    fields: RawSupportFields,
) -> Result<SupportEffect, SupportError> {
    let missing = |field| SupportError::Missing {
        kind: kind.name(),
        field,
    };
    let unexpected = |field| SupportError::Unexpected {
        kind: kind.name(),
        field,
    };
    let RawSupportFields {
        speed,
        duration_ticks,
        element,
    } = fields;

    match kind {
        SupportKind::Haste | SupportKind::Paralyse => {
            if element.is_some() {
                return Err(unexpected("element"));
            }
            let effect = match kind {
                SupportKind::Haste => SpeedEffect::Haste,
                _ => SpeedEffect::Paralysis,
            };
            Ok(SupportEffect::Speed {
                effect,
                formula: speed.ok_or_else(|| missing("speed"))?.into(),
                duration: duration_ticks.ok_or_else(|| missing("duration_ticks"))?,
            })
        }
        SupportKind::MagicShield => {
            if speed.is_some() {
                return Err(unexpected("speed"));
            }
            if element.is_some() {
                return Err(unexpected("element"));
            }
            Ok(SupportEffect::MagicShield {
                duration: duration_ticks.ok_or_else(|| missing("duration_ticks"))?,
            })
        }
        SupportKind::Cure => {
            if speed.is_some() {
                return Err(unexpected("speed"));
            }
            if duration_ticks.is_some() {
                return Err(unexpected("duration_ticks"));
            }
            Ok(SupportEffect::Cure {
                element: element.ok_or_else(|| missing("element"))?,
            })
        }
    }
}

use crate::{
    entities::{
        combat::CombatElement,
        conditions::SpeedEffect,
        effects::{EffectId, MissileId},
        targeting::TargetMode,
    },
    game::TickDelta,
};

/// One end of a speed roll: `factor × the target's base speed + flat`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedTerm {
    pub factor: f32,
    pub flat: i16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeedFormula {
    pub min: SpeedTerm,
    pub max: SpeedTerm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SupportEffect {
    Speed {
        effect: SpeedEffect,
        formula: SpeedFormula,
        duration: TickDelta,
    },
    MagicShield {
        duration: TickDelta,
    },
    Cure {
        element: CombatElement,
    },
}

#[derive(Debug, Clone)]
pub struct SupportCast {
    pub target: TargetMode,
    pub effect: SupportEffect,
    pub effect_id: Option<EffectId>,
    pub missile_id: Option<MissileId>,
}

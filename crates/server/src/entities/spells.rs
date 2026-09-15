use std::sync::Arc;

use strum::EnumCount;

use crate::{
    entities::{
        agent::{AgentId, AgentKey},
        combat::CombatElement,
        effects::{AreaShape, EffectId, MissileId},
        position::Position,
        vocation::Vocation,
    },
    game::{TickDelta, config::GAME_CONFIG},
};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SpellId(pub u16);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize, EnumCount)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SpellGroup {
    Attack = 0,
    Healing = 1,
    Support = 2,
}

impl SpellGroup {
    pub fn index(&self) -> usize {
        *self as usize
    }

    pub fn as_id(&self) -> u8 {
        *self as u8
    }

    pub fn cooldown(&self) -> TickDelta {
        match self {
            SpellGroup::Attack => GAME_CONFIG.combat.attack_group_cooldown,
            SpellGroup::Healing => GAME_CONFIG.combat.healing_group_cooldown,
            SpellGroup::Support => GAME_CONFIG.combat.support_group_cooldown,
        }
    }
}

#[derive(Debug)]
pub struct Spell {
    pub id: SpellId,
    pub name: String,
    pub words: String,
    pub group: SpellGroup,
    pub group_cooldown: Option<TickDelta>,
    pub cooldown: TickDelta,
    pub mana: u32,
    pub level: u16,
    pub icon: u16,
    pub vocations: Vec<Vocation>,
    pub effect: SpellEffect,
}

impl Spell {
    /// Whether a cast needs an agent or a tile to aim at.
    pub fn is_aimable(&self) -> bool {
        let target = match &self.effect {
            SpellEffect::Attack(attack) => &attack.target,
            SpellEffect::Healing(healing) => &healing.target,
        };
        matches!(
            target,
            SpellTargetMode::Area {
                origin: AreaOrigin::Target,
                ..
            }
        )
    }
}

/// `level_factor` and `magic_factor` are percentages of `base_power`; `spread` is the
/// fraction either side of the centre the roll spans.
#[derive(Debug)]
pub struct PowerCurve {
    pub base_power: f32,
    pub level_factor: f32,
    pub magic_factor: f32,
    pub spread: f32,
}

#[derive(Debug)]
pub struct SpellAttack {
    pub target: SpellTargetMode,
    pub element: CombatElement,
    pub power: PowerCurve,
    pub effect_id: EffectId,
    pub missile_id: Option<MissileId>,
    #[allow(dead_code)]
    pub chain: Option<ChainAttack>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct ChainAttack {
    pub num_targets: u16,
    pub damage_factor: f32,
    pub delay_ticks: TickDelta,
    pub max_range: u16,
    pub sorting: ChainSorting,
    pub missile_id: MissileId,
    pub effect_id: Option<EffectId>,
    pub chain: Option<Box<ChainAttack>>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainSorting {
    Closest,
}

#[derive(Debug)]
pub struct SpellHealing {
    pub target: SpellTargetMode,
    pub power: PowerCurve,
}

#[derive(Debug)]
pub enum SpellEffect {
    Attack(SpellAttack),
    Healing(SpellHealing),
}

#[derive(Debug, Clone)]
pub enum SpellTargetMode {
    /// Accepts only CastTarget::None
    Caster,
    /// Accepts only CastTarget::None, the target is always
    /// agent.target()
    Target { range: u16 },
    /// if origin is caster accepts only CastTarget::None
    /// otherwise accepts only CastTarget::Agent or CastTarget::Position
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

/// Client target enum
#[derive(Debug, Clone)]
pub enum SpellTarget {
    None,
    Agent(AgentId),
    Position(Position),
}

/// World target enum
#[derive(Debug, Clone)]
pub enum CastTarget {
    None,
    Agent(AgentKey),
    Position(Position),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Its twin is `group_ids_match_the_server` in the client's `core/spells.rs`. If the two
    /// disagree, a cast shades the wrong group icon and slots, and nothing errors.
    #[test]
    fn the_group_wire_ids_are_pinned() {
        assert_eq!(SpellGroup::Attack.as_id(), 0);
        assert_eq!(SpellGroup::Healing.as_id(), 1);
        assert_eq!(SpellGroup::Support.as_id(), 2);
    }
}

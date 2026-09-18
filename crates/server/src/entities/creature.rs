use serde::Deserialize;

use crate::{
    entities::{
        Bounds,
        agent::{OutfitColors, OutfitId, Pool},
        combat::CombatElement,
        conditions::ConditionSpec,
        effects::{EffectId, MissileId},
        items::{FluidType, ItemId},
        spells::SpellGroup,
        support::SupportCast,
        targeting::TargetMode,
    },
    game::TickDelta,
};

pub type CreatureKindId = String;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BloodType {
    Blood,
    Poison,
}

impl BloodType {
    pub fn get_fluid(&self) -> FluidType {
        match self {
            BloodType::Blood => FluidType::Blood,
            BloodType::Poison => FluidType::Slime,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LootEntry {
    pub item_id: ItemId,
    pub chance: u32,
    pub amount: u32,
}

#[derive(Clone, Debug)]
pub struct CreatureVoices {
    pub cooldown: TickDelta,
    pub chance: u32,
    pub sentences: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CreatureAttackDamage {
    pub element: CombatElement,
    pub value: Bounds,
    pub condition: Option<ConditionSpec>,
}

#[derive(Clone, Debug)]
pub struct CreatureAttack {
    pub damage: CreatureAttackDamage,
    pub target: TargetMode,
    pub effect_id: Option<EffectId>,
    pub missile_id: Option<MissileId>,
}

#[derive(Clone, Debug)]
pub struct ConditionAttack {
    pub condition: ConditionSpec,
    pub target: TargetMode,
    pub effect_id: Option<EffectId>,
    pub missile_id: Option<MissileId>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreatureFlag {
    ImmuneParalysis,
}

#[derive(Clone, Debug)]
pub enum AbilityEffect {
    Attack(CreatureAttack),
    Heal(Bounds),
    Support(SupportCast),
    Condition(ConditionAttack),
}

impl AbilityEffect {
    pub fn cooldown_group(&self) -> SpellGroup {
        match self {
            AbilityEffect::Attack { .. } => SpellGroup::Attack,
            AbilityEffect::Heal { .. } => SpellGroup::Healing,
            AbilityEffect::Support(..) => SpellGroup::Support,
            AbilityEffect::Condition(..) => SpellGroup::Attack,
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(transparent)]
pub struct CreatureAbilityId(pub u8);

#[derive(Clone, Debug)]
pub struct CreatureAbility {
    pub id: CreatureAbilityId,
    pub cooldown: TickDelta,
    pub chance: u32,
    pub effect: AbilityEffect,
}

#[derive(Clone, Debug)]
pub struct CreatureKind {
    pub name: String,
    pub life: Pool,
    pub outfit: (OutfitId, OutfitColors),
    pub speed: u16,
    pub melee: CreatureAttackDamage,
    pub abilities: Vec<CreatureAbility>,
    pub blood_type: BloodType,
    pub armor: u16,
    pub defense: u16,
    pub experience: u32,
    pub corpse: ItemId,
    pub loot_table: Vec<LootEntry>,
    pub flee_threshold: Option<u32>,
    pub say: CreatureVoices,
    pub flags: Vec<CreatureFlag>,
}

impl CreatureKind {
    pub fn has_flag(&self, flag: CreatureFlag) -> bool {
        self.flags.contains(&flag)
    }

    pub fn get_ability_effect(&self, id: CreatureAbilityId) -> Option<&AbilityEffect> {
        self.abilities
            .iter()
            .find(|a| a.id == id)
            .map(|a| &a.effect)
    }
}

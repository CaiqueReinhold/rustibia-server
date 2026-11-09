use smallvec::SmallVec;
use thiserror::Error;

use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::{Agent, AgentKey},
        combat::{AttackCost, AttackPlan, CombatDamage},
        conditions::ConditionAttack,
        effects::{AreaEffect, Missile},
        map::GameMap,
        player::Player,
        position::{Position, Rect},
        skills::SkillType,
        spells::{
            ChainAttack, ChainSorting, PowerCurve, Spell, SpellAttack, SpellDelivery, SpellEffect,
            SpellField, SpellGroup, SpellHealing,
        },
        support::{SupportCast, SupportEffect},
        targeting::{AreaOrigin, AreaTarget, TargetFilter, TargetMode},
    },
    game::{
        Tick, TickCtx,
        combat::{execute_attack, plan_spell_attack, spell_missile},
        conditions::apply_condition,
        events::BroadcastMessage,
        fields::{can_hold_field, can_place_field, create_fields},
        healing::{execute_healing, plan_healing_spell},
        map_query::{can_target, can_throw},
        random::Rolls,
        skills::tick_skill,
        support::cast_support,
    },
    persistence::items::ITEM_CONFIGS,
};

#[derive(Error, Debug, Clone)]
pub enum SpellCastingDenyReason {
    #[error("Invalid State: {0:?} {1}")]
    InvalidState(AgentKey, &'static str),
    #[error("Invalid spell")]
    IdNotFound,
    #[error("Not enough mana")]
    NoMana,
    #[error("You can't cast that spell")]
    RequirementFailed,
    #[error("No target")]
    InvalidTarget,
    #[error("Target out of reach")]
    OutOfReach,
    #[error("You're exausted")]
    StillInCooldown,
    #[error("You need a weapon")]
    NoWeapon,
    #[error("There is not enough room.")]
    NotEnoughRoom,
}

impl SpellCastingDenyReason {
    pub fn message(&self, via: SpellDelivery) -> String {
        match (self, via) {
            (Self::RequirementFailed, SpellDelivery::Rune) => {
                "You do not have the requirements to use this object.".to_owned()
            }
            (Self::StillInCooldown, SpellDelivery::Rune) => "You are exhausted.".to_owned(),
            (Self::OutOfReach, SpellDelivery::Rune) => "Destination is out of range.".to_owned(),
            (Self::InvalidTarget | Self::IdNotFound, SpellDelivery::Rune) => {
                "You cannot use this object.".to_owned()
            }
            _ => self.to_string(),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum CastSource {
    Words,
    Rune,
}

impl CastSource {
    fn delivers(&self, delivery: SpellDelivery) -> bool {
        matches!(
            (self, delivery),
            (CastSource::Words, SpellDelivery::Words) | (CastSource::Rune, SpellDelivery::Rune)
        )
    }
}

pub fn cast_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    spell: &Spell,
    target: AreaTarget,
    param: Option<String>,
    source: CastSource,
) -> Result<(), SpellCastingDenyReason> {
    let Some(position) = ctx.map.agent_position(agent_key).cloned() else {
        return Err(SpellCastingDenyReason::InvalidState(
            agent_key,
            "spell caster not found",
        ));
    };
    let Some(player) = ctx.map.get_player(agent_key) else {
        return Err(SpellCastingDenyReason::InvalidState(
            agent_key,
            "non player casting spell",
        ));
    };
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        return Err(SpellCastingDenyReason::InvalidState(
            agent_key,
            "spell caster not found",
        ));
    };

    let refusal = if !source.delivers(spell.delivery) {
        Some(SpellCastingDenyReason::IdNotFound)
    } else if !has_spell_requirements(spell, player) {
        Some(SpellCastingDenyReason::RequirementFailed)
    } else if !agent.mana().can_afford(spell.mana) {
        Some(SpellCastingDenyReason::NoMana)
    } else if !can_cast_spell(agent, spell, ctx.tick) {
        Some(SpellCastingDenyReason::StillInCooldown)
    } else {
        None
    };

    if let Some(reason) = refusal {
        return deny(ctx, agent_key, position, spell.delivery, reason);
    }

    let mark = ctx.mark();
    match execute_effect(ctx, agent_key, spell, target, param.as_deref()) {
        Err(reason) => {
            ctx.rollback_to(mark);
            deny(ctx, agent_key, position, spell.delivery, reason)
        }
        Ok(()) => {
            ctx.events.push(BroadcastMessage::SpellCast {
                agent_key,
                position,
                spell_id: spell.id,
            });
            Ok(())
        }
    }
}

fn deny(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    position: Position,
    delivery: SpellDelivery,
    reason: SpellCastingDenyReason,
) -> Result<(), SpellCastingDenyReason> {
    ctx.events.push(BroadcastMessage::SpellDenied {
        agent_key,
        position,
        reason: reason.clone(),
        delivery,
    });
    Err(reason)
}

pub struct ResolvedTargets {
    /// Includes the caster when an area covers it; a planner that must not hit its own
    /// caster filters this itself.
    pub keys: Vec<AgentKey>,
    /// The single target's tile, or an area's centre. `None` for a cast on the caster.
    pub aim: Option<Position>,
    pub delta: Option<Vec<(i8, i8)>>,
}

pub fn resolve_targets(
    map: &GameMap,
    caster: AgentKey,
    mode: &TargetMode,
    area_target: &AreaTarget,
    param: Option<&str>,
    filter: TargetFilter,
) -> Result<ResolvedTargets, SpellCastingDenyReason> {
    let agent = map
        .get_agent(caster)
        .ok_or(SpellCastingDenyReason::InvalidState(
            caster,
            "spell caster not found",
        ))?;
    let position = map
        .agent_position(caster)
        .ok_or(SpellCastingDenyReason::InvalidState(
            caster,
            "spell caster not found",
        ))?;

    match mode {
        TargetMode::Caster => Ok(ResolvedTargets {
            keys: Vec::from([caster]),
            aim: None,
            delta: None,
        }),
        TargetMode::Target { range } => {
            let target = agent
                .target()
                .ok_or(SpellCastingDenyReason::InvalidTarget)?;
            let target_pos =
                map.agent_position(target)
                    .ok_or(SpellCastingDenyReason::InvalidState(
                        target,
                        "spell target not found",
                    ))?;

            if position.distance(target_pos) > *range || !can_throw(map, position, target_pos, true)
            {
                return Err(SpellCastingDenyReason::OutOfReach);
            }

            if !passes(map, target, filter) {
                return Err(SpellCastingDenyReason::InvalidTarget);
            }

            Ok(ResolvedTargets {
                keys: Vec::from([target]),
                aim: Some(target_pos.clone()),
                delta: Some(vec![(0, 0)]),
            })
        }
        TargetMode::Named { range } => {
            let name = param.ok_or(SpellCastingDenyReason::InvalidTarget)?;
            let (key, target_pos) = map
                .iter_agents_in_rect(&Rect::radius(position, (*range, *range)), position.z)
                .find(|(key, _)| {
                    map.get_player(*key)
                        .is_some_and(|player| player.name() == name)
                })
                .ok_or(SpellCastingDenyReason::InvalidTarget)?;

            if position.distance(&target_pos) > *range
                || !can_throw(map, position, &target_pos, true)
            {
                return Err(SpellCastingDenyReason::OutOfReach);
            }

            Ok(ResolvedTargets {
                keys: Vec::from([key]),
                aim: Some(target_pos),
                delta: Some(vec![(0, 0)]),
            })
        }
        TargetMode::Aimed => {
            let AreaTarget::Agent(key) = area_target else {
                return Err(SpellCastingDenyReason::InvalidTarget);
            };
            let target_pos =
                map.agent_position(*key)
                    .ok_or(SpellCastingDenyReason::InvalidState(
                        *key,
                        "spell target not found",
                    ))?;

            if !can_target(position, target_pos) || !can_throw(map, position, target_pos, true) {
                return Err(SpellCastingDenyReason::OutOfReach);
            }

            if !passes(map, *key, filter) {
                return Err(SpellCastingDenyReason::InvalidTarget);
            }

            Ok(ResolvedTargets {
                keys: Vec::from([*key]),
                aim: Some(target_pos.clone()),
                delta: Some(vec![(0, 0)]),
            })
        }
        TargetMode::Area {
            origin: area_origin,
            shape,
        } => {
            let origin = resolve_area_origin(map, area_origin, position, area_target)
                .ok_or(SpellCastingDenyReason::InvalidTarget)?;
            let delta = match area_origin {
                AreaOrigin::Caster => shape.get_delta_facing(agent.facing()),
                AreaOrigin::Target => shape.get_delta_towards(
                    (
                        origin.x as i32 - position.x as i32,
                        origin.y as i32 - position.y as i32,
                    ),
                    agent.facing(),
                ),
            };
            let (mut keys, delta) = resolve_area(map, origin, delta);
            keys.retain(|key| passes(map, *key, filter));
            Ok(ResolvedTargets {
                keys,
                aim: Some(origin.clone()),
                delta: Some(delta),
            })
        }
        TargetMode::Chain { count, hop_range } => {
            let mut keys: Vec<AgentKey> = Vec::new();
            let mut from = position.clone();
            while keys.len() < usize::from(*count) {
                let squared = |pos: &Position| {
                    let (dx, dy) = (
                        i32::from(pos.x) - i32::from(from.x),
                        i32::from(pos.y) - i32::from(from.y),
                    );
                    dx * dx + dy * dy
                };
                let Some((key, pos)) = map
                    .iter_agents_in_rect(&Rect::radius(&from, (*hop_range, *hop_range)), from.z)
                    .filter(|(key, pos)| {
                        *key != caster
                            && !keys.contains(key)
                            && passes(map, *key, filter)
                            && can_throw(map, &from, pos, true)
                    })
                    .min_by_key(|(_, pos)| squared(pos))
                else {
                    break;
                };
                keys.push(key);
                from = pos;
            }
            if keys.is_empty() {
                return Err(SpellCastingDenyReason::InvalidTarget);
            }
            Ok(ResolvedTargets {
                keys,
                aim: None,
                delta: None,
            })
        }
    }
}

/// The curve's centre scaled by the caster's level and magic level, rolled across its spread.
pub fn roll_power(player: &Player, curve: &PowerCurve, roll: &mut Rolls, use_weapon: bool) -> u32 {
    let weapon_attack = if use_weapon {
        player.weapon_attack() as f32
    } else {
        0.0
    };
    let melee_skill = player
        .weapon_type()
        .skill()
        .map(|skill_type| player.skill(skill_type) as f32)
        .unwrap_or(0.0);
    let center = curve.base_power
        * (1.0
            + (f32::from(player.skill(SkillType::Magic)) * curve.magic_factor / 100.0)
            + ((melee_skill + weapon_attack) * curve.melee_factor / 100.0))
        + f32::from(player.level()) * curve.level_factor
        + curve.flat;
    let min = (center * (1.0 - curve.spread_min)).max(0.0).round() as u32;
    let max = (center * (1.0 + curve.spread_max)).max(0.0).round() as u32;
    roll.damage_roll(min, max)
}

fn resolve_area_origin<'a>(
    map: &'a GameMap,
    origin: &'a AreaOrigin,
    caster_pos: &'a Position,
    area_target: &'a AreaTarget,
) -> Option<&'a Position> {
    match origin {
        AreaOrigin::Caster => Some(caster_pos),
        AreaOrigin::Target => {
            let candidate = match area_target {
                AreaTarget::Agent(key) => map.agent_position(*key),
                AreaTarget::Position(pos) => Some(pos),
                AreaTarget::None => None,
            };
            candidate
                .filter(|pos| can_target(caster_pos, pos) && can_throw(map, caster_pos, pos, true))
        }
    }
}

fn passes(map: &GameMap, key: AgentKey, filter: TargetFilter) -> bool {
    match filter {
        TargetFilter::Any => map.get_agent(key).is_some_and(|a| !a.is_npc()),
        TargetFilter::Players => map.get_player(key).is_some(),
        TargetFilter::Creatures => map.get_agent(key).is_some_and(Agent::is_creature),
        TargetFilter::RangedCreatures => map
            .get_agent(key)
            .and_then(Agent::get_creature_kind)
            .is_some_and(|kind| kind.target_distance > 1),
    }
}

pub fn resolve_area(
    map: &GameMap,
    origin: &Position,
    shape: &[(i8, i8)],
) -> (Vec<AgentKey>, Vec<(i8, i8)>) {
    let affected_area: Vec<((i8, i8), Position)> = shape
        .iter()
        .flat_map(|(dx, dy)| {
            origin
                .checked_offset(*dx as i32, *dy as i32)
                .map(|pos| ((*dx, *dy), pos))
        })
        .filter(|(_, pos)| can_throw(map, origin, pos, true))
        .collect();

    (
        affected_area
            .iter()
            .flat_map(|(_, pos)| map.iter_agents_at(pos).ok())
            .flatten()
            .copied()
            .collect(),
        affected_area.into_iter().map(|(delta, _)| delta).collect(),
    )
}

pub fn consume_mana(ctx: &mut TickCtx, agent_key: AgentKey, mana_cost: u32) {
    let Some(mut agent) = ctx.map.agent_mut(agent_key) else {
        return;
    };
    agent.remove_mana(mana_cost);
    tick_skill(ctx, agent_key, SkillType::Magic, mana_cost as u64);
}

pub fn chain_attack(
    ctx: &mut TickCtx,
    attacker: AgentKey,
    sources: Vec<(Position, CombatDamage)>,
    chain: ChainAttack,
    mut targeted: Vec<AgentKey>,
) {
    let mut hits = Vec::new();
    let mut positions = Vec::new();

    for (target_pos, damage) in sources {
        let mut candidates: Vec<(AgentKey, Position)> = ctx
            .map
            .iter_agents_in_rect(
                &Rect::radius(&target_pos, (chain.max_range, chain.max_range)),
                target_pos.z,
            )
            .filter(|(key, _)| *key != attacker && !targeted.contains(key))
            .collect();
        match chain.sorting {
            ChainSorting::Closest => {
                candidates.sort_by_key(|(_, pos)| pos.distance(&target_pos));
            }
        }
        candidates.truncate(chain.num_targets as usize);

        for (agent, pos) in &candidates {
            ctx.events.push(BroadcastMessage::MissileLaunched {
                missile: Missile {
                    from: target_pos.clone(),
                    to: pos.clone(),
                    missile_id: chain.missile_id,
                },
            });
            hits.push((
                *agent,
                CombatDamage {
                    element: damage.element,
                    value: ((damage.value as f32) * chain.damage_factor) as u32,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            ));
            positions.push(pos.clone());
            targeted.push(*agent);
        }
    }

    let plan = AttackPlan {
        attacker,
        damage: SmallVec::from(hits.clone()),
        cost: AttackCost::None,
        trains: None,
        missile: None,
        area_effect: None,
        missed: false,
        condition: None,
    };
    execute_attack(ctx, plan);

    if let Some(effect_id) = chain.effect_id {
        for pos in &positions {
            ctx.events.push(BroadcastMessage::AreaEffectAppeared {
                area_effect: AreaEffect::single(effect_id, pos.clone()),
            });
        }
    }

    if let Some(new_chain) = chain.chain.map(|c| *c) {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + new_chain.delay_ticks,
            command: WorldCommand::ChainAttack {
                attacker,
                sources: hits
                    .into_iter()
                    .zip(positions)
                    .map(|((_, dmg), pos)| (pos, dmg))
                    .collect(),
                chain: new_chain.clone(),
                targeted,
            },
        })
    }
}

fn has_spell_requirements(spell: &Spell, player: &Player) -> bool {
    spell.level <= player.level()
        && spell.magic_level <= player.skill(SkillType::Magic)
        && (spell.vocations.is_empty() || spell.vocations.contains(&player.vocation()))
}

fn can_cast_spell(agent: &Agent, spell: &Spell, current_tick: Tick) -> bool {
    agent.next_spell_group_tick(spell.group) <= current_tick
        && agent.next_spell_tick(spell.id) <= current_tick
}

fn execute_effect(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    spell: &Spell,
    target: AreaTarget,
    param: Option<&str>,
) -> Result<(), SpellCastingDenyReason> {
    match &spell.effect {
        SpellEffect::Attack(attack) => attack_spell(ctx, agent_key, &target, param, attack),
        SpellEffect::Healing(healing) => healing_spell(ctx, agent_key, &target, param, healing),
        SpellEffect::Support(support) => support_spell(ctx, agent_key, &target, param, support),
        SpellEffect::Field(field) => field_spell(ctx, agent_key, &target, param, field),
        SpellEffect::Condition(attack) => condition_spell(ctx, agent_key, &target, param, attack),
    }?;

    let mut agent = ctx
        .map
        .agent_mut(agent_key)
        .ok_or(SpellCastingDenyReason::InvalidState(
            agent_key,
            "missing after casting spell sucessfully",
        ))?;
    agent.stamp_spell(ctx.tick, spell);
    if matches!(spell.group, SpellGroup::Attack) {
        agent.stamp_auto_attack(ctx.tick);
    }
    consume_mana(ctx, agent_key, spell.mana);

    Ok(())
}

fn attack_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    spell_attack: &SpellAttack,
) -> Result<(), SpellCastingDenyReason> {
    let plan = plan_spell_attack(ctx.map, agent_key, ctx.roll, spell_attack, target, param)?;
    let chain = spell_attack
        .chain
        .as_ref()
        .and_then(|chain| plan.damage.first().map(|(t, d)| (chain, *t, d.clone())));

    if let Some((chain, target, damage)) = chain
        && let Some(target_pos) = ctx.map.agent_position(target)
    {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + chain.delay_ticks,
            command: WorldCommand::ChainAttack {
                attacker: agent_key,
                sources: vec![(target_pos.clone(), damage)],
                chain: chain.clone(),
                targeted: vec![target],
            },
        });
    }

    execute_attack(ctx, plan);
    Ok(())
}

fn healing_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    spell_healing: &SpellHealing,
) -> Result<(), SpellCastingDenyReason> {
    let plan = plan_healing_spell(ctx.map, agent_key, ctx.roll, spell_healing, target, param)?;
    let healed: SmallVec<[AgentKey; 1]> = plan.restores.iter().map(|(key, _)| *key).collect();
    execute_healing(ctx, plan);
    for key in healed {
        if let Some(mut agent) = ctx.map.agent_mut(key) {
            agent.conditions(|c| c.cure_paralysis());
        }
    }
    Ok(())
}

fn support_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    cast: &SupportCast,
) -> Result<(), SpellCastingDenyReason> {
    let filter = match (&cast.effect, &cast.target) {
        (SupportEffect::Challenge { melee: Some(_), .. }, _) => TargetFilter::RangedCreatures,
        (SupportEffect::Challenge { melee: None, .. }, _) => TargetFilter::Creatures,
        (_, TargetMode::Aimed) => TargetFilter::Any,
        _ => TargetFilter::Players,
    };
    let targets = resolve_targets(ctx.map, agent_key, &cast.target, target, param, filter)?;
    cast_support(ctx, agent_key, cast, targets);
    Ok(())
}

fn condition_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    attack: &ConditionAttack,
) -> Result<(), SpellCastingDenyReason> {
    let mut targets = resolve_targets(
        ctx.map,
        agent_key,
        &attack.target,
        target,
        param,
        TargetFilter::Any,
    )?;
    targets.keys.retain(|key| *key != agent_key);
    if matches!(attack.target, TargetMode::Aimed) && targets.keys.is_empty() {
        return Err(SpellCastingDenyReason::InvalidTarget);
    }

    let missile_id = attack
        .missile_id
        .and_then(|source| spell_missile(source, ctx.map.get_player(agent_key)?));
    if let Some(aim) = &targets.aim
        && let Some(from) = ctx.map.agent_position(agent_key).cloned()
    {
        if let Some(missile_id) = missile_id {
            ctx.events.push(BroadcastMessage::MissileLaunched {
                missile: Missile {
                    missile_id,
                    from,
                    to: aim.clone(),
                },
            });
        }
        if let (Some(effect_id), Some(delta)) = (attack.effect_id, targets.delta) {
            ctx.events.push(BroadcastMessage::AreaEffectAppeared {
                area_effect: AreaEffect {
                    effect_id,
                    origin: aim.clone(),
                    delta,
                },
            });
        }
    }
    for key in targets.keys {
        apply_condition(ctx, key, &attack.condition, Some(agent_key));
    }
    Ok(())
}

fn field_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    field: &SpellField,
) -> Result<(), SpellCastingDenyReason> {
    let targets = resolve_targets(
        ctx.map,
        agent_key,
        &field.target,
        target,
        param,
        TargetFilter::Any,
    )?;
    let (Some(aim), Some(delta)) = (targets.aim, targets.delta) else {
        return Err(SpellCastingDenyReason::InvalidTarget);
    };
    if !can_hold_field(ctx.map, &aim) {
        return Err(SpellCastingDenyReason::NotEnoughRoom);
    }
    let config = ITEM_CONFIGS
        .get(&field.item)
        .ok_or(SpellCastingDenyReason::InvalidState(
            agent_key,
            "field spell names an item not in the catalogue",
        ))?;
    if !can_place_field(ctx.map, &aim, config) {
        return Err(SpellCastingDenyReason::NotEnoughRoom);
    }

    let area: Vec<Position> = delta
        .iter()
        .filter_map(|(dx, dy)| aim.checked_offset(*dx as i32, *dy as i32))
        .collect();
    create_fields(ctx, agent_key, config, &area);

    if let Some(missile_id) = field.missile_id
        && let Some(from) = ctx.map.agent_position(agent_key).cloned()
    {
        ctx.events.push(BroadcastMessage::MissileLaunched {
            missile: Missile {
                missile_id,
                from,
                to: aim,
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::entities::world_map::WorldMap;
    use std::sync::Arc;

    use super::*;
    use crate::entities::Bounds;
    use crate::entities::agent::Facing;
    use crate::entities::combat::CombatElement;
    use crate::entities::combat::WeaponType;
    use crate::entities::conditions::{ConditionSpec, SpecSchedule, SpeedEffect};
    use crate::entities::creature::CreatureKind;
    use crate::entities::effects::AreaShape;
    use crate::entities::effects::EffectId;
    use crate::entities::effects::{MissileId, MissileSource};
    use crate::entities::inventory::InventorySlot;
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag, ItemId};
    use crate::entities::map::MapTile;
    use crate::entities::skills::SkillValue;
    use crate::entities::vocation::Vocation;
    use crate::game::TestHarness;
    use crate::game::TickDelta;
    use crate::persistence::test_fixtures::{
        a_creature_kind, a_spell, a_test_creature, a_test_snapshot,
    };

    fn a_caster_at(position: &Position) -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), position)
            .unwrap();
        (map, key)
    }

    /// Places `agent` next to the caster and hands back both keys.
    fn beside(map: &mut GameMap, position: &Position, agent: Agent) -> AgentKey {
        let spot = Position::new(position.x + 1, position.y, position.z);
        map.insert_tile(spot.clone(), MapTile::new());
        map.insert_agent(agent, &spot).unwrap()
    }

    /// Symmetric under rotation, so the caster's facing cannot move a tile out of it.
    fn a_cross_area() -> TargetMode {
        TargetMode::Area {
            origin: AreaOrigin::Caster,
            shape: Arc::new(AreaShape::new(
                vec![(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)].into_boxed_slice(),
            )),
        }
    }

    fn aimed_area() -> TargetMode {
        TargetMode::Area {
            origin: AreaOrigin::Target,
            shape: Arc::new(AreaShape::new(vec![(0, 0)].into_boxed_slice())),
        }
    }

    fn a_player_at(position: &Position, level: u16, magic: u16) -> (GameMap, AgentKey) {
        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.skills.insert(
            SkillType::Level,
            SkillValue {
                value: level,
                current_ticks: 0,
            },
        );
        snapshot.skills.insert(
            SkillType::Magic,
            SkillValue {
                value: magic,
                current_ticks: 0,
            },
        );

        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(snapshot), position)
            .unwrap();
        (map, key)
    }

    #[test]
    fn an_aimed_cast_refuses_a_tile_and_refuses_naming_nothing() {
        let position = Position::new(100, 100, 7);
        let (map, caster) = a_caster_at(&position);

        for named in [AreaTarget::None, AreaTarget::Position(position.clone())] {
            let refused = resolve_targets(
                &map,
                caster,
                &TargetMode::Aimed,
                &named,
                None,
                TargetFilter::Any,
            );
            assert!(
                matches!(refused, Err(SpellCastingDenyReason::InvalidTarget)),
                "{named:?} resolved"
            );
        }
    }

    #[test]
    fn an_aimed_cast_resolves_an_agent_and_still_honours_the_filter() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let creature = beside(&mut map, &position, a_test_creature("Rat", 10, (1, 2)));

        let hit = resolve_targets(
            &map,
            caster,
            &TargetMode::Aimed,
            &AreaTarget::Agent(creature),
            None,
            TargetFilter::Any,
        )
        .unwrap();
        assert_eq!(hit.keys, Vec::from([creature]));

        let healed = resolve_targets(
            &map,
            caster,
            &TargetMode::Aimed,
            &AreaTarget::Agent(creature),
            None,
            TargetFilter::Players,
        );
        assert!(matches!(healed, Err(SpellCastingDenyReason::InvalidTarget)));
    }

    #[test]
    fn a_rune_refusal_is_worded_as_an_object_not_a_spell() {
        assert_eq!(
            SpellCastingDenyReason::RequirementFailed.message(SpellDelivery::Rune),
            "You do not have the requirements to use this object."
        );
        assert_eq!(
            SpellCastingDenyReason::StillInCooldown.message(SpellDelivery::Rune),
            "You are exhausted."
        );
        assert_eq!(
            SpellCastingDenyReason::RequirementFailed.message(SpellDelivery::Words),
            SpellCastingDenyReason::RequirementFailed.to_string()
        );
    }

    #[test]
    fn a_rune_spell_cannot_be_cast_by_words() {
        let position = Position::new(100, 100, 7);
        let (map, caster) = a_caster_at(&position);
        let mut spell = a_spell(1, 0, Vec::new());
        spell.delivery = SpellDelivery::Rune;
        let mut h = TestHarness::new();

        let mut map = WorldMap::new(map);
        let result = cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        );

        assert!(matches!(result, Err(SpellCastingDenyReason::IdNotFound)));
        assert!(matches!(
            h.events.as_slice(),
            [BroadcastMessage::SpellDenied {
                delivery: SpellDelivery::Rune,
                ..
            }]
        ));
    }

    #[test]
    fn a_word_spell_cannot_be_cast_by_a_rune() {
        let position = Position::new(100, 100, 7);
        let (map, caster) = a_caster_at(&position);
        let spell = a_spell(1, 0, Vec::new());
        let mut h = TestHarness::new();

        let mut map = WorldMap::new(map);
        let result = cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Rune,
        );

        assert!(matches!(result, Err(SpellCastingDenyReason::IdNotFound)));
    }

    #[test]
    fn a_heal_lifts_its_targets_paralysis() {
        let (map, caster) = a_caster_at(&Position::new(100, 100, 7));
        let mut map = WorldMap::new(map);
        let base = map.get_agent(caster).unwrap().speed();
        map.agent_mut(caster)
            .unwrap()
            .conditions(|c| c.apply_speed(SpeedEffect::Paralysis, -60));
        map.take_delta();
        let mut h = TestHarness::seeded(1);

        cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &a_spell(1, 0, Vec::new()),
            AreaTarget::None,
            None,
            CastSource::Words,
        )
        .unwrap();

        assert_eq!(map.get_agent(caster).unwrap().speed(), base);
        assert!(map.delta().agent(caster).speed());
    }

    #[test]
    fn a_condition_spell_afflicts_its_target_without_a_hit_and_never_its_caster() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let rat = beside(&mut map, &position, a_test_creature("Rat", 100, (1, 2)));
        let mut spell = a_spell(1, 0, Vec::new());
        spell.effect = SpellEffect::Condition(ConditionAttack {
            condition: ConditionSpec {
                element: CombatElement::Fire,
                interval: TickDelta(60),
                schedule: SpecSchedule::Flat {
                    damage: Bounds { min: 10, max: 10 },
                    count: 3,
                },
                delayed: true,
            },
            target: TargetMode::Aimed,
            effect_id: None,
            missile_id: Some(MissileSource::Fixed(MissileId(4))),
        });
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        let on_self = cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::Agent(caster),
            None,
            CastSource::Words,
        );
        assert!(matches!(
            on_self,
            Err(SpellCastingDenyReason::InvalidTarget)
        ));

        cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::Agent(rat),
            None,
            CastSource::Words,
        )
        .unwrap();

        assert_eq!(map.get_agent(rat).unwrap().life().current, 100);
        assert!(
            h.events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::MissileLaunched { .. }))
        );
        assert!(
            !h.events
                .iter()
                .any(|e| matches!(e, BroadcastMessage::DamageTaken { .. }))
        );
        assert!(h.scheduled.iter().any(|s| matches!(
            s.command,
            WorldCommand::DamageOverTimeTick { agent_key, .. } if agent_key == rat
        )));
    }

    fn a_player_holding(weapon: Option<Item>, at: &Position) -> (GameMap, AgentKey) {
        let mut snapshot = a_test_snapshot(1, 1);
        if let Some(weapon) = weapon {
            snapshot.inventory.insert(InventorySlot::LeftHand, weapon);
        }
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let key = map.insert_agent(Agent::from_player(snapshot), at).unwrap();
        (map, key)
    }

    fn an_axe() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(16),
                "axe".to_string(),
                None,
                None,
                Vec::new(),
                vec![
                    ItemAttribute::WeaponType(WeaponType::Axe),
                    ItemAttribute::WeaponAttack(10),
                ],
            )),
            1,
        )
    }

    fn launched(events: &[BroadcastMessage]) -> Vec<MissileId> {
        events
            .iter()
            .filter_map(|e| match e {
                BroadcastMessage::MissileLaunched { missile } => Some(missile.missile_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_weapon_attack_throws_the_casters_axe() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_player_holding(Some(an_axe()), &position);
        let rat = beside(&mut map, &position, a_test_creature("Rat", 100, (1, 2)));
        let mut spell = a_spell(1, 0, Vec::new());
        spell.effect = SpellEffect::Attack(SpellAttack {
            target: TargetMode::Aimed,
            element: CombatElement::Physical,
            power: PowerCurve {
                base_power: 1.0,
                level_factor: 0.0,
                magic_factor: 0.0,
                melee_factor: 0.0,
                spread_min: 0.0,
                spread_max: 0.0,
                flat: 0.0,
            },
            effect_id: EffectId(1),
            missile_id: Some(MissileSource::Weapon),
            chain: None,
            weapon_required: true,
        });
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::Agent(rat),
            None,
            CastSource::Words,
        )
        .unwrap();

        assert_eq!(
            launched(&h.events),
            [crate::game::config::GAME_CONFIG.weapon_missiles.axe]
        );
    }

    #[test]
    fn a_weapon_condition_cast_bare_handed_throws_nothing_but_still_lands() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_player_holding(None, &position);
        let rat = beside(&mut map, &position, a_test_creature("Rat", 100, (1, 2)));
        let mut spell = a_spell(1, 0, Vec::new());
        spell.effect = SpellEffect::Condition(ConditionAttack {
            condition: ConditionSpec {
                element: CombatElement::Physical,
                interval: TickDelta(40),
                schedule: SpecSchedule::Flat {
                    damage: Bounds { min: 10, max: 10 },
                    count: 3,
                },
                delayed: true,
            },
            target: TargetMode::Aimed,
            effect_id: None,
            missile_id: Some(MissileSource::Weapon),
        });
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::Agent(rat),
            None,
            CastSource::Words,
        )
        .unwrap();

        assert!(launched(&h.events).is_empty());
        assert!(h.scheduled.iter().any(|s| matches!(
            s.command,
            WorldCommand::DamageOverTimeTick { agent_key, .. } if agent_key == rat
        )));
    }

    #[test]
    fn a_spell_is_refused_below_its_magic_level_and_allowed_at_it() {
        let (map, key) = a_player_at(&Position::new(100, 100, 7), 30, 3);
        let player = map.get_player(key).expect("the player just inserted");

        assert!(!has_spell_requirements(&a_spell(1, 4, Vec::new()), player));
        assert!(has_spell_requirements(&a_spell(1, 3, Vec::new()), player));
    }

    #[test]
    fn an_empty_vocation_list_admits_every_vocation() {
        let (map, key) = a_player_at(&Position::new(100, 100, 7), 30, 3);
        let player = map.get_player(key).expect("the player just inserted");

        assert!(has_spell_requirements(&a_spell(1, 0, Vec::new()), player));
        assert!(!has_spell_requirements(
            &a_spell(1, 0, Vec::from([Vocation::Druid])),
            player
        ));
    }

    /// What lets `exura gran mas res` exist: the same area that hands a wave every agent
    /// hands a heal only the players among them.
    #[test]
    fn a_players_only_area_leaves_the_creatures_in_it_alone() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let creature = beside(&mut map, &position, a_test_creature("Rat", 10, (1, 2)));

        let any = resolve_targets(
            &map,
            caster,
            &a_cross_area(),
            &AreaTarget::None,
            None,
            TargetFilter::Any,
        )
        .unwrap();
        let players = resolve_targets(
            &map,
            caster,
            &a_cross_area(),
            &AreaTarget::None,
            None,
            TargetFilter::Players,
        )
        .unwrap();

        assert!(any.keys.contains(&creature));
        assert_eq!(players.keys, Vec::from([caster]));
    }

    #[test]
    fn a_named_cast_reaches_the_player_it_names_and_no_one_else() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let named = TargetMode::Named { range: 7 };

        let targets = resolve_targets(
            &map,
            caster,
            &named,
            &AreaTarget::None,
            Some("Rizael"),
            TargetFilter::Players,
        )
        .unwrap();
        assert_eq!(targets.keys, Vec::from([caster]));

        for param in [None, Some("Nobody")] {
            assert!(
                matches!(
                    resolve_targets(
                        &map,
                        caster,
                        &named,
                        &AreaTarget::None,
                        param,
                        TargetFilter::Players
                    ),
                    Err(SpellCastingDenyReason::InvalidTarget)
                ),
                "{param:?} must not resolve to a player"
            );
        }

        let far = Position::new(position.x + 20, position.y, position.z);
        map.insert_tile(far.clone(), MapTile::new());
        let mut stranger = a_test_snapshot(2, 2);
        stranger.name = "Stranger".to_string();
        map.insert_agent(Agent::from_player(stranger), &far)
            .unwrap();

        assert!(
            matches!(
                resolve_targets(
                    &map,
                    caster,
                    &named,
                    &AreaTarget::None,
                    Some("Stranger"),
                    TargetFilter::Players
                ),
                Err(SpellCastingDenyReason::InvalidTarget)
            ),
            "a player past the range is not in the rect the cast searches"
        );
    }

    /// A wall rune must lie across the throw whatever way its caster happens to be facing.
    #[test]
    fn an_aimed_area_turns_with_the_throw_rather_than_the_caster() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let aim = Position::new(103, 100, 7);
        for y in 98..=102 {
            map.insert_tile(Position::new(103, y, 7), MapTile::new());
        }
        map.get_agent_mut(caster).unwrap().set_facing(Facing::North);

        let targets = resolve_targets(
            &map,
            caster,
            &TargetMode::Area {
                origin: AreaOrigin::Target,
                shape: Arc::new(AreaShape::new(
                    vec![(-2, 0), (-1, 0), (0, 0), (1, 0), (2, 0)].into_boxed_slice(),
                )),
            },
            &AreaTarget::Position(aim.clone()),
            None,
            TargetFilter::Any,
        )
        .unwrap();

        let mut delta = targets.delta.unwrap();
        delta.sort_unstable();
        assert_eq!(delta, [(0, -2), (0, -1), (0, 0), (0, 1), (0, 2)]);
    }

    #[test]
    fn an_aimed_tile_within_reach_centres_the_area() {
        let (map, caster) = a_caster_at(&Position::new(100, 100, 7));
        let aim = Position::new(103, 101, 7);

        let targets = resolve_targets(
            &map,
            caster,
            &aimed_area(),
            &AreaTarget::Position(aim.clone()),
            None,
            TargetFilter::Any,
        )
        .unwrap();

        assert_eq!(targets.aim, Some(aim));
    }

    #[test]
    fn an_aimed_tile_out_of_reach_or_on_another_floor_is_refused() {
        let (map, caster) = a_caster_at(&Position::new(100, 100, 7));

        for aim in [Position::new(140, 100, 7), Position::new(101, 100, 6)] {
            assert!(
                matches!(
                    resolve_targets(
                        &map,
                        caster,
                        &aimed_area(),
                        &AreaTarget::Position(aim.clone()),
                        None,
                        TargetFilter::Any,
                    ),
                    Err(SpellCastingDenyReason::InvalidTarget)
                ),
                "{aim:?} must not be a legal aim"
            );
        }
    }

    #[test]
    fn a_field_aimed_where_one_cannot_go_is_refused_for_want_of_room() {
        let position = Position::new(100, 100, 7);
        let (mut map, caster) = a_caster_at(&position);
        let aim = Position::new(101, 100, 7);
        let mut tile = MapTile::new();
        for flag in [ItemFlag::Ground, ItemFlag::Unreplaceable] {
            tile.push_item(Item::new(
                Arc::new(ItemConfig::new(
                    ItemId(1),
                    "thing".to_string(),
                    None,
                    None,
                    [flag],
                    Vec::new(),
                )),
                1,
            ));
        }
        map.insert_tile(aim.clone(), tile);
        let mut spell = a_spell(0, 0, Vec::new());
        spell.effect = SpellEffect::Field(SpellField {
            target: aimed_area(),
            item: ItemId(9),
            missile_id: None,
        });
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::new();

        let refused = cast_spell(
            &mut h.ctx(&mut map),
            caster,
            &spell,
            AreaTarget::Position(aim),
            None,
            CastSource::Words,
        );

        assert!(matches!(
            refused,
            Err(SpellCastingDenyReason::NotEnoughRoom)
        ));
    }

    fn a_plain() -> GameMap {
        let mut map = GameMap::new();
        for x in 95..=120 {
            for y in 95..=110 {
                map.insert_tile(Position::new(x, y, 7), MapTile::new());
            }
        }
        map
    }

    fn put_at(map: &mut GameMap, x: u16, y: u16, agent: Agent) -> AgentKey {
        map.insert_agent(agent, &Position::new(x, y, 7)).unwrap()
    }

    fn an_archer() -> Agent {
        Agent::from_creature_kind(
            Arc::new(CreatureKind {
                target_distance: 4,
                ..a_creature_kind("Archer")
            }),
            Position::new(0, 0, 7),
        )
    }

    fn chained(
        map: &GameMap,
        caster: AgentKey,
        count: u16,
        filter: TargetFilter,
    ) -> Result<Vec<AgentKey>, SpellCastingDenyReason> {
        resolve_targets(
            map,
            caster,
            &TargetMode::Chain {
                count,
                hop_range: 6,
            },
            &AreaTarget::None,
            None,
            filter,
        )
        .map(|targets| targets.keys)
    }

    /// The caster at (100, 100) and a line of archers each 5 tiles from the last, only the
    /// first within 6 of the caster, plus one at (100, 102) nearer still.
    fn a_line_of_archers() -> (GameMap, AgentKey, [AgentKey; 4]) {
        let mut map = a_plain();
        let caster = put_at(
            &mut map,
            100,
            100,
            Agent::from_player(a_test_snapshot(1, 1)),
        );
        let nearest = put_at(&mut map, 100, 102, an_archer());
        let first = put_at(&mut map, 103, 100, an_archer());
        let second = put_at(&mut map, 108, 100, an_archer());
        let third = put_at(&mut map, 113, 100, an_archer());
        (map, caster, [nearest, first, second, third])
    }

    #[test]
    fn a_chain_takes_the_nearest_first_and_hops_on_from_each_pick() {
        let (map, caster, archers) = a_line_of_archers();

        assert_eq!(
            chained(&map, caster, 5, TargetFilter::RangedCreatures).unwrap(),
            archers.to_vec()
        );
    }

    #[test]
    fn a_chain_stops_at_its_count() {
        let (map, caster, [nearest, first, ..]) = a_line_of_archers();

        assert_eq!(
            chained(&map, caster, 2, TargetFilter::RangedCreatures).unwrap(),
            vec![nearest, first]
        );
    }

    #[test]
    fn a_chain_skips_what_it_cannot_see_and_what_its_filter_refuses() {
        let mut map = a_plain();
        let caster = put_at(
            &mut map,
            100,
            100,
            Agent::from_player(a_test_snapshot(1, 1)),
        );
        let rat = put_at(&mut map, 101, 100, a_test_creature("Rat", 10, (1, 2)));
        put_at(&mut map, 104, 100, an_archer());
        map.place_item(
            &Position::new(102, 100, 7),
            None,
            None,
            Item::new(
                Arc::new(ItemConfig::new(
                    ItemId(3),
                    "wall".to_string(),
                    None,
                    None,
                    std::collections::HashSet::from([ItemFlag::Unsight]),
                    Vec::new(),
                )),
                1,
            ),
        )
        .unwrap();

        assert!(matches!(
            chained(&map, caster, 5, TargetFilter::RangedCreatures),
            Err(SpellCastingDenyReason::InvalidTarget)
        ));
        assert_eq!(
            chained(&map, caster, 5, TargetFilter::Creatures).unwrap(),
            vec![rat]
        );
    }
}

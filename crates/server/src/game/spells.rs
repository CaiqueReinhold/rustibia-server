use smallvec::SmallVec;
use thiserror::Error;

use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::{Agent, AgentKey},
        combat::{AttackCost, AttackPlan, CombatDamage},
        effects::{AreaEffect, Missile},
        map::GameMap,
        player::Player,
        position::{Position, Rect},
        skills::SkillType,
        spells::{
            AreaOrigin, CastTarget, ChainAttack, ChainSorting, PowerCurve, Spell, SpellAttack,
            SpellEffect, SpellGroup, SpellHealing, SpellId, SpellTargetMode,
        },
    },
    game::{
        Tick, TickCtx,
        combat::{execute_attack, plan_spell_attack},
        events::BroadcastMessage,
        healing::{execute_healing, plan_healing_spell},
        map_query::{can_target, can_throw},
        random::Rolls,
        skills::tick_skill,
    },
    persistence::spells::SPELLS,
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
}

pub fn cast_spell(ctx: &mut TickCtx, agent_key: AgentKey, spell_id: SpellId, target: CastTarget) {
    let Some(position) = ctx.map.agent_position(agent_key).cloned() else {
        return;
    };
    let Some(player) = ctx.map.get_player(agent_key) else {
        return;
    };
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        return;
    };

    let Some(spell) = SPELLS.get(&spell_id) else {
        ctx.events.push(BroadcastMessage::SpellDenied {
            agent_key,
            position,
            reason: SpellCastingDenyReason::IdNotFound,
        });
        return;
    };

    if !has_spell_requirements(spell, player) {
        ctx.events.push(BroadcastMessage::SpellDenied {
            agent_key,
            position,
            reason: SpellCastingDenyReason::RequirementFailed,
        });
        return;
    }

    if !agent.mana().can_afford(spell.mana) {
        ctx.events.push(BroadcastMessage::SpellDenied {
            agent_key,
            position,
            reason: SpellCastingDenyReason::NoMana,
        });
        return;
    }

    if !can_cast_spell(agent, spell, ctx.tick) {
        ctx.events.push(BroadcastMessage::SpellDenied {
            agent_key,
            position,
            reason: SpellCastingDenyReason::StillInCooldown,
        });
        return;
    }

    let mark = ctx.mark();
    if let Err(reason) = execute_effect(ctx, agent_key, spell, target) {
        ctx.rollback_to(mark);
        ctx.events.push(BroadcastMessage::SpellDenied {
            agent_key,
            position,
            reason,
        });
    } else {
        ctx.events.push(BroadcastMessage::SpellCast {
            agent_key,
            position,
            spell_id,
        });
    }
}

pub struct SpellTargets {
    /// Includes the caster when an area covers it; a planner that must not hit its own
    /// caster filters this itself.
    pub keys: Vec<AgentKey>,
    /// The single target's tile, or an area's centre. `None` for a cast on the caster.
    pub aim: Option<Position>,
    pub delta: Option<Vec<(i8, i8)>>,
}

pub fn resolve_spell_targets(
    map: &GameMap,
    caster: AgentKey,
    mode: &SpellTargetMode,
    cast_target: &CastTarget,
) -> Result<SpellTargets, SpellCastingDenyReason> {
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
        SpellTargetMode::Caster => Ok(SpellTargets {
            keys: Vec::from([caster]),
            aim: None,
            delta: None,
        }),
        SpellTargetMode::Target { range } => {
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

            Ok(SpellTargets {
                keys: Vec::from([target]),
                aim: Some(target_pos.clone()),
                delta: Some(vec![(0, 0)]),
            })
        }
        SpellTargetMode::Area { origin, shape } => {
            let origin = resolve_area_origin(map, origin, position, cast_target)
                .ok_or(SpellCastingDenyReason::InvalidTarget)?;
            let (keys, delta) = resolve_area(map, origin, shape.get_delta_facing(agent.facing()));
            Ok(SpellTargets {
                keys,
                aim: Some(origin.clone()),
                delta: Some(delta),
            })
        }
    }
}

/// The curve's centre scaled by the caster's level and magic level, rolled across its spread.
pub fn roll_power(player: &Player, curve: &PowerCurve, roll: &mut Rolls) -> u32 {
    let center = curve.base_power
        * (1.0
            + (f32::from(player.level()) * curve.level_factor / 100.0)
            + (f32::from(player.skill(SkillType::Magic)) * curve.magic_factor / 100.0));
    let min = (center * (1.0 - curve.spread)).max(0.0).round() as u32;
    let max = (center * (1.0 + curve.spread)).round() as u32;
    roll.damage_roll(min, max)
}

fn resolve_area_origin<'a>(
    map: &'a GameMap,
    origin: &'a AreaOrigin,
    caster_pos: &'a Position,
    cast_target: &'a CastTarget,
) -> Option<&'a Position> {
    match origin {
        AreaOrigin::Caster => Some(caster_pos),
        AreaOrigin::Target => {
            let candidate = match cast_target {
                CastTarget::Agent(key) => map.agent_position(*key),
                CastTarget::Position(pos) => Some(pos),
                CastTarget::None => None,
            };
            candidate
                .filter(|pos| can_target(caster_pos, pos) && can_throw(map, caster_pos, pos, true))
        }
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
    let Some(agent) = ctx.map.get_agent_mut(agent_key) else {
        return;
    };
    agent.remove_mana(mana_cost);
    ctx.events
        .push(BroadcastMessage::PlayerManaUpdated { agent_key });
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
    spell.level <= player.level() && spell.vocations.contains(&player.vocation())
}

fn can_cast_spell(agent: &Agent, spell: &Spell, current_tick: Tick) -> bool {
    agent.next_spell_group_tick(spell.group) <= current_tick
        && agent.next_spell_tick(spell.id) <= current_tick
}

fn execute_effect(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    spell: &Spell,
    target: CastTarget,
) -> Result<(), SpellCastingDenyReason> {
    match &spell.effect {
        SpellEffect::Attack(attack) => attack_spell(ctx, agent_key, &target, attack),
        SpellEffect::Healing(healing) => healing_spell(ctx, agent_key, &target, healing),
    }?;

    let agent = ctx
        .map
        .get_agent_mut(agent_key)
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
    target: &CastTarget,
    spell_attack: &SpellAttack,
) -> Result<(), SpellCastingDenyReason> {
    let plan = plan_spell_attack(ctx.map, agent_key, ctx.roll, spell_attack, target)?;
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
    target: &CastTarget,
    spell_healing: &SpellHealing,
) -> Result<(), SpellCastingDenyReason> {
    let plan = plan_healing_spell(ctx.map, agent_key, ctx.roll, spell_healing, target)?;
    execute_healing(ctx, plan);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::entities::effects::AreaShape;
    use crate::entities::map::MapTile;
    use crate::persistence::test_fixtures::a_test_snapshot;

    fn a_caster_at(position: &Position) -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), position)
            .unwrap();
        (map, key)
    }

    fn aimed_area() -> SpellTargetMode {
        SpellTargetMode::Area {
            origin: AreaOrigin::Target,
            shape: Arc::new(AreaShape::new(vec![(0, 0)].into_boxed_slice())),
        }
    }

    #[test]
    fn an_aimed_tile_within_reach_centres_the_area() {
        let (map, caster) = a_caster_at(&Position::new(100, 100, 7));
        let aim = Position::new(103, 101, 7);

        let targets = resolve_spell_targets(
            &map,
            caster,
            &aimed_area(),
            &CastTarget::Position(aim.clone()),
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
                    resolve_spell_targets(
                        &map,
                        caster,
                        &aimed_area(),
                        &CastTarget::Position(aim.clone())
                    ),
                    Err(SpellCastingDenyReason::InvalidTarget)
                ),
                "{aim:?} must not be a legal aim"
            );
        }
    }
}

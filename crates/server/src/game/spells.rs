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
            ChainAttack, ChainSorting, PowerCurve, Spell, SpellAttack, SpellDelivery, SpellEffect,
            SpellGroup, SpellHealing,
        },
        support::SupportCast,
        targeting::{AreaOrigin, AreaTarget, TargetFilter, TargetMode},
    },
    game::{
        Tick, TickCtx,
        combat::{execute_attack, plan_spell_attack},
        events::BroadcastMessage,
        healing::{execute_healing, plan_healing_spell},
        map_query::{can_target, can_throw},
        random::Rolls,
        skills::tick_skill,
        support::cast_support,
    },
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
        TargetMode::Area { origin, shape } => {
            let origin = resolve_area_origin(map, origin, position, area_target)
                .ok_or(SpellCastingDenyReason::InvalidTarget)?;
            let (mut keys, delta) =
                resolve_area(map, origin, shape.get_delta_facing(agent.facing()));
            keys.retain(|key| passes(map, *key, filter));
            Ok(ResolvedTargets {
                keys,
                aim: Some(origin.clone()),
                delta: Some(delta),
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
        TargetFilter::Any => true,
        TargetFilter::Players => map.get_player(key).is_some(),
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
    execute_healing(ctx, plan);
    Ok(())
}

fn support_spell(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    target: &AreaTarget,
    param: Option<&str>,
    cast: &SupportCast,
) -> Result<(), SpellCastingDenyReason> {
    let filter = match cast.target {
        TargetMode::Aimed => TargetFilter::Any,
        _ => TargetFilter::Players,
    };
    let targets = resolve_targets(ctx.map, agent_key, &cast.target, target, param, filter)?;
    cast_support(ctx, agent_key, cast, targets);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::entities::world_map::WorldMap;
    use std::sync::Arc;

    use super::*;
    use crate::entities::effects::AreaShape;
    use crate::entities::map::MapTile;
    use crate::entities::skills::SkillValue;
    use crate::entities::vocation::Vocation;
    use crate::game::TestHarness;
    use crate::persistence::test_fixtures::{a_spell, a_test_creature, a_test_snapshot};

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
}

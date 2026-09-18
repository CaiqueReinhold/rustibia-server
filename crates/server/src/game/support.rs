use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::AgentKey,
        conditions::SpeedEffect,
        creature::CreatureFlag,
        effects::{AreaEffect, Missile},
        support::{SpeedFormula, SpeedTerm, SupportCast, SupportEffect},
    },
    game::{TickCtx, events::BroadcastMessage, random::Rolls, spells::ResolvedTargets},
};

pub fn cast_support(
    ctx: &mut TickCtx,
    caster: AgentKey,
    cast: &SupportCast,
    targets: ResolvedTargets,
) {
    if let (Some(missile_id), Some(to), Some(from)) = (
        cast.missile_id,
        targets.aim,
        ctx.map.agent_position(caster).cloned(),
    ) {
        ctx.events.push(BroadcastMessage::MissileLaunched {
            missile: Missile {
                missile_id,
                from,
                to,
            },
        });
    }

    for target in targets.keys {
        if let Some(effect_id) = cast.effect_id
            && let Some(position) = ctx.map.agent_position(target).cloned()
        {
            ctx.events.push(BroadcastMessage::AreaEffectAppeared {
                area_effect: AreaEffect::single(effect_id, position),
            });
        }
        apply_support(ctx, target, &cast.effect);
    }
}

pub fn apply_support(ctx: &mut TickCtx, target: AgentKey, effect: &SupportEffect) {
    match effect {
        SupportEffect::Speed {
            effect,
            formula,
            duration,
        } => {
            let Some(agent) = ctx.map.get_agent(target) else {
                return;
            };
            if *effect == SpeedEffect::Paralysis
                && agent
                    .get_creature_kind()
                    .is_some_and(|kind| kind.has_flag(CreatureFlag::ImmuneParalysis))
            {
                return;
            }
            let change = roll_speed_change(*effect, formula, agent.base_speed(), ctx.roll);
            let Some(position) = ctx.map.agent_position(target).cloned() else {
                return;
            };
            let Some(agent) = ctx.map.get_agent_mut(target) else {
                return;
            };
            let generation = agent.conditions_mut().apply_speed(*effect, change);

            ctx.scheduled.push(ScheduledCommand {
                at_tick: ctx.tick + *duration,
                command: WorldCommand::SpeedExpired {
                    agent_key: target,
                    generation,
                },
            });
            ctx.events.push(BroadcastMessage::AgentSpeedChanged {
                agent_key: target,
                position,
            });
        }
        SupportEffect::MagicShield { duration } => {
            let until = ctx.tick + *duration;
            if let Some(agent) = ctx.map.get_agent_mut(target) {
                agent.conditions_mut().set_magic_shield(until);
            }
        }
        SupportEffect::Cure { element } => {
            if let Some(agent) = ctx.map.get_agent_mut(target) {
                agent.conditions_mut().cure(*element);
            }
        }
    }
}

pub fn expire_speed(ctx: &mut TickCtx, agent_key: AgentKey, generation: u32) {
    let Some(agent) = ctx.map.get_agent_mut(agent_key) else {
        return;
    };
    if !agent.conditions_mut().expire_speed(generation) {
        return;
    }
    if let Some(position) = ctx.map.agent_position(agent_key).cloned() {
        ctx.events.push(BroadcastMessage::AgentSpeedChanged {
            agent_key,
            position,
        });
    }
}

fn roll_speed_change(
    effect: SpeedEffect,
    formula: &SpeedFormula,
    base: u16,
    roll: &mut Rolls,
) -> i16 {
    let at =
        |term: &SpeedTerm| (term.factor * f32::from(base)).round() as i32 + i32::from(term.flat);
    let (a, b) = (at(&formula.min), at(&formula.max));
    let (low, high) = (a.min(b), a.max(b));
    let rolled = low + roll.uniform(0, (high - low) as u32) as i32;
    let bounded = match effect {
        SpeedEffect::Haste => rolled.max(0),
        SpeedEffect::Paralysis => rolled.min(0),
    };
    bounded.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::entities::{
        agent::Agent,
        combat::CombatElement,
        conditions::DamageOverTime,
        creature::CreatureKind,
        map::{GameMap, MapTile},
        position::Position,
        spells::{Spell, SpellDelivery, SpellEffect, SpellGroup},
        targeting::{AreaTarget, TargetMode},
    };
    use crate::game::{
        TestHarness, Tick, TickDelta,
        config::GAME_CONFIG,
        spells::{CastSource, cast_spell},
    };
    use crate::persistence::test_fixtures::{a_creature_kind, a_spell, a_test_snapshot};

    fn at(x: u16) -> Position {
        Position::new(x, 10, 7)
    }

    fn a_player() -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        map.insert_tile(at(10), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(10))
            .unwrap();
        (map, key)
    }

    fn flat(effect: SpeedEffect, change: i16, duration: u64) -> SupportEffect {
        let term = SpeedTerm {
            factor: 0.0,
            flat: change,
        };
        SupportEffect::Speed {
            effect,
            formula: SpeedFormula {
                min: term.clone(),
                max: term,
            },
            duration: TickDelta(duration),
        }
    }

    fn scarab_paralysis() -> SpeedFormula {
        SpeedFormula {
            min: SpeedTerm {
                factor: -1.0,
                flat: 44,
            },
            max: SpeedTerm {
                factor: -1.0,
                flat: 149,
            },
        }
    }

    fn a_support_spell(group: SpellGroup, target: TargetMode, effect: SupportEffect) -> Spell {
        Spell {
            group,
            effect: SpellEffect::Support(SupportCast {
                target,
                effect,
                effect_id: None,
                missile_id: None,
            }),
            ..a_spell(1, 0, Vec::new())
        }
    }

    fn booked_generation(h: &TestHarness) -> u32 {
        h.scheduled
            .iter()
            .rev()
            .find_map(|s| match s.command {
                WorldCommand::SpeedExpired { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("no expiry was booked")
    }

    #[test]
    fn a_haste_speeds_up_its_caster_until_its_expiry_is_due() {
        let (mut map, player) = a_player();
        let spell = a_support_spell(
            SpellGroup::Support,
            TargetMode::Caster,
            flat(SpeedEffect::Haste, 30, 660),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        cast_spell(
            &mut h.ctx(&mut map),
            player,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        )
        .unwrap();

        let agent = map.get_agent(player).unwrap();
        assert_eq!(agent.speed(), 150);
        assert_eq!(
            agent.next_spell_group_tick(SpellGroup::Support),
            Tick(100) + GAME_CONFIG.combat.support_group_cooldown
        );
        assert!(h.events.iter().any(|e| matches!(
            e,
            BroadcastMessage::AgentSpeedChanged { agent_key, .. } if *agent_key == player
        )));
        assert!(h.scheduled.iter().any(|s| s.at_tick == Tick(760)
            && matches!(s.command, WorldCommand::SpeedExpired { agent_key, .. } if agent_key == player)));
    }

    #[test]
    fn the_expiry_restores_speed_and_a_stale_one_does_nothing() {
        let (mut map, player) = a_player();
        let mut h = TestHarness::seeded(1);

        apply_support(
            &mut h.ctx(&mut map),
            player,
            &flat(SpeedEffect::Haste, 30, 100),
        );
        let stale = booked_generation(&h);
        apply_support(
            &mut h.ctx(&mut map),
            player,
            &flat(SpeedEffect::Haste, 50, 100),
        );
        let live = booked_generation(&h);
        h.events.clear();

        expire_speed(&mut h.ctx(&mut map), player, stale);
        assert_eq!(map.get_agent(player).unwrap().speed(), 170);
        assert!(h.events.is_empty());

        expire_speed(&mut h.ctx(&mut map), player, live);
        assert_eq!(map.get_agent(player).unwrap().speed(), 120);
        assert!(matches!(
            h.events.as_slice(),
            [BroadcastMessage::AgentSpeedChanged { .. }]
        ));
    }

    #[test]
    fn a_haste_cast_while_paralysed_replaces_the_paralysis() {
        let (mut map, player) = a_player();
        let mut h = TestHarness::seeded(1);

        apply_support(
            &mut h.ctx(&mut map),
            player,
            &flat(SpeedEffect::Paralysis, -60, 100),
        );
        apply_support(
            &mut h.ctx(&mut map),
            player,
            &flat(SpeedEffect::Haste, 30, 100),
        );

        assert_eq!(map.get_agent(player).unwrap().speed(), 150);
    }

    #[test]
    fn a_paralysis_never_raises_speed_and_a_haste_never_lowers_it() {
        let mut roll = Rolls::new(1);
        for _ in 0..200 {
            let change =
                roll_speed_change(SpeedEffect::Paralysis, &scarab_paralysis(), 20, &mut roll);
            assert!(change <= 0, "{change}");
        }

        let weak = SpeedFormula {
            min: SpeedTerm {
                factor: 0.3,
                flat: -12,
            },
            max: SpeedTerm {
                factor: 0.3,
                flat: -12,
            },
        };
        assert_eq!(
            roll_speed_change(SpeedEffect::Haste, &weak, 10, &mut roll),
            0
        );
    }

    /// Measured in the Tibia client: 44–149 at base 258 and at base 546.
    #[test]
    fn the_scarab_paralysis_lands_between_44_and_149_whatever_the_base() {
        let mut roll = Rolls::new(7);
        for base in [258u16, 546] {
            let landed: Vec<i32> = (0..500)
                .map(|_| {
                    i32::from(base)
                        + i32::from(roll_speed_change(
                            SpeedEffect::Paralysis,
                            &scarab_paralysis(),
                            base,
                            &mut roll,
                        ))
                })
                .collect();
            assert!(
                landed.iter().all(|speed| (44..=149).contains(speed)),
                "base {base}: {landed:?}"
            );
        }
    }

    #[test]
    fn a_paralysis_immune_creature_keeps_its_speed_and_a_haste_still_lands() {
        let mut map = GameMap::new();
        map.insert_tile(at(10), MapTile::new());
        let kind = CreatureKind {
            flags: vec![CreatureFlag::ImmuneParalysis],
            ..a_creature_kind("Scarab")
        };
        let scarab = map
            .insert_agent(Agent::from_creature_kind(Arc::new(kind), at(10)), &at(10))
            .unwrap();
        let mut h = TestHarness::seeded(1);

        apply_support(
            &mut h.ctx(&mut map),
            scarab,
            &flat(SpeedEffect::Paralysis, -50, 100),
        );
        assert_eq!(map.get_agent(scarab).unwrap().speed(), 100);
        assert!(h.scheduled.is_empty());

        apply_support(
            &mut h.ctx(&mut map),
            scarab,
            &flat(SpeedEffect::Haste, 20, 100),
        );
        assert_eq!(map.get_agent(scarab).unwrap().speed(), 120);
    }

    #[test]
    fn a_cure_spell_removes_its_element_and_leaves_the_others() {
        let (mut map, player) = a_player();
        let conditions = map.get_agent_mut(player).unwrap().conditions_mut();
        conditions.apply_damage_over_time(DamageOverTime::decaying(
            CombatElement::Earth,
            None,
            TickDelta(80),
            100,
            None,
        ));
        conditions.apply_damage_over_time(DamageOverTime::decaying(
            CombatElement::Fire,
            None,
            TickDelta(180),
            100,
            None,
        ));
        let spell = a_support_spell(
            SpellGroup::Healing,
            TargetMode::Caster,
            SupportEffect::Cure {
                element: CombatElement::Earth,
            },
        );
        let mut h = TestHarness::seeded(1);

        cast_spell(
            &mut h.ctx(&mut map),
            player,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        )
        .unwrap();

        let conditions = map.get_agent_mut(player).unwrap().conditions_mut();
        assert!(!conditions.cure(CombatElement::Earth));
        assert!(conditions.cure(CombatElement::Fire));
    }

    #[test]
    fn a_magic_shield_spell_shields_its_caster_for_its_duration() {
        let (mut map, player) = a_player();
        let spell = a_support_spell(
            SpellGroup::Support,
            TargetMode::Caster,
            SupportEffect::MagicShield {
                duration: TickDelta(4000),
            },
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        cast_spell(
            &mut h.ctx(&mut map),
            player,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        )
        .unwrap();

        let conditions = map.get_agent(player).unwrap().conditions();
        assert!(conditions.is_magic_shielded(Tick(4099)));
        assert!(!conditions.is_magic_shielded(Tick(4100)));
    }

    #[test]
    fn a_paralyse_rune_slows_the_creature_it_is_aimed_at() {
        let (mut map, player) = a_player();
        map.insert_tile(at(11), MapTile::new());
        let rat = map
            .insert_agent(
                Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), at(11)),
                &at(11),
            )
            .unwrap();
        let mut spell = a_support_spell(
            SpellGroup::Attack,
            TargetMode::Aimed,
            flat(SpeedEffect::Paralysis, -50, 400),
        );
        spell.delivery = SpellDelivery::Rune;
        let mut h = TestHarness::seeded(1);

        cast_spell(
            &mut h.ctx(&mut map),
            player,
            &spell,
            AreaTarget::Agent(rat),
            None,
            CastSource::Rune,
        )
        .unwrap();

        assert_eq!(map.get_agent(rat).unwrap().speed(), 50);
    }
}

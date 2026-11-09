use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::{Agent, AgentKey, Challenge},
        conditions::{SpeedEffect, TimedCondition},
        creature::CreatureFlag,
        effects::{AreaEffect, EffectId, Missile},
        support::{SpeedFormula, SpeedTerm, SupportCast, SupportEffect},
        targeting::TargetMode,
    },
    game::{
        TickCtx, conditions, events::BroadcastMessage, pathfinding, random::Rolls,
        spells::ResolvedTargets, targeting,
    },
};

pub fn cast_support(
    ctx: &mut TickCtx,
    caster: AgentKey,
    cast: &SupportCast,
    targets: ResolvedTargets,
) {
    if let (Some(missile_id), Some(to), Some(from)) = (
        cast.missile_id,
        targets.aim.clone(),
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

    if let Some(effect_id) = cast.effect_id {
        match (&cast.target, &targets.aim, &targets.delta) {
            (TargetMode::Chain { .. }, _, _) => draw_chain(ctx, caster, effect_id, &targets.keys),
            (TargetMode::Area { .. }, Some(origin), Some(delta)) => {
                ctx.events.push(BroadcastMessage::AreaEffectAppeared {
                    area_effect: AreaEffect {
                        effect_id,
                        origin: origin.clone(),
                        delta: delta.clone(),
                    },
                });
            }
            _ => {
                for target in &targets.keys {
                    if let Some(position) = ctx.map.agent_position(*target).cloned() {
                        ctx.events.push(BroadcastMessage::AreaEffectAppeared {
                            area_effect: AreaEffect::single(effect_id, position),
                        });
                    }
                }
            }
        }
    }

    for target in targets.keys {
        apply_support(ctx, caster, target, &cast.effect);
    }
}

/// Canary's `maxSearchDist` for a chain effect's path.
const CHAIN_PATH_SEARCH: u16 = 9;

fn draw_chain(ctx: &mut TickCtx, caster: AgentKey, effect_id: EffectId, keys: &[AgentKey]) {
    let Some(mut from) = ctx.map.agent_position(caster).cloned() else {
        return;
    };
    for key in keys {
        let Some(to) = ctx.map.agent_position(*key).cloned() else {
            continue;
        };
        let delta = pathfinding::path_beside(ctx.map, &from, &to, CHAIN_PATH_SEARCH)
            .iter()
            .chain(std::iter::once(&to))
            .map(|tile| {
                (
                    (i32::from(tile.x) - i32::from(from.x)) as i8,
                    (i32::from(tile.y) - i32::from(from.y)) as i8,
                )
            })
            .collect();
        ctx.events.push(BroadcastMessage::AreaEffectAppeared {
            area_effect: AreaEffect {
                effect_id,
                origin: from.clone(),
                delta,
            },
        });
        from = to;
    }
}

pub fn apply_support(
    ctx: &mut TickCtx,
    caster: AgentKey,
    target: AgentKey,
    effect: &SupportEffect,
) {
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
            let Some(mut agent) = ctx.map.agent_mut(target) else {
                return;
            };
            let generation = agent.conditions(|c| c.apply_speed(*effect, change));

            ctx.scheduled.push(ScheduledCommand {
                at_tick: ctx.tick + *duration,
                command: WorldCommand::SpeedExpired {
                    agent_key: target,
                    generation,
                },
            });
        }
        SupportEffect::MagicShield { duration } => {
            let until = ctx.tick + *duration;
            let created = ctx
                .map
                .agent_mut(target)
                .is_some_and(|mut agent| agent.conditions(|c| c.extend_magic_shield(until)));
            if created {
                conditions::schedule_expiry(ctx, target, TimedCondition::MagicShield);
            }
        }
        SupportEffect::CancelMagicShield => {
            if let Some(mut agent) = ctx.map.agent_mut(target) {
                agent.conditions(|c| c.remove_magic_shield());
            }
        }
        SupportEffect::Challenge { duration, melee } => {
            if !ctx.map.get_agent(target).is_some_and(Agent::is_creature) {
                return;
            }
            targeting::set_target(ctx, target, Some(caster), 0);
            let tick = ctx.tick;
            if let Some(mut agent) = ctx.map.agent_mut(target) {
                agent.set_challenge(Some(Challenge {
                    by: caster,
                    since: tick,
                    until: tick + *duration,
                }));
                if let Some(melee) = melee {
                    agent.force_melee(tick + *melee);
                }
            }
        }
        SupportEffect::Cure { element } => {
            if let Some(mut agent) = ctx.map.agent_mut(target) {
                agent.conditions(|c| c.cure(*element));
            }
        }
    }
}

pub fn expire_speed(ctx: &mut TickCtx, agent_key: AgentKey, generation: u32) {
    if let Some(mut agent) = ctx.map.agent_mut(agent_key) {
        agent.conditions(|c| c.expire_speed(generation));
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
    use crate::entities::world_map::WorldMap;
    use std::sync::Arc;

    use super::*;
    use crate::entities::agent::Challenge;
    use crate::entities::effects::{AreaShape, EffectId};
    use crate::entities::items::{Item, ItemConfig, ItemFlag, ItemId};
    use crate::entities::targeting::AreaOrigin;
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
    use crate::game::spells::SpellCastingDenyReason;
    use crate::game::{
        TestHarness, Tick, TickDelta,
        config::GAME_CONFIG,
        spells::{CastSource, cast_spell},
    };
    use crate::persistence::test_fixtures::{a_creature_kind, a_spell, a_test_snapshot};
    use std::collections::HashSet;

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
        let (map, player) = a_player();
        let spell = a_support_spell(
            SpellGroup::Support,
            TargetMode::Caster,
            flat(SpeedEffect::Haste, 30, 660),
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        let mut map = WorldMap::new(map);
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
        assert!(map.delta().agent(player).speed());
        assert!(h.scheduled.iter().any(|s| s.at_tick == Tick(760)
            && matches!(s.command, WorldCommand::SpeedExpired { agent_key, .. } if agent_key == player)));
    }

    #[test]
    fn the_expiry_restores_speed_and_a_stale_one_does_nothing() {
        let (map, player) = a_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_support(
            &mut h.ctx(&mut map),
            player,
            player,
            &flat(SpeedEffect::Haste, 30, 100),
        );
        let stale = booked_generation(&h);
        apply_support(
            &mut h.ctx(&mut map),
            player,
            player,
            &flat(SpeedEffect::Haste, 50, 100),
        );
        let live = booked_generation(&h);
        map.take_delta();

        expire_speed(&mut h.ctx(&mut map), player, stale);
        assert_eq!(map.get_agent(player).unwrap().speed(), 170);
        assert!(map.delta().is_empty());

        expire_speed(&mut h.ctx(&mut map), player, live);
        assert_eq!(map.get_agent(player).unwrap().speed(), 120);
        assert!(map.delta().agent(player).speed());
    }

    #[test]
    fn a_haste_cast_while_paralysed_replaces_the_paralysis() {
        let (map, player) = a_player();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        apply_support(
            &mut h.ctx(&mut map),
            player,
            player,
            &flat(SpeedEffect::Paralysis, -60, 100),
        );
        apply_support(
            &mut h.ctx(&mut map),
            player,
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

        let mut map = WorldMap::new(map);
        apply_support(
            &mut h.ctx(&mut map),
            scarab,
            scarab,
            &flat(SpeedEffect::Paralysis, -50, 100),
        );
        assert_eq!(map.get_agent(scarab).unwrap().speed(), 100);
        assert!(h.scheduled.is_empty());

        apply_support(
            &mut h.ctx(&mut map),
            scarab,
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

        let mut map = WorldMap::new(map);
        cast_spell(
            &mut h.ctx(&mut map),
            player,
            &spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        )
        .unwrap();

        let conditions = map
            .inner_mut()
            .get_agent_mut(player)
            .unwrap()
            .conditions_mut();
        assert!(!conditions.cure(CombatElement::Earth));
        assert!(conditions.cure(CombatElement::Fire));
    }

    #[test]
    fn a_magic_shield_spell_shields_its_caster_for_its_duration() {
        let (map, player) = a_player();
        let spell = a_support_spell(
            SpellGroup::Support,
            TargetMode::Caster,
            SupportEffect::MagicShield {
                duration: TickDelta(4000),
            },
        );
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);

        let mut map = WorldMap::new(map);
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
    fn cancelling_the_magic_shield_removes_it_at_once() {
        let (map, player) = a_player();
        let mut map = WorldMap::new(map);
        map.agent_mut(player)
            .unwrap()
            .conditions(|c| c.extend_magic_shield(Tick(4000)));
        map.take_delta();
        let spell = a_support_spell(
            SpellGroup::Support,
            TargetMode::Caster,
            SupportEffect::CancelMagicShield,
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

        assert!(
            !map.get_agent(player)
                .unwrap()
                .conditions()
                .is_magic_shielded(Tick(0))
        );
        assert!(map.delta().agent(player).status());
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

        let mut map = WorldMap::new(map);
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

    fn ground() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "ground".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Ground]),
                Vec::new(),
            )),
            1,
        )
    }

    /// Rows 9 to 11 from x = 5 to 20; walkable only when `grounded`.
    fn a_room(grounded: bool) -> GameMap {
        let mut map = GameMap::new();
        for x in 5..=20 {
            for y in 9..=11 {
                let mut tile = MapTile::new();
                if grounded {
                    tile.push_item(ground());
                }
                map.insert_tile(Position::new(x, y, 7), tile);
            }
        }
        map
    }

    fn put(map: &mut GameMap, x: u16, agent: Agent) -> AgentKey {
        map.insert_agent(agent, &at(x)).unwrap()
    }

    fn a_knight(map: &mut GameMap) -> AgentKey {
        put(map, 10, Agent::from_player(a_test_snapshot(1, 1)))
    }

    fn an_archer(x: u16) -> Agent {
        Agent::from_creature_kind(
            Arc::new(CreatureKind {
                target_distance: 4,
                ..a_creature_kind("Archer")
            }),
            at(x),
        )
    }

    fn a_boxed_area() -> TargetMode {
        TargetMode::Area {
            origin: AreaOrigin::Caster,
            shape: Arc::new(AreaShape::new(
                vec![
                    (-1, -1),
                    (0, -1),
                    (1, -1),
                    (-1, 0),
                    (0, 0),
                    (1, 0),
                    (-1, 1),
                    (0, 1),
                    (1, 1),
                ]
                .into_boxed_slice(),
            )),
        }
    }

    fn a_chain() -> TargetMode {
        TargetMode::Chain {
            count: 5,
            hop_range: 6,
        }
    }

    fn a_challenge(target: TargetMode, melee: Option<u64>) -> Spell {
        let mut spell = a_support_spell(
            SpellGroup::Support,
            target,
            SupportEffect::Challenge {
                duration: TickDelta(120),
                melee: melee.map(TickDelta),
            },
        );
        if let SpellEffect::Support(cast) = &mut spell.effect {
            cast.effect_id = Some(EffectId(13));
        }
        spell
    }

    fn cast_at(
        map: &mut WorldMap,
        h: &mut TestHarness,
        caster: AgentKey,
        spell: &Spell,
    ) -> Result<(), SpellCastingDenyReason> {
        cast_spell(
            &mut h.ctx(map),
            caster,
            spell,
            AreaTarget::None,
            None,
            CastSource::Words,
        )
    }

    fn effects(h: &TestHarness) -> Vec<&AreaEffect> {
        h.events
            .iter()
            .filter_map(|event| match event {
                BroadcastMessage::AreaEffectAppeared { area_effect } => Some(area_effect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_challenge_turns_the_creatures_around_its_caster_and_no_player() {
        let mut map = a_room(false);
        let knight = a_knight(&mut map);
        let bystander = put(&mut map, 9, Agent::from_player(a_test_snapshot(2, 1)));
        let rat = put(
            &mut map,
            11,
            Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), at(11)),
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness {
            tick: Tick(30),
            ..TestHarness::seeded(1)
        };

        cast_at(&mut map, &mut h, knight, &a_challenge(a_boxed_area(), None)).unwrap();

        let rat = map.get_agent(rat).unwrap();
        assert_eq!(rat.target(), Some(knight));
        assert_eq!(
            rat.challenge(Tick(30)),
            Some(Challenge {
                by: knight,
                since: Tick(30),
                until: Tick(150),
            })
        );
        assert_eq!(map.get_agent(bystander).unwrap().target(), None);
    }

    #[test]
    fn an_area_support_cast_shows_its_effect_over_the_whole_area() {
        let mut map = a_room(false);
        let knight = a_knight(&mut map);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_at(&mut map, &mut h, knight, &a_challenge(a_boxed_area(), None)).unwrap();

        let shown = effects(&h);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].origin, at(10));
        assert_eq!(shown[0].delta.len(), 9);
    }

    #[test]
    fn chivalrous_challenge_forces_the_ranged_creature_it_reaches_into_melee() {
        let mut map = a_room(false);
        let knight = a_knight(&mut map);
        let archer = put(&mut map, 13, an_archer(13));
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_at(&mut map, &mut h, knight, &a_challenge(a_chain(), Some(240))).unwrap();

        let archer = map.get_agent(archer).unwrap();
        assert_eq!(archer.target(), Some(knight));
        assert_eq!(archer.target_distance(Tick(239)), Some(1));
    }

    #[test]
    fn a_chain_with_nothing_to_reach_refuses_the_cast() {
        let mut map = a_room(false);
        let knight = a_knight(&mut map);
        let rat = put(
            &mut map,
            11,
            Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), at(11)),
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        let result = cast_at(&mut map, &mut h, knight, &a_challenge(a_chain(), Some(240)));

        assert!(matches!(result, Err(SpellCastingDenyReason::InvalidTarget)));
        assert_eq!(map.get_agent(rat).unwrap().target(), None);
    }

    #[test]
    fn a_chain_is_drawn_along_the_walk_to_each_pick() {
        let mut map = a_room(true);
        let knight = a_knight(&mut map);
        put(&mut map, 14, an_archer(14));
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_at(&mut map, &mut h, knight, &a_challenge(a_chain(), Some(240))).unwrap();

        let shown = effects(&h);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].origin, at(10));
        assert_eq!(shown[0].delta, vec![(1, 0), (2, 0), (3, 0), (4, 0)]);
    }

    #[test]
    fn a_chain_with_no_walk_is_drawn_on_the_pick_alone() {
        let mut map = a_room(false);
        let knight = a_knight(&mut map);
        put(&mut map, 13, an_archer(13));
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        cast_at(&mut map, &mut h, knight, &a_challenge(a_chain(), Some(240))).unwrap();

        let shown = effects(&h);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].delta, vec![(3, 0)]);
    }
}

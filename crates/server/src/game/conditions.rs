use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::AgentKey,
        combat::{CombatDamage, CombatElement},
        conditions::{ConditionSpec, DamageOverTime, SpecSchedule, TimedCondition},
    },
    game::{TickCtx, config::GAME_CONFIG, damage::apply_damage},
};

pub fn regenerate_life_mana(ctx: &mut TickCtx, agent_key: AgentKey, generation: u32) {
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        return;
    };
    let Some((until, current_generation)) = agent.conditions().fed() else {
        return;
    };
    if current_generation != generation {
        return;
    }
    if until <= ctx.tick {
        if let Some(mut agent) = ctx.map.agent_mut(agent_key) {
            agent.conditions(|c| c.remove_fed());
        }
        return;
    }

    let Some(vocation) = ctx
        .map
        .get_player(agent_key)
        .map(|player| player.vocation())
    else {
        return;
    };
    let mut agent = ctx.map.agent_mut(agent_key).unwrap();
    let life_missing = agent.life().missing() > 0;
    let mana_missing = agent.mana().missing() > 0;
    if life_missing {
        agent.restore_life(vocation.life_regen_amount());
    }
    if mana_missing {
        agent.restore_mana(vocation.mana_regen_amount());
    }

    ctx.scheduled.push(ScheduledCommand {
        at_tick: ctx.tick + GAME_CONFIG.regen_ticks,
        command: WorldCommand::RegeneratePlayer {
            agent_key,
            generation,
        },
    });
}

pub fn schedule_expiry(ctx: &mut TickCtx, agent_key: AgentKey, kind: TimedCondition) {
    let Some(until) = ctx
        .map
        .get_agent(agent_key)
        .and_then(|agent| agent.conditions().until(kind))
    else {
        return;
    };
    ctx.scheduled.push(ScheduledCommand {
        at_tick: until,
        command: WorldCommand::ConditionExpired { agent_key, kind },
    });
}

pub fn expire_condition(ctx: &mut TickCtx, agent_key: AgentKey, kind: TimedCondition) {
    let Some(until) = ctx
        .map
        .get_agent(agent_key)
        .and_then(|agent| agent.conditions().until(kind))
    else {
        return;
    };
    if until > ctx.tick {
        return schedule_expiry(ctx, agent_key, kind);
    }
    if let Some(mut agent) = ctx.map.agent_mut(agent_key) {
        agent.conditions(|c| c.remove_timed(kind));
    }
}

pub fn apply_condition(
    ctx: &mut TickCtx,
    target: AgentKey,
    spec: &ConditionSpec,
    source: Option<AgentKey>,
) {
    let rolled = ctx.roll.damage_roll(spec.damage.min, spec.damage.max);
    if rolled == 0 {
        return;
    }
    let dot = match spec.schedule {
        SpecSchedule::Decaying { start } => {
            DamageOverTime::decaying(spec.element, source, spec.interval, rolled, start)
        }
        SpecSchedule::Flat { count } => {
            DamageOverTime::flat(spec.element, source, spec.interval, rolled, count)
        }
    };

    let Some(mut agent) = ctx.map.agent_mut(target) else {
        return;
    };
    let Some(generation) = agent.conditions(|c| c.apply_damage_over_time(dot)) else {
        return;
    };
    if spec.delayed {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + spec.interval,
            command: WorldCommand::DamageOverTimeTick {
                agent_key: target,
                element: spec.element,
                generation,
            },
        });
    } else {
        tick_damage_over_time(ctx, target, spec.element, generation);
    }
}

pub fn tick_damage_over_time(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    element: CombatElement,
    generation: u32,
) {
    let Some(mut agent) = ctx.map.agent_mut(agent_key) else {
        return;
    };
    let Some(hit) = agent.conditions(|c| c.next_damage_hit(element, generation)) else {
        return;
    };
    let source = hit
        .source
        .filter(|source| ctx.map.get_agent(*source).is_some());

    apply_damage(
        ctx,
        agent_key,
        CombatDamage {
            element,
            value: hit.value,
            blocked_shield: false,
            blocked_armor: false,
        },
        source,
    );

    if !hit.exhausted && ctx.map.get_agent(agent_key).is_some() {
        ctx.scheduled.push(ScheduledCommand {
            at_tick: ctx.tick + hit.interval,
            command: WorldCommand::DamageOverTimeTick {
                agent_key,
                element,
                generation,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::Bounds;
    use crate::entities::conditions::TimedCondition;
    use crate::entities::world_map::WorldMap;
    use crate::entities::{
        agent::Agent,
        map::{GameMap, MapTile},
        position::Position,
    };
    use crate::game::{TestHarness, Tick, TickDelta};
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};

    fn a_fed_player(fed_for: u64) -> (GameMap, AgentKey, u32) {
        let position = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &position)
            .unwrap();
        let generation = map
            .get_agent_mut(key)
            .unwrap()
            .conditions_mut()
            .add_fed_ticks(Tick(0), TickDelta(fed_for));
        (map, key, generation)
    }

    #[test]
    fn a_full_pool_does_not_end_the_chain() {
        let (mut map, key, generation) = a_fed_player(1000);
        map.get_agent_mut(key).unwrap().remove_mana(50);

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(120);
        let mut map = WorldMap::new(map);
        regenerate_life_mana(&mut h.ctx(&mut map), key, generation);

        let mana = map.get_agent(key).unwrap().mana().current;
        assert_eq!(
            mana,
            50 + a_test_snapshot(1, 1).vocation.mana_regen_amount()
        );
        assert_eq!(h.scheduled.len(), 1);
        assert_eq!(h.scheduled[0].at_tick, Tick(120) + GAME_CONFIG.regen_ticks);
    }

    #[test]
    fn the_chain_ends_and_clears_fed_when_the_food_runs_out() {
        let (map, key, generation) = a_fed_player(100);

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);
        let mut map = WorldMap::new(map);
        regenerate_life_mana(&mut h.ctx(&mut map), key, generation);

        assert!(map.get_agent(key).unwrap().conditions().fed().is_none());
        assert!(h.scheduled.is_empty());
    }

    #[test]
    fn a_stale_generation_regenerates_nothing() {
        let (mut map, key, generation) = a_fed_player(1000);
        map.get_agent_mut(key).unwrap().remove_life(50);

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(120);
        let mut map = WorldMap::new(map);
        regenerate_life_mana(&mut h.ctx(&mut map), key, generation + 7);

        assert_eq!(map.get_agent(key).unwrap().life().current, 50);
        assert!(h.scheduled.is_empty());
    }

    #[test]
    fn a_damage_tick_hits_and_books_the_next_one() {
        let position = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &position)
            .unwrap();
        let generation = map
            .get_agent_mut(key)
            .unwrap()
            .conditions_mut()
            .apply_damage_over_time(DamageOverTime::decaying(
                CombatElement::Earth,
                None,
                TickDelta(80),
                100,
                None,
            ))
            .unwrap();

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(80);
        let mut map = WorldMap::new(map);
        tick_damage_over_time(&mut h.ctx(&mut map), key, CombatElement::Earth, generation);

        assert_eq!(map.get_agent(key).unwrap().life().current, 95);
        assert_eq!(h.scheduled.len(), 1);
        assert_eq!(h.scheduled[0].at_tick, Tick(160));
    }

    #[test]
    fn a_stale_damage_generation_does_nothing() {
        let position = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &position)
            .unwrap();
        let generation = map
            .get_agent_mut(key)
            .unwrap()
            .conditions_mut()
            .apply_damage_over_time(DamageOverTime::flat(
                CombatElement::Fire,
                None,
                TickDelta(200),
                10,
                7,
            ))
            .unwrap();

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(200);
        let mut map = WorldMap::new(map);
        tick_damage_over_time(
            &mut h.ctx(&mut map),
            key,
            CombatElement::Fire,
            generation + 1,
        );

        assert_eq!(map.get_agent(key).unwrap().life().current, 100);
        assert!(h.scheduled.is_empty());
    }

    fn a_blocked_player(at: Tick) -> (WorldMap, AgentKey) {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        map.get_agent_mut(key)
            .unwrap()
            .conditions_mut()
            .reset_logout_block(at);
        (WorldMap::new(map), key)
    }

    #[test]
    fn a_due_condition_is_removed_and_marks_the_status() {
        let (mut map, player) = a_blocked_player(Tick(0));
        let mut h = TestHarness::new();
        h.tick = Tick(0) + GAME_CONFIG.logout_block_ticks;

        expire_condition(&mut h.ctx(&mut map), player, TimedCondition::LogoutBlock);

        let conditions = map.get_agent(player).unwrap().conditions();
        assert_eq!(conditions.until(TimedCondition::LogoutBlock), None);
        assert!(map.delta().agent(player).status());
    }

    #[test]
    fn an_extended_condition_reschedules_instead_of_expiring() {
        let (mut map, player) = a_blocked_player(Tick(0));
        map.inner_mut()
            .get_agent_mut(player)
            .unwrap()
            .conditions_mut()
            .reset_logout_block(Tick(10));
        let mut h = TestHarness::new();
        h.tick = Tick(0) + GAME_CONFIG.logout_block_ticks;

        expire_condition(&mut h.ctx(&mut map), player, TimedCondition::LogoutBlock);

        assert!(map.delta().is_empty());
        assert!(matches!(
            h.scheduled.as_slice(),
            [ScheduledCommand {
                at_tick,
                command: WorldCommand::ConditionExpired {
                    kind: TimedCondition::LogoutBlock,
                    ..
                },
            }] if *at_tick == Tick(10) + GAME_CONFIG.logout_block_ticks
        ));
    }

    #[test]
    fn an_expiry_for_a_condition_already_gone_does_nothing() {
        let (mut map, player) = a_blocked_player(Tick(0));
        let mut h = TestHarness::new();

        expire_condition(&mut h.ctx(&mut map), player, TimedCondition::MagicShield);

        assert!(map.delta().is_empty());
        assert!(h.scheduled.is_empty());
    }

    fn a_rat_at(position: &Position) -> (WorldMap, AgentKey) {
        let mut map = GameMap::new();
        map.insert_tile(position.clone(), MapTile::new());
        let key = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), position)
            .unwrap();
        (WorldMap::new(map), key)
    }

    fn a_burn(damage: u32, delayed: bool) -> ConditionSpec {
        ConditionSpec {
            element: CombatElement::Fire,
            damage: Bounds {
                min: damage,
                max: damage,
            },
            interval: TickDelta(200),
            schedule: SpecSchedule::Flat { count: 7 },
            delayed,
        }
    }

    #[test]
    fn an_undelayed_condition_hits_at_once_and_books_the_next_hit() {
        let (mut map, rat) = a_rat_at(&Position::new(10, 10, 7));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(50);

        apply_condition(&mut h.ctx(&mut map), rat, &a_burn(20, false), None);

        assert_eq!(map.get_agent(rat).unwrap().life().current, 80);
        assert_eq!(h.scheduled.len(), 1);
        assert_eq!(h.scheduled[0].at_tick, Tick(250));
    }

    #[test]
    fn a_delayed_condition_waits_an_interval_for_its_first_hit() {
        let (mut map, rat) = a_rat_at(&Position::new(10, 10, 7));
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(50);

        apply_condition(&mut h.ctx(&mut map), rat, &a_burn(20, true), None);

        assert_eq!(map.get_agent(rat).unwrap().life().current, 100);
        assert_eq!(h.scheduled.len(), 1);
        assert_eq!(h.scheduled[0].at_tick, Tick(250));
    }

    #[test]
    fn a_condition_that_deals_nothing_is_not_applied() {
        let (mut map, rat) = a_rat_at(&Position::new(10, 10, 7));
        let status = map.get_agent(rat).unwrap().conditions().status();
        let mut h = TestHarness::seeded(1);

        apply_condition(&mut h.ctx(&mut map), rat, &a_burn(0, false), None);

        let rat = map.get_agent(rat).unwrap();
        assert_eq!(rat.life().current, 100);
        assert_eq!(rat.conditions().status(), status);
        assert!(h.scheduled.is_empty());
    }
}

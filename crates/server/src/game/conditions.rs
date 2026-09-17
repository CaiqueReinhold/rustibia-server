use crate::{
    actors::world::{ScheduledCommand, WorldCommand},
    entities::{
        agent::AgentKey,
        combat::{CombatDamage, CombatElement},
        conditions::{ConditionSpec, DamageOverTime, SpecSchedule},
    },
    game::{TickCtx, config::GAME_CONFIG, damage::apply_damage, events::BroadcastMessage},
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
        if let Some(agent) = ctx.map.get_agent_mut(agent_key) {
            agent.conditions_mut().remove_fed();
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
    let position = ctx.map.agent_position(agent_key).cloned();
    let agent = ctx.map.get_agent_mut(agent_key).unwrap();
    let life_missing = agent.life().missing() > 0;
    let mana_missing = agent.mana().missing() > 0;
    if life_missing {
        agent.restore_life(vocation.life_regen_amount());
    }
    if mana_missing {
        agent.restore_mana(vocation.mana_regen_amount());
    }

    if let Some(position) = position.filter(|_| life_missing) {
        ctx.events.push(BroadcastMessage::PlayerLifeUpdated {
            agent_key,
            position,
        });
    }
    if mana_missing {
        ctx.events
            .push(BroadcastMessage::PlayerManaUpdated { agent_key });
    }

    ctx.scheduled.push(ScheduledCommand {
        at_tick: ctx.tick + GAME_CONFIG.regen_ticks,
        command: WorldCommand::RegeneratePlayer {
            agent_key,
            generation,
        },
    });
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

    let Some(agent) = ctx.map.get_agent_mut(target) else {
        return;
    };
    let Some(generation) = agent.conditions_mut().apply_damage_over_time(dot) else {
        return;
    };
    ctx.scheduled.push(ScheduledCommand {
        at_tick: ctx.tick + spec.interval,
        command: WorldCommand::DamageOverTimeTick {
            agent_key: target,
            element: spec.element,
            generation,
        },
    });
}

pub fn tick_damage_over_time(
    ctx: &mut TickCtx,
    agent_key: AgentKey,
    element: CombatElement,
    generation: u32,
) {
    let Some(agent) = ctx.map.get_agent_mut(agent_key) else {
        return;
    };
    let Some(hit) = agent.conditions_mut().next_damage_hit(element, generation) else {
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
        let (mut map, key, generation) = a_fed_player(100);

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(100);
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
        tick_damage_over_time(
            &mut h.ctx(&mut map),
            key,
            CombatElement::Fire,
            generation + 1,
        );

        assert_eq!(map.get_agent(key).unwrap().life().current, 100);
        assert!(h.scheduled.is_empty());
    }
}

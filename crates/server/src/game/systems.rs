use crate::{
    entities::{agent::AgentKey, conditions::TimedCondition},
    game::{TickCtx, combat, conditions, targeting},
};

pub fn combat_system(ctx: &mut TickCtx) {
    let with_targets: Vec<(AgentKey, AgentKey)> = ctx
        .map
        .iter_agents()
        .filter(|(_, agent)| agent.target().is_some())
        .map(|(key, agent)| (key, agent.target().unwrap()))
        .collect();

    for (agent_key, target) in with_targets {
        if targeting::drop_unreachable_target(ctx, agent_key) {
            continue;
        }

        let tick = ctx.tick;
        let created = match ctx.map.agent_mut(target) {
            Some(mut agent) if !agent.is_creature() => {
                agent.conditions(|c| c.reset_logout_block(tick))
            }
            _ => false,
        };
        if created {
            conditions::schedule_expiry(ctx, target, TimedCondition::LogoutBlock);
        }

        drive_auto_attack(ctx, agent_key);
    }
}

fn drive_auto_attack(ctx: &mut TickCtx, agent_key: AgentKey) {
    let Some(plan) = combat::plan_auto_attack(ctx.map, agent_key, ctx.roll, ctx.tick) else {
        return;
    };
    if let Some(mut attacker) = ctx.map.agent_mut(plan.attacker) {
        attacker.stamp_auto_attack(ctx.tick);
    }
    combat::execute_attack(ctx, plan);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::world::WorldCommand;
    use crate::entities::conditions::TimedCondition;
    use crate::entities::world_map::WorldMap;
    use crate::entities::{
        agent::Agent,
        map::{GameMap, MapTile},
        position::Position,
    };
    use crate::game::{TestHarness, Tick, config::GAME_CONFIG};
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};

    /// The cadence belongs to the world, not to the executor a spell planner also feeds:
    /// `execute_attack` stamps nothing, so this is the only thing standing between a player
    /// and a swing every tick.
    #[test]
    fn a_swing_stamps_the_auto_attack_cooldown() {
        let (a, b) = (Position::new(10, 10, 7), Position::new(11, 10, 7));
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &a)
            .unwrap();
        let target = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &b)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(target), 0);

        let mut h = TestHarness::seeded(1);
        h.tick = Tick(7);
        let mut map = WorldMap::new(map);
        combat_system(&mut h.ctx(&mut map));

        assert_eq!(
            map.get_agent(attacker).unwrap().next_auto_attack_tick,
            Tick(7) + GAME_CONFIG.combat.auto_attack_ticks
        );
    }

    #[test]
    fn a_player_under_attack_keeps_one_logout_expiry_pending() {
        let (a, b) = (Position::new(10, 10, 7), Position::new(11, 10, 7));
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 100, (0, 0)), &a)
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &b)
            .unwrap();
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 0);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        for tick in [Tick(7), Tick(8), Tick(9)] {
            h.tick = tick;
            combat_system(&mut h.ctx(&mut map));
        }

        let expiries = h
            .scheduled
            .iter()
            .filter(|s| {
                matches!(
                    s.command,
                    WorldCommand::ConditionExpired {
                        kind: TimedCondition::LogoutBlock,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(expiries, 1);
    }
}

use tracing::{error, info};

use crate::actors::world::{ScheduledCommand, WorldCommand};
use crate::constants::items::MAX_STACK_AMOUNT;
use crate::entities::agent::{Agent, AgentKey};
use crate::entities::creature::CreatureKind;
use crate::entities::items::{Item, ItemFlag, ItemId};
use crate::entities::position::ItemPlacement;
use crate::game::TickCtx;
use crate::game::events::BroadcastMessage;
use crate::game::item_movement::insert_item_at;
use crate::game::random::Rolls;
use crate::game::{experience, targeting};
use crate::persistence::items::ITEM_CONFIGS;

pub fn reap(ctx: &mut TickCtx, agent_key: AgentKey, source: Option<AgentKey>) {
    let Some(dead) = ctx.map.get_agent(agent_key) else {
        return;
    };
    if !dead.is_creature() || dead.life().current > 0 {
        return;
    }

    let victim = dead.name();
    match source
        .and_then(|key| ctx.map.get_agent(key))
        .map(|a| a.name())
    {
        Some(killer) => info!("{killer} killed {victim}"),
        None => info!("{victim} died"),
    }

    experience::award(ctx, agent_key);

    let still_targeting: Vec<AgentKey> = ctx
        .map
        .iter_agents()
        .filter(|(_, other)| other.target() == Some(agent_key))
        .map(|(key, _)| key)
        .collect();
    for key in still_targeting {
        targeting::lose_target(ctx, key);
    }

    let Some((agent, position)) = ctx.map.remove_agent(agent_key) else {
        return;
    };
    ctx.events.push(BroadcastMessage::AgentDespawned {
        agent_key,
        position: position.clone(),
        snapshot: None,
    });
    schedule_respawn(ctx, &agent);

    let agent_corpse = agent.get_corpse();
    let Some(config) = ITEM_CONFIGS.get(&agent_corpse) else {
        error!("creature corpse with id {} not found", agent_corpse);
        return;
    };
    let mut corpse = Item::new(config.clone(), 1);
    if let Some(creature) = agent.get_creature_kind() {
        roll_creature_loot(&mut corpse, creature, ctx.roll);
    }
    if let Err(e) = insert_item_at(ctx, corpse, &ItemPlacement::Map(position.clone()), None) {
        error!(
            "Error inserting creature corpse at tile {}: {}",
            position, e
        );
    }
}

fn schedule_respawn(ctx: &mut TickCtx, dead: &Agent) {
    let Some(respawn_ticks) = dead.respawn_ticks() else {
        return;
    };
    let Some(kind) = dead.creature_kind().cloned() else {
        return;
    };
    ctx.scheduled.push(ScheduledCommand {
        at_tick: ctx.tick + respawn_ticks,
        command: WorldCommand::SpawnCreature {
            kind,
            position: dead.get_origin().clone(),
            respawn_ticks: Some(respawn_ticks),
        },
    });
}

fn roll_creature_loot(corpse: &mut Item, creature: &CreatureKind, rolls: &mut Rolls) {
    for loot in &creature.loot_table {
        let mut total = 0;
        for _ in 0..rolls.drop_rolls(100) {
            if rolls.chance(loot.chance) {
                total += if loot.amount > 1 {
                    rolls.uniform(1, loot.amount)
                } else {
                    1
                };
            }
        }
        if total > 0 && add_to_corpse(corpse, loot.item_id, total) {
            break;
        }
    }
}

// returns true when the corpse is full
fn add_to_corpse(corpse: &mut Item, item_id: ItemId, amount: u32) -> bool {
    let Some(config) = ITEM_CONFIGS.get(&item_id) else {
        error!(
            "Item id {} present in loot table but missing from item config",
            item_id
        );
        return false;
    };

    let mut add_content = |item: Item| {
        if let Some(content) = &mut corpse.content {
            content.push(item);
        } else {
            error!("Corpse {} is not a container", corpse.item_id);
        }
        corpse.available_capacity() == Some(0) || corpse.available_capacity().is_none()
    };

    if config.has_flag(ItemFlag::Cumulative) {
        let (full, remaining) = (
            amount / MAX_STACK_AMOUNT as u32,
            amount % MAX_STACK_AMOUNT as u32,
        );
        for _ in 0..full {
            if add_content(Item::new(config.clone(), MAX_STACK_AMOUNT)) {
                return true;
            }
        }

        if remaining > 0 && add_content(Item::new(config.clone(), remaining as u8)) {
            return true;
        }
    } else {
        for _ in 0..amount {
            if add_content(Item::new(config.clone(), 1)) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::Position;
    use crate::entities::world_map::WorldMap;
    use crate::game::TestHarness;
    use crate::game::{Tick, TickDelta};
    use crate::persistence::test_fixtures::{
        a_creature_kind, a_test_creature, a_test_creature_worth, a_test_snapshot,
    };
    use std::sync::Arc;

    /// A creature the spawn table put there, so it books a replacement when it dies.
    fn a_spawned_creature(origin: Position, respawn_ticks: TickDelta) -> Agent {
        Agent::respawning(Arc::new(a_creature_kind("Rat")), origin, respawn_ticks)
    }

    #[test]
    fn a_reaped_spawn_creature_books_its_replacement_at_the_point_it_came_from() {
        let origin = Position::new(10, 10, 7);
        let died_at = Position::new(14, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(origin.clone(), MapTile::new());
        map.insert_tile(died_at.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_spawned_creature(origin.clone(), TickDelta(600)), &died_at)
            .unwrap();
        map.get_agent_mut(rat).unwrap().remove_life(1);
        let mut h = TestHarness::seeded(1);
        h.tick = Tick(50);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, None);

        assert!(
            matches!(
                h.scheduled.as_slice(),
                [ScheduledCommand {
                    at_tick: Tick(650),
                    command: WorldCommand::SpawnCreature {
                        position,
                        respawn_ticks: Some(TickDelta(600)),
                        ..
                    },
                }] if *position == origin
            ),
            "{:?}",
            h.scheduled
        );
    }

    /// A creature no spawn point owns — a future summon, or one a test placed — must not
    /// book anything, or killing it would populate the map forever.
    #[test]
    fn a_creature_from_no_spawn_point_books_nothing() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 0, (1, 2)), &pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, None);

        assert!(map.get_agent(rat).is_none(), "it should still be reaped");
        assert!(h.scheduled.is_empty());
    }

    #[test]
    fn removes_the_creature_and_announces_it() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 0, (1, 2)), &pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, None);

        assert!(map.get_agent(rat).is_none());
        assert!(matches!(
            h.events.as_slice(),
            [BroadcastMessage::AgentDespawned { agent_key, .. }] if *agent_key == rat
        ));
        assert!(map.delta().tile_dirty(&pos));
    }

    #[test]
    fn is_a_no_op_on_a_living_creature() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 5, (1, 2)), &pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, None);

        assert!(map.get_agent(rat).is_some());
        assert!(h.events.is_empty());
    }

    #[test]
    fn clears_a_target_that_named_the_dead() {
        let rat_pos = Position::new(10, 10, 7);
        let hunter_pos = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(hunter_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 0, (1, 2)), &rat_pos)
            .unwrap();
        let hunter = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &hunter_pos)
            .unwrap();
        map.get_agent_mut(hunter).unwrap().set_target(Some(rat), 0);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, Some(hunter));

        assert_eq!(map.get_agent(hunter).unwrap().target(), None);
        assert!(h.events.iter().any(|m| matches!(
            m,
            BroadcastMessage::AgentLostTarget { agent_key, .. } if *agent_key == hunter
        )));
    }

    #[test]
    fn leaves_a_bystander_targeting_someone_else_alone() {
        let rat_pos = Position::new(10, 10, 7);
        let hunter_pos = Position::new(11, 10, 7);
        let bystander_pos = Position::new(12, 10, 7);
        let third_pos = Position::new(13, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(hunter_pos.clone(), MapTile::new());
        map.insert_tile(bystander_pos.clone(), MapTile::new());
        map.insert_tile(third_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature("Rat", 0, (1, 2)), &rat_pos)
            .unwrap();
        let hunter = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &hunter_pos)
            .unwrap();
        let bystander = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 2)), &bystander_pos)
            .unwrap();
        let third = map
            .insert_agent(a_test_creature("Cave Rat", 0, (1, 2)), &third_pos)
            .unwrap();
        map.get_agent_mut(hunter).unwrap().set_target(Some(rat), 0);
        map.get_agent_mut(bystander)
            .unwrap()
            .set_target(Some(third), 0);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, Some(hunter));

        assert_eq!(map.get_agent(hunter).unwrap().target(), None);
        assert_eq!(map.get_agent(bystander).unwrap().target(), Some(third));
        assert!(!h.events.iter().any(|m| matches!(
            m,
            BroadcastMessage::AgentLostTarget { agent_key, .. } if *agent_key == bystander
        )));
    }

    #[test]
    fn is_a_no_op_on_a_player() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), player, None);

        assert!(map.get_agent(player).is_some());
        assert!(h.events.is_empty());
    }

    #[test]
    fn a_kill_awards_the_creatures_experience_before_removing_it() {
        let rat_pos = Position::new(10, 10, 7);
        let hunter_pos = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(hunter_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature_worth("Rat", 0, (1, 2), 100), &rat_pos)
            .unwrap();
        let hunter = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &hunter_pos)
            .unwrap();
        map.get_agent_mut(rat).unwrap().record_damage(hunter, 100);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, Some(hunter));

        assert!(map.get_agent(rat).is_none());
        assert_eq!(
            map.get_player(hunter)
                .unwrap()
                .skills()
                .get(&crate::entities::skills::SkillType::Level)
                .unwrap()
                .value,
            2
        );
    }

    #[test]
    fn a_creature_nobody_damaged_awards_nothing() {
        let rat_pos = Position::new(10, 10, 7);
        let hunter_pos = Position::new(11, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(rat_pos.clone(), MapTile::new());
        map.insert_tile(hunter_pos.clone(), MapTile::new());
        let rat = map
            .insert_agent(a_test_creature_worth("Rat", 0, (1, 2), 100), &rat_pos)
            .unwrap();
        let hunter = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &hunter_pos)
            .unwrap();
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat, Some(hunter));

        assert_eq!(
            map.get_player(hunter)
                .unwrap()
                .skills()
                .get(&crate::entities::skills::SkillType::Level)
                .unwrap()
                .value,
            1
        );
    }
}

use tracing::error;

use crate::actors::world::{ScheduledCommand, WorldCommand};
use crate::constants::items::MAX_STACK_AMOUNT;
use crate::entities::agent::{Agent, AgentKey};
use crate::entities::creature::CreatureKind;
use crate::entities::inventory::{InventorySlot, PLAYER_DROP_SLOTS};
use crate::entities::items::{Item, ItemFlag, ItemId};
use crate::entities::position::ItemPlacement;
use crate::game::TickCtx;
use crate::game::config::GAME_CONFIG;
use crate::game::events::BroadcastMessage;
use crate::game::item_action::check_decay;
use crate::game::item_movement::insert_item_at;
use crate::game::random::Rolls;
use crate::game::{experience, skills, targeting};
use crate::persistence::items::ITEM_CONFIGS;

pub fn reap(ctx: &mut TickCtx, agent_key: AgentKey) {
    let Some(dead) = ctx.map.get_agent(agent_key) else {
        return;
    };
    if dead.life().current > 0 || dead.is_npc() {
        return;
    }

    experience::award(ctx, agent_key);

    let mut still_targeting = Vec::new();
    let mut remembering = Vec::new();
    for (key, other) in ctx.map.iter_agents() {
        if other.target() == Some(agent_key) {
            still_targeting.push(key);
        }
        if experience::remembers(other, agent_key) {
            remembering.push(key);
        }
    }
    for key in still_targeting {
        targeting::lose_target(ctx, key);
    }
    experience::forget_departed(ctx.map, agent_key, &remembering);

    if ctx.map.get_player(agent_key).is_some() {
        skills::apply_death_loss(ctx, agent_key, GAME_CONFIG.death_loss_percent);
    }

    let Some((mut agent, position)) = ctx.map.remove_agent(agent_key) else {
        return;
    };
    let agent_corpse = agent.get_corpse();
    let Some(config) = ITEM_CONFIGS.get(&agent_corpse) else {
        error!("creature corpse with id {} not found", agent_corpse);
        return;
    };
    let mut corpse = Item::new(config.clone(), 1);
    if let Some(creature) = agent.get_creature_kind() {
        roll_creature_loot(&mut corpse, creature, ctx.roll);
    } else {
        roll_player_drop_chance(&mut corpse, &mut agent, ctx.roll);
    }
    let placement = ItemPlacement::Map(position.clone());
    check_decay(ctx.scheduled, &corpse, placement.clone(), ctx.tick);
    if let Err(e) = insert_item_at(ctx, corpse, &placement, None) {
        error!(
            "Error inserting creature corpse at tile {}: {}",
            position, e
        );
    }

    if agent.is_player() {
        reset_player(&mut agent);
        let origin = agent.get_origin().clone();
        if let Some(snapshot) = agent.to_snapshot(origin) {
            ctx.persistence.save_player(snapshot, ctx.tick);
        }

        ctx.events.push(BroadcastMessage::PlayerDied { agent_key });
    }
    ctx.events.push(BroadcastMessage::AgentDespawned {
        agent_key,
        position,
    });
    schedule_respawn(ctx, &agent);
}

fn roll_player_drop_chance(corpse: &mut Item, agent: &mut Agent, rolls: &mut Rolls) {
    let Some(player) = agent.get_player_mut() else {
        return;
    };

    let item = player.inventory_mut().take_slot(&InventorySlot::Backpack);
    if let Some(item) = item
        && let Some(content) = corpse.content_mut()
    {
        content.push(item);
    }

    for slot in PLAYER_DROP_SLOTS {
        if rolls.uniform(0, 100) < GAME_CONFIG.slot_drop_chance {
            let item = player.inventory_mut().take_slot(&slot);
            if let Some(item) = item
                && let Some(content) = corpse.content_mut()
            {
                content.push(item);
            }
        }
    }
}

fn reset_player(agent: &mut Agent) {
    agent.restore_life(agent.life().missing());
    agent.restore_mana(agent.mana().missing());
    // TODO: reset conditions
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
        if let Some(content) = corpse.content_mut() {
            content.push(item);
        } else {
            error!("Corpse {} is not a container", corpse.id());
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
        reap(&mut h.ctx(&mut map), rat);

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
        reap(&mut h.ctx(&mut map), rat);

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
        reap(&mut h.ctx(&mut map), rat);

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
        reap(&mut h.ctx(&mut map), rat);

        assert!(map.get_agent(rat).is_some());
        assert!(h.events.is_empty());
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
        reap(&mut h.ctx(&mut map), rat);

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
        reap(&mut h.ctx(&mut map), player);

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
        reap(&mut h.ctx(&mut map), rat);

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
        reap(&mut h.ctx(&mut map), rat);

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

    #[test]
    fn a_dead_player_is_saved_with_the_loss_and_full_at_the_lower_maximum() {
        use crate::actors::persistence::PersistenceCommand;
        use crate::entities::skills::{SkillType, SkillValue};
        use crate::entities::vocation::Vocation;
        use std::collections::HashMap;

        let mut snapshot = a_test_snapshot(1, 1);
        snapshot.skills = HashMap::from([(
            SkillType::Level,
            SkillValue {
                value: 3,
                current_ticks: 0,
            },
        )]);
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(Agent::from_player(snapshot), &pos)
            .unwrap();
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);
        map.agent_mut(key).unwrap().remove_life(100);

        reap(&mut h.ctx(&mut map), key);

        let Ok(PersistenceCommand::SavePlayer(saved, _)) = h.persistence_rx.try_recv() else {
            panic!("the dead player was not saved");
        };
        let knight = Vocation::Knight;
        assert_eq!(saved.skills[&SkillType::Level].value, 2);
        assert_eq!(saved.life.maximum, (100 - knight.life_on_level_up()) as u32);
        assert_eq!(saved.life.current, saved.life.maximum);
        assert_eq!(saved.mana.maximum, (100 - knight.mana_on_level_up()) as u32);
        assert_eq!(saved.mana.current, saved.mana.maximum);
    }

    #[test]
    fn a_dead_creature_leaves_the_tables_of_the_players_it_hit() {
        let mut map = GameMap::new();
        let rat_at = Position::new(10, 10, 7);
        let player_at = Position::new(11, 10, 7);
        let wolf_at = Position::new(12, 10, 7);
        for pos in [&rat_at, &player_at, &wolf_at] {
            map.insert_tile(pos.clone(), MapTile::new());
        }
        let rat = map
            .insert_agent(a_test_creature("Rat", 0, (1, 2)), &rat_at)
            .unwrap();
        let wolf = map
            .insert_agent(a_test_creature("Wolf", 100, (1, 2)), &wolf_at)
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &player_at)
            .unwrap();
        map.get_agent_mut(player).unwrap().record_damage(rat, 20);
        map.get_agent_mut(player).unwrap().record_damage(wolf, 30);
        let mut h = TestHarness::seeded(1);

        let mut map = WorldMap::new(map);
        reap(&mut h.ctx(&mut map), rat);

        let table = map.get_agent(player).unwrap().participation();
        assert!(!table.contains(rat));
        assert!(table.contains(wolf));
        assert_eq!(table.total(), 30);
    }
}

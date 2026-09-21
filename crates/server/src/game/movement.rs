use std::sync::Arc;

use smallvec::SmallVec;
use tracing::error;

use crate::entities::{
    agent::{AgentKey, Facing},
    conditions::ConditionSpec,
    items::FloorChangeDirection,
    map::GameMap,
    position::{Direction, Position},
};

use super::TickCtx;
use super::conditions::apply_condition;
use super::events::BroadcastMessage;

const MAX_STEP_CHAIN: u8 = 4;

enum StepEffect {
    Condition {
        spec: Arc<ConditionSpec>,
        owner: Option<AgentKey>,
    },
    FloorChange(FloorChangeDirection),
}

pub fn walk(ctx: &mut TickCtx, direction: Direction, agent_key: AgentKey) {
    let Some(agent) = ctx.map.get_agent(agent_key) else {
        error!("agent {agent_key:?} not spawned");
        return;
    };
    let next_walk_tick = agent.next_walk_tick;
    let Some(current_pos) = ctx.map.agent_position(agent_key).cloned() else {
        error!("agent {agent_key:?} position not found");
        return;
    };

    if next_walk_tick > ctx.tick {
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    }

    let new_pos = current_pos.clone() + direction;
    if !ctx.map.can_move(&new_pos, agent_key) {
        ctx.map
            .agent_mut(agent_key)
            .unwrap()
            .set_facing(direction_to_facing(&direction));
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    }

    let Some(tile_friction) = ctx.map.tile_friction(&new_pos) else {
        ctx.events
            .push(BroadcastMessage::AgentWalkDenied { agent_key });
        return;
    };

    let walk_ticks = ctx
        .map
        .get_agent(agent_key)
        .unwrap()
        .calculate_walk_ticks(tile_friction, direction.is_diagonal());

    ctx.map
        .agent_mut(agent_key)
        .unwrap()
        .stamp_walk(ctx.tick + walk_ticks);
    if let Err(e) = ctx
        .map
        .step_agent(agent_key, &new_pos, direction_to_facing(&direction))
    {
        error!("agent {agent_key:?} could not walk to {new_pos}: {e:?}");
        return;
    }
    ctx.events.push(BroadcastMessage::AgentMoved {
        agent_key,
        direction,
        from_position: current_pos,
        to_position: new_pos.clone(),
    });

    on_step(ctx, agent_key, &new_pos);
}

pub fn on_step(ctx: &mut TickCtx, agent_key: AgentKey, pos: &Position) {
    step_onto(ctx, agent_key, pos, 0);
}

fn step_onto(ctx: &mut TickCtx, agent_key: AgentKey, pos: &Position, depth: u8) {
    for effect in step_effects(ctx.map, pos) {
        if ctx.map.get_agent(agent_key).is_none() {
            return;
        }
        match effect {
            StepEffect::Condition { spec, owner } => {
                apply_condition(ctx, agent_key, &spec, owner);
            }
            StepEffect::FloorChange(direction) => {
                if depth >= MAX_STEP_CHAIN {
                    error!("agent {agent_key:?} is caught in a floor-change loop at {pos}");
                    return;
                }
                let destination = floor_change_destination(ctx.map, pos, direction);
                if let Err(e) = ctx.map.move_agent(agent_key, &destination) {
                    error!(
                        "agent {agent_key:?} could not take the floor change to {destination}: {e:?}"
                    );
                    return;
                }
                ctx.events.push(BroadcastMessage::AgentTeleported {
                    agent_key,
                    from_position: pos.clone(),
                    to_position: destination.clone(),
                });
                return step_onto(ctx, agent_key, &destination, depth + 1);
            }
        }
    }
}

fn step_effects(map: &GameMap, pos: &Position) -> SmallVec<[StepEffect; 2]> {
    let mut effects = SmallVec::new();
    let mut movement = None;
    if let Ok(items) = map.iter_items(pos) {
        for item in items {
            if let Some(spec) = item.config.attr_field() {
                effects.push(StepEffect::Condition {
                    spec: spec.clone(),
                    owner: item.owner,
                });
            }
            if movement.is_none() {
                movement = item.config.attr_floor_change().map(StepEffect::FloorChange);
            }
        }
    }
    effects.extend(movement);
    effects
}

fn floor_change_destination(
    map: &GameMap,
    pos: &Position,
    direction: FloorChangeDirection,
) -> Position {
    match direction {
        FloorChangeDirection::Up => Position::new(pos.x, pos.y, pos.z - 1),
        FloorChangeDirection::Down => {
            if let Some(downstairs_change) =
                map.get_floor_change(&Position::new(pos.x, pos.y, pos.z + 1))
            {
                let (x, y) = match downstairs_change {
                    FloorChangeDirection::Up | FloorChangeDirection::Down => (pos.x, pos.y),
                    FloorChangeDirection::North => (pos.x, pos.y + 1),
                    FloorChangeDirection::East => (pos.x - 1, pos.y),
                    FloorChangeDirection::South => (pos.x, pos.y - 1),
                    FloorChangeDirection::West => (pos.x + 1, pos.y),
                };
                Position::new(x, y, pos.z + 1)
            } else {
                Position::new(pos.x, pos.y, pos.z + 1)
            }
        }
        FloorChangeDirection::North => Position::new(pos.x, pos.y - 1, pos.z - 1),
        FloorChangeDirection::East => Position::new(pos.x + 1, pos.y, pos.z - 1),
        FloorChangeDirection::South => Position::new(pos.x, pos.y + 1, pos.z - 1),
        FloorChangeDirection::West => Position::new(pos.x - 1, pos.y, pos.z - 1),
    }
}

fn direction_to_facing(direction: &Direction) -> Facing {
    match direction {
        Direction::North => Facing::North,
        Direction::East => Facing::East,
        Direction::South => Facing::South,
        Direction::West => Facing::West,
        Direction::NorthEast => Facing::East,
        Direction::NorthWest => Facing::West,
        Direction::SouthEast => Facing::East,
        Direction::SouthWest => Facing::West,
    }
}

pub fn change_direction(ctx: &mut TickCtx, agent_key: AgentKey, facing: Facing) {
    if let Some(mut agent) = ctx.map.agent_mut(agent_key) {
        agent.set_facing(facing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::Bounds;
    use crate::entities::agent::Agent;
    use crate::entities::combat::CombatElement;
    use crate::entities::conditions::{ConditionSpec, SpecSchedule};
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag, ItemId};
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::world_map::WorldMap;
    use crate::game::TestHarness;
    use crate::game::{Tick, TickDelta};
    use crate::persistence::test_fixtures::a_test_snapshot;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn a_floor_tile() -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "ground".to_string(),
                None,
                None,
                HashSet::from([ItemFlag::Ground]),
                vec![ItemAttribute::TileFriction(100)],
            )),
            1,
        ));
        tile
    }

    fn a_player_on(tiles: &[Position]) -> (WorldMap, AgentKey) {
        let mut map = GameMap::new();
        for pos in tiles {
            map.insert_tile(pos.clone(), a_floor_tile());
        }
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &tiles[0])
            .unwrap();
        (WorldMap::new(map), key)
    }

    #[test]
    fn a_walk_into_nothing_turns_the_walker_and_marks_the_turn() {
        let (mut map, player) = a_player_on(&[Position::new(10, 10, 7)]);
        let mut h = TestHarness::new();

        walk(&mut h.ctx(&mut map), Direction::North, player);

        assert_eq!(map.get_agent(player).unwrap().facing(), Facing::North);
        assert!(map.delta().agent(player).facing());
    }

    #[test]
    fn a_step_turns_the_walker_without_marking_a_turn() {
        let (here, east) = (Position::new(10, 10, 7), Position::new(11, 10, 7));
        let (mut map, player) = a_player_on(&[here, east.clone()]);
        let mut h = TestHarness::new();

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&east));
        assert_eq!(map.get_agent(player).unwrap().facing(), Facing::East);
        assert!(!map.delta().agent(player).facing());
    }

    fn an_item_with(attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(2),
                "thing".to_string(),
                None,
                None,
                [],
                attributes,
            )),
            1,
        )
    }

    fn a_field(damage: u32) -> Item {
        an_item_with(vec![ItemAttribute::Field(Arc::new(ConditionSpec {
            element: CombatElement::Fire,
            damage: Bounds {
                min: damage,
                max: damage,
            },
            interval: TickDelta(200),
            schedule: SpecSchedule::Flat { count: 7 },
            delayed: false,
        }))])
    }

    fn a_floor_change(direction: FloorChangeDirection) -> Item {
        an_item_with(vec![ItemAttribute::FloorChange(direction)])
    }

    const HERE: Position = Position { x: 10, y: 10, z: 7 };
    const EAST: Position = Position { x: 11, y: 10, z: 7 };

    /// A player on `HERE`, floor tiles at `HERE`, `EAST` and every position in `more`, and
    /// `items` placed on the tiles they name.
    fn a_walk(more: &[Position], items: Vec<(Position, Item)>) -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        for pos in [HERE, EAST].iter().chain(more) {
            map.insert_tile(pos.clone(), a_floor_tile());
        }
        for (pos, item) in items {
            map.place_item(&pos, None, None, item).unwrap();
        }
        let key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &HERE)
            .unwrap();
        (map, key)
    }

    fn life(map: &WorldMap, key: AgentKey) -> u32 {
        map.get_agent(key).unwrap().life().current
    }

    fn teleports(h: &TestHarness) -> usize {
        h.events
            .iter()
            .filter(|e| matches!(e, BroadcastMessage::AgentTeleported { .. }))
            .count()
    }

    #[test]
    fn stepping_into_a_field_hits_at_once_and_credits_its_owner() {
        let elsewhere = Position::new(20, 20, 7);
        let (mut map, player) = a_walk(std::slice::from_ref(&elsewhere), Vec::new());
        let owner = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &elsewhere)
            .unwrap();
        let mut field = a_field(20);
        field.owner = Some(owner);
        map.place_item(&EAST, None, None, field).unwrap();
        let mut map = WorldMap::new(map);
        let before = life(&map, player);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(life(&map, player), before - 20);
        assert!(h.events.iter().any(|e| matches!(
            e,
            BroadcastMessage::DamageTaken { source: Some(s), target, .. }
                if *s == owner && *target == player
        )));
        assert!(h.scheduled.iter().any(|s| s.at_tick == Tick(200)));
    }

    #[test]
    fn a_spent_field_does_nothing() {
        let (map, player) = a_walk(&[], vec![(EAST, a_field(0))]);
        let mut map = WorldMap::new(map);
        let before = life(&map, player);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(life(&map, player), before);
        assert!(h.scheduled.is_empty());
    }

    #[test]
    fn a_floor_change_moves_the_walker_and_reports_it() {
        let below = Position::new(11, 10, 8);
        let (map, player) = a_walk(
            std::slice::from_ref(&below),
            vec![(EAST, a_floor_change(FloorChangeDirection::Down))],
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&below));
        assert_eq!(teleports(&h), 1);
    }

    #[test]
    fn a_burning_stair_burns_before_it_moves() {
        let below = Position::new(11, 10, 8);
        let (map, player) = a_walk(
            std::slice::from_ref(&below),
            vec![
                (EAST, a_floor_change(FloorChangeDirection::Down)),
                (EAST, a_field(20)),
            ],
        );
        let mut map = WorldMap::new(map);
        let before = life(&map, player);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&below));
        assert_eq!(life(&map, player), before - 20);
        let hit = h
            .events
            .iter()
            .position(|e| matches!(e, BroadcastMessage::DamageTaken { .. }))
            .unwrap();
        let moved = h
            .events
            .iter()
            .position(|e| matches!(e, BroadcastMessage::AgentTeleported { .. }))
            .unwrap();
        assert!(hit < moved);
    }

    #[test]
    fn a_field_where_a_floor_change_lands_applies_too() {
        let below = Position::new(11, 10, 8);
        let (map, player) = a_walk(
            std::slice::from_ref(&below),
            vec![
                (EAST, a_floor_change(FloorChangeDirection::Down)),
                (below.clone(), a_field(20)),
            ],
        );
        let mut map = WorldMap::new(map);
        let before = life(&map, player);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&below));
        assert_eq!(life(&map, player), before - 20);
    }

    #[test]
    fn floor_changes_that_lead_into_each_other_stop_at_the_cap() {
        let above = Position::new(11, 10, 6);
        let (map, player) = a_walk(
            std::slice::from_ref(&above),
            vec![
                (EAST, a_floor_change(FloorChangeDirection::Up)),
                (above.clone(), a_floor_change(FloorChangeDirection::Down)),
            ],
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(teleports(&h), MAX_STEP_CHAIN as usize);
    }
}

use std::sync::Arc;

use smallvec::SmallVec;
use tracing::{error, warn};

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
    Teleport(Position),
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

/// Moves an agent one tile without its say: a step that ignores its walk cooldown, stamps a new
/// one, and leaves its facing alone.
pub fn push(ctx: &mut TickCtx, agent_key: AgentKey, direction: Direction) {
    let Some(from) = ctx.map.agent_position(agent_key).cloned() else {
        return;
    };
    let to = from.clone() + direction;
    let (Some(agent), Some(friction)) = (ctx.map.get_agent(agent_key), ctx.map.tile_friction(&to))
    else {
        return;
    };
    let facing = agent.facing();
    let walk_ticks = agent.calculate_walk_ticks(friction, direction.is_diagonal());

    ctx.map
        .agent_mut(agent_key)
        .unwrap()
        .stamp_walk(ctx.tick + walk_ticks);
    if let Err(e) = ctx.map.step_agent(agent_key, &to, facing) {
        error!("agent {agent_key:?} could not be pushed to {to}: {e:?}");
        return;
    }
    ctx.events.push(BroadcastMessage::AgentMoved {
        agent_key,
        direction,
        from_position: from,
        to_position: to.clone(),
    });
    on_step(ctx, agent_key, &to);
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
                let destination = floor_change_destination(ctx.map, pos, direction);
                return relocate(ctx, agent_key, pos, &destination, depth);
            }
            StepEffect::Teleport(destination) => {
                return relocate(ctx, agent_key, pos, &destination, depth);
            }
        }
    }
}

fn relocate(ctx: &mut TickCtx, agent_key: AgentKey, from: &Position, to: &Position, depth: u8) {
    if depth >= MAX_STEP_CHAIN {
        error!("agent {agent_key:?} is caught in a floor-change loop at {from}");
        return;
    }
    if ctx.map.get_tile(to).is_err() {
        warn!("agent {agent_key:?} cannot be moved from {from} to {to}: there is no tile");
        return;
    }
    if let Err(e) = ctx.map.move_agent(agent_key, to) {
        error!("agent {agent_key:?} could not be moved from {from} to {to}: {e:?}");
        return;
    }
    ctx.events.push(BroadcastMessage::AgentTeleported {
        agent_key,
        from_position: from.clone(),
        to_position: to.clone(),
    });
    step_onto(ctx, agent_key, to, depth + 1);
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
    effects.extend(
        map.teleport_destination(pos)
            .map(StepEffect::Teleport)
            .or(movement),
    );
    effects
}

fn floor_change_destination(
    map: &GameMap,
    pos: &Position,
    direction: FloorChangeDirection,
) -> Position {
    let (x, y, z) = (pos.x, pos.y, pos.z);
    match direction {
        FloorChangeDirection::Up => Position::new(x, y, z - 1),
        FloorChangeDirection::Down => down_destination(map, pos),
        FloorChangeDirection::North => Position::new(x, y.saturating_sub(1), z - 1),
        FloorChangeDirection::East => Position::new(x.saturating_add(1), y, z - 1),
        FloorChangeDirection::South => Position::new(x, y.saturating_add(1), z - 1),
        FloorChangeDirection::West => Position::new(x.saturating_sub(1), y, z - 1),
        FloorChangeDirection::EastAlt => Position::new(x.saturating_add(2), y, z - 1),
        FloorChangeDirection::SouthAlt => Position::new(x, y.saturating_add(2), z - 1),
    }
}

/// Where a hole or a downward stair at `pos` lands, which depends on the ramp below it: Canary's
/// `Tile::queryDestination`.
fn down_destination(map: &GameMap, pos: &Position) -> Position {
    let (x, y, z) = (pos.x, pos.y, pos.z + 1);
    let below = |x: u16, y: u16| map.get_floor_change(&Position::new(x, y, z));

    if below(x, y.saturating_sub(1)) == Some(FloorChangeDirection::SouthAlt) {
        return Position::new(x, y.saturating_sub(2), z);
    }
    if below(x.saturating_sub(1), y) == Some(FloorChangeDirection::EastAlt) {
        return Position::new(x.saturating_sub(2), y, z);
    }
    let (x, y) = match below(x, y) {
        Some(FloorChangeDirection::North) => (x, y.saturating_add(1)),
        Some(FloorChangeDirection::South) => (x, y.saturating_sub(1)),
        Some(FloorChangeDirection::SouthAlt) => (x, y.saturating_sub(2)),
        Some(FloorChangeDirection::East) => (x.saturating_sub(1), y),
        Some(FloorChangeDirection::EastAlt) => (x.saturating_sub(2), y),
        Some(FloorChangeDirection::West) => (x.saturating_add(1), y),
        _ => (x, y),
    };
    Position::new(x, y, z)
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
    fn a_push_is_a_walk_that_keeps_the_facing() {
        let (map, player) = a_walk(&[], vec![(EAST, a_field(20))]);
        let mut map = WorldMap::new(map);
        let facing = map.get_agent(player).unwrap().facing();
        let before = life(&map, player);
        let mut h = TestHarness::seeded(1);

        push(&mut h.ctx(&mut map), player, Direction::East);

        assert_eq!(map.agent_position(player), Some(&EAST));
        assert_eq!(map.get_agent(player).unwrap().facing(), facing);
        assert!(map.get_agent(player).unwrap().next_walk_tick > Tick(0));
        assert_eq!(life(&map, player), before - 20, "the landing effects run");
        assert!(h.events.iter().any(|e| matches!(
            e,
            BroadcastMessage::AgentMoved {
                direction: Direction::East,
                ..
            }
        )));
    }

    fn a_teleport() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(3),
                "teleport".to_string(),
                None,
                None,
                [ItemFlag::Teleport],
                Vec::new(),
            )),
            1,
        )
    }

    #[test]
    fn a_teleport_moves_the_walker_and_reports_it() {
        let destination = Position::new(50, 50, 7);
        let (mut map, player) = a_walk(
            std::slice::from_ref(&destination),
            vec![(EAST, a_teleport())],
        );
        map.insert_teleport(EAST, destination.clone());
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&destination));
        assert_eq!(teleports(&h), 1);
    }

    #[test]
    fn a_teleport_without_a_destination_does_nothing() {
        let (map, player) = a_walk(&[], vec![(EAST, a_teleport())]);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&EAST));
        assert_eq!(teleports(&h), 0);
    }

    #[test]
    fn a_teleport_to_a_missing_tile_leaves_the_walker_where_it_stepped() {
        let (mut map, player) = a_walk(&[], vec![(EAST, a_teleport())]);
        map.insert_teleport(EAST, Position::new(50, 50, 7));
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&EAST));
        assert!(map.iter_agents_at(&EAST).unwrap().any(|k| *k == player));
        assert_eq!(teleports(&h), 0);
    }

    #[test]
    fn a_teleport_onto_a_stair_takes_the_stair_too() {
        let (stair, below) = (Position::new(50, 50, 7), Position::new(50, 50, 8));
        let (mut map, player) = a_walk(
            &[stair.clone(), below.clone()],
            vec![
                (EAST, a_teleport()),
                (stair.clone(), a_floor_change(FloorChangeDirection::Down)),
            ],
        );
        map.insert_teleport(EAST, stair);
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&below));
        assert_eq!(teleports(&h), 2);
    }

    #[test]
    fn an_east_alt_ramp_lands_two_tiles_east_on_the_floor_above() {
        let landing = Position::new(13, 10, 6);
        let (map, player) = a_walk(
            std::slice::from_ref(&landing),
            vec![(EAST, a_floor_change(FloorChangeDirection::EastAlt))],
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&landing));
    }

    #[test]
    fn going_down_beside_an_east_alt_ramp_lands_two_tiles_west() {
        let ramp = Position::new(10, 10, 8);
        let landing = Position::new(9, 10, 8);
        let (map, player) = a_walk(
            &[ramp.clone(), landing.clone()],
            vec![
                (EAST, a_floor_change(FloorChangeDirection::Down)),
                (ramp, a_floor_change(FloorChangeDirection::EastAlt)),
            ],
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&landing));
    }

    #[test]
    fn going_down_beside_a_south_alt_ramp_lands_two_tiles_north() {
        let ramp = Position::new(11, 9, 8);
        let landing = Position::new(11, 8, 8);
        let (map, player) = a_walk(
            &[ramp.clone(), landing.clone()],
            vec![
                (EAST, a_floor_change(FloorChangeDirection::Down)),
                (ramp, a_floor_change(FloorChangeDirection::SouthAlt)),
            ],
        );
        let mut map = WorldMap::new(map);
        let mut h = TestHarness::seeded(1);

        walk(&mut h.ctx(&mut map), Direction::East, player);

        assert_eq!(map.agent_position(player), Some(&landing));
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

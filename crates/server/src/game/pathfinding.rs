use std::collections::HashMap;

use pathfinding::prelude::{astar, dijkstra_all};

use crate::entities::agent::AgentKey;
use crate::entities::map::GameMap;
use crate::entities::position::{ALL_DIRECTIONS, Direction, Position, Rect};
use crate::game::TickDelta;
use crate::game::map_query::can_throw;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Goal {
    #[allow(dead_code)]
    Tile(Position),
    /// `range` is Chebyshev, as `Agent::attack_range` is.
    Within { of: Position, range: u16 },
    /// `Within`, from a tile with a missile's line of sight to `of`.
    InSight { of: Position, range: u16 },
}

impl Goal {
    pub fn adjacent(of: Position) -> Self {
        Goal::Within { of, range: 1 }
    }

    pub fn within_bounds(of: Position, range: u16) -> Self {
        Goal::Within { of, range }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Arrived,
    Move(Direction),
    Unreachable,
}

/// Ordered by how much a walker objects, so the worst item on a tile is the `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TileEntry {
    Free,
    Harmful,
    Refused,
}

/// What entering a `Harmful` tile adds, in steps of the walker's own cost for that tile.
pub const HARMFUL_FIELD_STEPS: u64 = 18;

/// The first step of the walk that reaches `goal` in the fewest ticks.
pub fn next_step(
    map: &GameMap,
    walker: AgentKey,
    goal: &Goal,
    bounds: &Rect,
    entry: impl Fn(&Position) -> TileEntry,
) -> Step {
    let Some(from) = map.agent_position(walker) else {
        return Step::Unreachable;
    };
    if goal_is_met(map, goal, from) {
        return Step::Arrived;
    }
    if goal_floor(goal) != from.z {
        return Step::Unreachable;
    }

    let found = astar(
        from,
        |pos| successors(map, walker, pos, bounds, &entry),
        |pos| heuristic(goal, pos),
        |pos| goal_is_met(map, goal, pos),
    );

    match found {
        Some((path, _)) => path
            .get(1)
            .and_then(|next| {
                Direction::from_step(next.x as i32 - from.x as i32, next.y as i32 - from.y as i32)
            })
            .map_or(Step::Unreachable, Step::Move),
        None => Step::Unreachable,
    }
}

/// The ticks it takes `walker` to reach every tile it can walk to inside `bounds`.
pub fn reachable_from(
    map: &GameMap,
    walker: AgentKey,
    bounds: &Rect,
    entry: impl Fn(&Position) -> TileEntry,
) -> Reachable {
    let Some(origin) = map.agent_position(walker).cloned() else {
        return Reachable {
            costs: HashMap::new(),
        };
    };

    let mut costs: HashMap<Position, TickDelta> =
        dijkstra_all(&origin, |pos| successors(map, walker, pos, bounds, &entry))
            .into_iter()
            .map(|(pos, (_parent, cost))| (pos, cost))
            .collect();
    costs.insert(origin, TickDelta(0));

    Reachable { costs }
}

/// The steps of a walk from `from` to a tile beside `to`, `from` excluded, over tiles anything
/// could stand on. Straight steps cost 10 and diagonals 35, Canary's `getMapWalkCost`. Empty
/// when `from` is already beside `to` or no walk stays within `search` tiles of `from`.
pub fn path_beside(map: &GameMap, from: &Position, to: &Position, search: u16) -> Vec<Position> {
    let bounds = Rect::radius(from, (search, search));
    astar(
        from,
        |pos| {
            ALL_DIRECTIONS
                .iter()
                .map(|direction| {
                    let cost: u32 = if direction.is_diagonal() { 35 } else { 10 };
                    (pos.clone() + *direction, cost)
                })
                .filter(|(next, _)| bounds.contains(next) && map.is_walkable(next))
                .collect::<Vec<_>>()
        },
        |pos| 10 * u32::from(pos.distance(to).saturating_sub(1)),
        |pos| pos.is_within(to, 1),
    )
    .map(|(path, _)| path.into_iter().skip(1).collect())
    .unwrap_or_default()
}

/// Every tile shares the origin's floor, which is why `cost_to` carries no `z` clause.
#[derive(Debug)]
pub struct Reachable {
    costs: HashMap<Position, TickDelta>,
}

impl Reachable {
    pub fn cost_to(&self, map: &GameMap, goal: &Goal) -> Option<TickDelta> {
        match goal {
            Goal::Tile(tile) => self.costs.get(tile).copied(),
            Goal::Within { of, range } | Goal::InSight { of, range } => {
                let range = *range as i32;
                (-range..=range)
                    .flat_map(|dy| (-range..=range).map(move |dx| (dx, dy)))
                    .filter_map(|(dx, dy)| of.checked_offset(dx, dy))
                    .filter_map(|pos| Some((self.costs.get(&pos).copied()?, pos)))
                    .filter(|(_, pos)| goal_is_met(map, goal, pos))
                    .map(|(cost, _)| cost)
                    .min()
            }
        }
    }
}

// private
fn successors(
    map: &GameMap,
    walker: AgentKey,
    from: &Position,
    bounds: &Rect,
    entry: &impl Fn(&Position) -> TileEntry,
) -> Vec<(Position, TickDelta)> {
    // Equal-cost routes break ties on `ALL_DIRECTIONS`' order, which is what keeps a
    // chase reproducible from its seed.
    ALL_DIRECTIONS
        .iter()
        .filter_map(|direction| {
            let to = from.clone() + *direction;
            if !bounds.contains(&to) {
                return None;
            }
            let surcharge = match entry(&to) {
                TileEntry::Refused => return None,
                TileEntry::Free => 0,
                TileEntry::Harmful => HARMFUL_FIELD_STEPS,
            };
            let cost = step_cost(map, walker, &to, direction.is_diagonal())?;
            Some((to, TickDelta(cost.0 * (1 + surcharge))))
        })
        .collect()
}

/// `None` where `movement::walk` would refuse the step, which is not only `can_move`:
/// `walk` needs friction too, and a route over a tile it would deny leaves the creature
/// re-proposing a denied step for ever.
fn step_cost(map: &GameMap, walker: AgentKey, to: &Position, diagonal: bool) -> Option<TickDelta> {
    if !map.can_move(to, walker) {
        return None;
    }
    let friction = map.tile_friction(to)?;
    let agent = map.get_agent(walker)?;
    Some(
        agent
            .calculate_walk_ticks(friction, diagonal)
            .max(TickDelta(1)),
    )
}

/// Admissible because [`step_cost`] floors at one tick, so `n` tiles cost at least `n`.
impl pathfinding::num_traits::Zero for TickDelta {
    fn zero() -> Self {
        TickDelta(0)
    }

    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

fn heuristic(goal: &Goal, from: &Position) -> TickDelta {
    let tiles = match goal {
        Goal::Tile(tile) => from.distance(tile),
        Goal::Within { of, range } | Goal::InSight { of, range } => {
            from.distance(of).saturating_sub(*range)
        }
    };
    TickDelta(tiles as u64)
}

fn goal_is_met(map: &GameMap, goal: &Goal, pos: &Position) -> bool {
    match goal {
        Goal::Tile(tile) => pos == tile,
        Goal::Within { of, range } => pos.is_within(of, *range),
        Goal::InSight { of, range } => pos.is_within(of, *range) && can_throw(map, pos, of, true),
    }
}

fn goal_floor(goal: &Goal) -> u8 {
    match goal {
        Goal::Tile(tile) => tile.z,
        Goal::Within { of, .. } | Goal::InSight { of, .. } => of.z,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::items::ItemId;
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag};
    use crate::entities::map::MapTile;
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};
    use std::collections::HashSet;
    use std::sync::Arc;

    const FLOOR: u8 = 7;
    const FAST: u16 = 100;
    const SLOW: u16 = 250;

    fn an_item(id: u16, flags: HashSet<ItemFlag>, attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(id),
                "thing".to_string(),
                None,
                None,
                flags,
                attributes,
            )),
            1,
        )
    }

    /// Both halves: `can_move` needs the `Ground` item, `walk` needs the friction.
    fn ground(friction: u16) -> Item {
        an_item(
            1,
            HashSet::from([ItemFlag::Ground]),
            [ItemAttribute::TileFriction(friction)].to_vec(),
        )
    }

    fn floor_tile(map: &mut GameMap, x: u16, y: u16, friction: u16) {
        let mut tile = MapTile::new();
        tile.push_item(ground(friction));
        map.insert_tile(Position::new(x, y, FLOOR), tile);
    }

    /// Everything off the row is void, so nothing placed on it can be walked around.
    fn a_corridor(x: std::ops::RangeInclusive<u16>) -> GameMap {
        let mut map = GameMap::new();
        for x in x {
            floor_tile(&mut map, x, 10, FAST);
        }
        map
    }

    fn a_room(x: std::ops::RangeInclusive<u16>, y: std::ops::RangeInclusive<u16>) -> GameMap {
        let mut map = GameMap::new();
        for x in x {
            for y in y.clone() {
                floor_tile(&mut map, x, y, FAST);
            }
        }
        map
    }

    fn put_creature(map: &mut GameMap, x: u16, y: u16) -> AgentKey {
        map.insert_agent(
            a_test_creature("Rat", 10, (1, 2)),
            &Position::new(x, y, FLOOR),
        )
        .unwrap()
    }

    fn put_player(map: &mut GameMap, x: u16, y: u16) -> AgentKey {
        map.insert_agent(
            Agent::from_player(a_test_snapshot(1, 1)),
            &Position::new(x, y, FLOOR),
        )
        .unwrap()
    }

    fn block(map: &mut GameMap, x: u16, y: u16, flag: ItemFlag) {
        map.place_item(
            &Position::new(x, y, FLOOR),
            None,
            None,
            an_item(2, HashSet::from([flag]), Vec::new()),
        )
        .unwrap();
    }

    fn wall(map: &mut GameMap, x: u16, y: u16) {
        map.place_item(
            &Position::new(x, y, FLOOR),
            None,
            None,
            an_item(
                2,
                HashSet::from([ItemFlag::Unpass, ItemFlag::Unsight]),
                Vec::new(),
            ),
        )
        .unwrap();
    }

    fn at(x: u16, y: u16) -> Position {
        Position::new(x, y, FLOOR)
    }

    fn everywhere() -> Rect {
        Rect::new(0, 0, u16::MAX, u16::MAX)
    }

    fn anywhere(_: &Position) -> TileEntry {
        TileEntry::Free
    }

    fn refused_at(pos: &Position, refused: Position) -> TileEntry {
        if *pos == refused {
            TileEntry::Refused
        } else {
            TileEntry::Free
        }
    }

    fn harmful_at(pos: &Position, harmful: Position) -> TileEntry {
        if *pos == harmful {
            TileEntry::Harmful
        } else {
            TileEntry::Free
        }
    }

    fn walk_ticks(map: &GameMap, walker: AgentKey, friction: u16, diagonal: bool) -> TickDelta {
        map.get_agent(walker)
            .unwrap()
            .calculate_walk_ticks(friction, diagonal)
    }

    #[test]
    fn a_walker_already_in_range_has_arrived() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        put_player(&mut map, 16, 10);

        let step = next_step(
            &map,
            rat,
            &Goal::adjacent(at(16, 10)),
            &everywhere(),
            anywhere,
        );

        assert_eq!(step, Step::Arrived);
    }

    #[test]
    fn the_first_step_of_a_corridor_points_at_the_goal() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        put_player(&mut map, 20, 10);

        let step = next_step(
            &map,
            rat,
            &Goal::adjacent(at(20, 10)),
            &everywhere(),
            anywhere,
        );

        assert_eq!(step, Step::Move(Direction::East));
    }

    #[test]
    fn a_goal_on_an_occupied_tile_is_unreachable() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        put_player(&mut map, 20, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(20, 10)), &everywhere(), anywhere);

        assert_eq!(step, Step::Unreachable);
        assert_eq!(
            next_step(
                &map,
                rat,
                &Goal::adjacent(at(20, 10)),
                &everywhere(),
                anywhere
            ),
            Step::Move(Direction::East),
            "the same chase against the adjacency goal does find a route"
        );
    }

    #[test]
    fn a_blocked_corridor_is_unreachable() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        block(&mut map, 17, 10, ItemFlag::Unpass);

        let step = next_step(&map, rat, &Goal::Tile(at(20, 10)), &everywhere(), anywhere);

        assert_eq!(step, Step::Unreachable);
    }

    #[test]
    fn a_walker_routes_around_a_tile_it_will_not_enter() {
        let mut map = a_room(5..=25, 8..=12);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(17, 10)), &everywhere(), |pos| {
            refused_at(pos, at(16, 10))
        });

        assert!(
            matches!(step, Step::Move(Direction::North | Direction::South)),
            "expected a route around the refused tile, got {step:?}"
        );
    }

    #[test]
    fn a_tile_the_walker_will_not_enter_is_not_reached() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);

        let reachable = reachable_from(&map, rat, &everywhere(), |pos| refused_at(pos, at(16, 10)));

        assert_eq!(reachable.cost_to(&map, &Goal::Tile(at(17, 10))), None);
    }

    #[test]
    fn a_harmful_tile_is_walked_around_when_the_detour_is_short() {
        let mut map = a_room(5..=25, 8..=12);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(17, 10)), &everywhere(), |pos| {
            harmful_at(pos, at(16, 10))
        });

        assert!(
            matches!(step, Step::Move(direction) if direction != Direction::East),
            "expected a route around the harmful tile, got {step:?}"
        );
    }

    #[test]
    fn a_harmful_tile_is_crossed_when_it_is_the_only_way() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(17, 10)), &everywhere(), |pos| {
            harmful_at(pos, at(16, 10))
        });

        assert_eq!(step, Step::Move(Direction::East));
    }

    #[test]
    fn a_harmful_tile_costs_eighteen_extra_steps() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        let step = walk_ticks(&map, rat, FAST, false);

        let reachable = reachable_from(&map, rat, &everywhere(), |pos| harmful_at(pos, at(16, 10)));

        assert_eq!(
            reachable.cost_to(&map, &Goal::Tile(at(17, 10))),
            Some(TickDelta(step.0 * (2 + HARMFUL_FIELD_STEPS)))
        );
    }

    #[test]
    fn a_tile_with_no_friction_is_not_walked() {
        let mut map = a_corridor(5..=25);
        let mut frictionless = MapTile::new();
        frictionless.push_item(an_item(3, HashSet::from([ItemFlag::Ground]), Vec::new()));
        map.insert_tile(at(17, 10), frictionless);
        let rat = put_creature(&mut map, 15, 10);

        assert!(map.can_move(&at(17, 10), rat), "can_move alone allows it");
        assert_eq!(
            next_step(&map, rat, &Goal::Tile(at(20, 10)), &everywhere(), anywhere),
            Step::Unreachable
        );
    }

    /// A diagonal costs 2.5x a cardinal, so two cardinals beat it and a chase staircases.
    #[test]
    fn a_cardinal_route_beats_the_diagonal_one_it_replaces() {
        let mut map = a_room(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(17, 12)), &everywhere(), anywhere);

        assert!(
            matches!(step, Step::Move(Direction::East | Direction::South)),
            "expected a cardinal first step, got {step:?}"
        );
    }

    #[test]
    fn a_diagonal_is_taken_when_it_is_the_only_way_through() {
        let mut map = GameMap::new();
        floor_tile(&mut map, 15, 10, FAST);
        floor_tile(&mut map, 16, 11, FAST);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(16, 11)), &everywhere(), anywhere);

        assert_eq!(step, Step::Move(Direction::SouthEast));
    }

    #[test]
    fn a_slow_stretch_is_walked_around() {
        let mut map = GameMap::new();
        for x in 10..=14 {
            floor_tile(
                &mut map,
                x,
                10,
                if (11..=13).contains(&x) { SLOW } else { FAST },
            );
        }
        for x in 11..=13 {
            floor_tile(&mut map, x, 9, FAST);
        }
        let rat = put_creature(&mut map, 10, 10);

        let step = next_step(&map, rat, &Goal::Tile(at(14, 10)), &everywhere(), anywhere);

        assert_eq!(step, Step::Move(Direction::NorthEast));
    }

    #[test]
    fn a_route_outside_the_bounds_is_not_taken() {
        let mut map = a_room(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15, 10);
        for y in 5..=15 {
            block(&mut map, 16, y, ItemFlag::Unpass);
        }
        // The only way past the wall is round its southern end, one row below the room.
        for x in 15..=17 {
            floor_tile(&mut map, x, 16, FAST);
        }

        let unbounded = next_step(&map, rat, &Goal::Tile(at(17, 15)), &everywhere(), anywhere);
        let bounded = next_step(
            &map,
            rat,
            &Goal::Tile(at(17, 15)),
            &Rect::new(5, 5, 25, 15),
            anywhere,
        );

        assert!(matches!(unbounded, Step::Move(_)), "the way round exists");
        assert_eq!(bounded, Step::Unreachable, "and it is outside the bounds");
    }

    #[test]
    fn a_goal_on_another_floor_is_unreachable() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);

        let step = next_step(
            &map,
            rat,
            &Goal::Tile(Position::new(17, 10, FLOOR - 1)),
            &everywhere(),
            anywhere,
        );

        assert_eq!(step, Step::Unreachable);
    }

    /// `+ Direction` saturates, so a neighbour off the map comes back as the walker's own
    /// tile rather than as nothing.
    #[test]
    fn a_walker_on_the_map_edge_routes_normally() {
        let mut map = a_room(0..=3, 0..=3);
        let rat = put_creature(&mut map, 0, 0);

        let step = next_step(&map, rat, &Goal::Tile(at(2, 0)), &everywhere(), anywhere);

        assert_eq!(step, Step::Move(Direction::East));
    }

    /// A pathfinder that broke ties on `HashMap` order would fail this only sometimes.
    #[test]
    fn the_same_question_gets_the_same_answer() {
        let mut map = a_room(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15, 10);
        let goal = Goal::Tile(at(18, 13));

        let first = next_step(&map, rat, &goal, &everywhere(), anywhere);
        for _ in 0..64 {
            assert_eq!(next_step(&map, rat, &goal, &everywhere(), anywhere), first);
        }
    }

    #[test]
    fn the_origin_costs_nothing_to_reach() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(
            reachable.cost_to(&map, &Goal::Tile(at(15, 10))),
            Some(TickDelta(0))
        );
    }

    #[test]
    fn a_cost_is_ticks_and_not_steps() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        let cardinal = walk_ticks(&map, rat, FAST, false);

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(
            reachable.cost_to(&map, &Goal::Tile(at(16, 10))),
            Some(cardinal)
        );
        assert_eq!(
            reachable.cost_to(&map, &Goal::Tile(at(18, 10))),
            Some(cardinal * 3)
        );
        assert!(
            cardinal * 2 < walk_ticks(&map, rat, FAST, true),
            "two cardinal steps are quicker than the diagonal they replace"
        );
    }

    #[test]
    fn a_tile_behind_a_wall_has_no_cost() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        block(&mut map, 17, 10, ItemFlag::Unpass);

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(reachable.cost_to(&map, &Goal::Tile(at(18, 10))), None);
        assert!(reachable.cost_to(&map, &Goal::Tile(at(16, 10))).is_some());
    }

    #[test]
    fn a_range_goal_costs_what_its_nearest_tile_costs() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        put_player(&mut map, 20, 10);
        let cardinal = walk_ticks(&map, rat, FAST, false);

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(
            reachable.cost_to(&map, &Goal::adjacent(at(20, 10))),
            Some(cardinal * 4),
            "x = 19 is the attacking tile, four steps out"
        );
    }

    #[test]
    fn a_range_goal_the_walker_already_satisfies_costs_nothing() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        put_player(&mut map, 16, 10);

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(
            reachable.cost_to(&map, &Goal::adjacent(at(16, 10))),
            Some(TickDelta(0))
        );
    }

    #[test]
    fn a_walker_that_is_not_on_the_map_reaches_nothing() {
        let map = a_corridor(5..=25);
        let orphan = AgentKey::default();

        assert_eq!(
            next_step(
                &map,
                orphan,
                &Goal::Tile(at(16, 10)),
                &everywhere(),
                anywhere
            ),
            Step::Unreachable
        );
        assert_eq!(
            reachable_from(&map, orphan, &everywhere(), anywhere)
                .cost_to(&map, &Goal::Tile(at(16, 10))),
            None
        );
    }

    fn around_the_wall() -> (Goal, Goal) {
        let of = at(19, 10);
        (
            Goal::Within {
                of: of.clone(),
                range: 4,
            },
            Goal::InSight { of, range: 4 },
        )
    }

    #[test]
    fn a_tile_in_range_behind_a_wall_does_not_meet_a_sight_goal() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        wall(&mut map, 17, 10);
        let (within, in_sight) = around_the_wall();

        assert_eq!(
            next_step(&map, rat, &within, &everywhere(), anywhere),
            Step::Arrived
        );
        assert_eq!(
            next_step(&map, rat, &in_sight, &everywhere(), anywhere),
            Step::Unreachable,
            "no tile this side of the wall sees past it"
        );
    }

    #[test]
    fn a_sight_goal_walks_out_from_behind_a_wall() {
        let mut map = a_room(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15, 10);
        wall(&mut map, 17, 10);
        let (_, in_sight) = around_the_wall();

        let step = next_step(&map, rat, &in_sight, &everywhere(), anywhere);

        assert!(matches!(step, Step::Move(_)), "got {step:?}");
    }

    #[test]
    fn a_sight_goal_costs_only_the_tiles_that_see_its_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15, 10);
        wall(&mut map, 17, 10);
        let (within, in_sight) = around_the_wall();

        let reachable = reachable_from(&map, rat, &everywhere(), anywhere);

        assert_eq!(reachable.cost_to(&map, &within), Some(TickDelta(0)));
        assert_eq!(reachable.cost_to(&map, &in_sight), None);
    }

    #[test]
    fn a_path_beside_stops_next_to_its_destination() {
        let map = a_corridor(5..=20);

        assert_eq!(
            path_beside(&map, &at(10, 10), &at(14, 10), 9),
            vec![at(11, 10), at(12, 10), at(13, 10)]
        );
    }

    #[test]
    fn a_path_beside_is_empty_when_already_beside() {
        let map = a_corridor(5..=20);

        assert!(path_beside(&map, &at(10, 10), &at(11, 11), 9).is_empty());
    }

    #[test]
    fn a_path_beside_walks_through_creatures_and_round_walls() {
        let mut map = a_room(5..=15, 5..=15);
        put_creature(&mut map, 11, 10);
        for y in 9..=11 {
            block(&mut map, 12, y, ItemFlag::Unpass);
        }

        let path = path_beside(&map, &at(10, 10), &at(14, 10), 9);

        assert!(path.contains(&at(11, 10)), "got {path:?}");
        assert!(
            path.iter()
                .all(|tile| tile.x != 12 || !(9..=11).contains(&tile.y))
        );
        assert!(path.last().unwrap().is_within(&at(14, 10), 1));
    }

    #[test]
    fn a_path_beside_takes_a_diagonal_only_when_it_must() {
        let map = a_room(5..=15, 5..=15);

        let path = path_beside(&map, &at(10, 10), &at(13, 13), 9);
        let steps: Vec<_> = std::iter::once(at(10, 10)).chain(path).collect();

        assert!(
            steps
                .windows(2)
                .all(|pair| (pair[0].x == pair[1].x) != (pair[0].y == pair[1].y)),
            "got {steps:?}"
        );
    }

    #[test]
    fn no_path_beside_means_an_empty_one() {
        let mut map = a_corridor(5..=20);
        block(&mut map, 12, 10, ItemFlag::Unpass);

        assert!(path_beside(&map, &at(10, 10), &at(14, 10), 9).is_empty());
    }
}

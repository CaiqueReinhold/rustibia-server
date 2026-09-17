use tracing::error;

use crate::entities::agent::{Agent, AgentKey};
use crate::entities::creature::{CreatureAbility, CreatureAbilityId, CreatureKind};
use crate::entities::map::GameMap;
use crate::entities::position::{Direction, Position, Rect};
use crate::game::Tick;
use crate::game::config::GAME_CONFIG;
use crate::game::map_query::can_throw;
use crate::game::pathfinding::{self, Goal};
use crate::game::random::Rolls;

// Separate from WorldCommand to limit what creatures can do
#[derive(Debug, Clone)]
pub enum CreatureAction {
    Walk {
        agent_key: AgentKey,
        direction: Direction,
    },
    SetTarget {
        agent_key: AgentKey,
        target: Option<AgentKey>,
    },
    Say {
        agent_key: AgentKey,
        message: String,
    },
    CastAbility {
        agent_key: AgentKey,
        ability_id: CreatureAbilityId,
    },
}

#[derive(Debug, Default)]
pub struct CreatureState {
    next_wander_tick: Tick,
    next_say_tick: Tick,
    ability_cooldowns: Vec<(CreatureAbilityId, Tick)>,
}

impl CreatureState {
    fn stamp_wander(&mut self, current_tick: Tick) {
        self.next_wander_tick = current_tick + GAME_CONFIG.movement.wander_ticks
    }

    fn stamp_say(&mut self, current_tick: Tick, kind: &CreatureKind) {
        self.next_say_tick = current_tick + kind.say.cooldown;
    }

    fn stamp_ability(&mut self, current_tick: Tick, ability: &CreatureAbility) {
        self.ability_cooldowns.retain(|(id, _)| *id != ability.id);
        self.ability_cooldowns
            .push((ability.id, current_tick + ability.cooldown));
    }

    fn next_ability_tick(&self, id: CreatureAbilityId) -> Tick {
        self.ability_cooldowns
            .iter()
            .find(|(caid, _)| *caid == id)
            .map(|(_, tick)| *tick)
            .unwrap_or(Tick(0))
    }
}

#[derive(Debug)]
pub struct CreatureBehaviourContext<'a> {
    pub creature: AgentKey,
    pub map: &'a GameMap,
    pub roll: Rolls,
    pub world_tick: Tick,
    pub state: &'a mut CreatureState,
}

pub fn decide_action(ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    act(get_state(&ctx), ctx)
}

// private

const WANDER_DIRECTIONS: [Direction; 4] = [
    Direction::North,
    Direction::East,
    Direction::South,
    Direction::West,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatureBehavior {
    Idle,
    InCombat,
    Fleeing,
    Returning,
}

fn get_state(ctx: &CreatureBehaviourContext) -> CreatureBehavior {
    let Some(agent) = ctx.map.get_agent(ctx.creature) else {
        error!("Creature {:?} not found on map", ctx.creature);
        return CreatureBehavior::Idle;
    };
    if !agent.is_creature() {
        error!("Agent {:?} is not creature", ctx.creature);
        return CreatureBehavior::Idle;
    }
    let Some(position) = ctx.map.agent_position(ctx.creature) else {
        error!("No position for agent {:?}", ctx.creature);
        return CreatureBehavior::Idle;
    };

    if agent.target().is_some() && agent.is_fleeing() {
        return CreatureBehavior::Fleeing;
    } else if agent.target().is_some() {
        return CreatureBehavior::InCombat;
    } else if agent.get_origin().distance(position) > GAME_CONFIG.movement.wander_distance {
        return CreatureBehavior::Returning;
    }

    CreatureBehavior::Idle
}

fn act(state: CreatureBehavior, ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    match state {
        CreatureBehavior::Idle => idle(ctx),
        CreatureBehavior::InCombat => in_combat(ctx),
        CreatureBehavior::Fleeing => fleeing(ctx),
        CreatureBehavior::Returning => returning(ctx),
    }
}

fn idle(mut ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    if let Some(target) = search_target(ctx.creature, ctx.map) {
        return Some(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(target),
        });
    }

    if let Some(wander_dir) = wander(&mut ctx) {
        return Some(CreatureAction::Walk {
            agent_key: ctx.creature,
            direction: wander_dir,
        });
    }

    if let Some(sentence) = say(&mut ctx) {
        return Some(CreatureAction::Say {
            agent_key: ctx.creature,
            message: sentence,
        });
    }

    None
}

fn in_combat(mut ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    let agent = ctx.map.get_agent(ctx.creature)?;
    let postion = ctx.map.agent_position(ctx.creature)?;
    let target_key = agent.target()?;
    let target_position = ctx.map.agent_position(target_key)?;

    // TODO: ajust for ranged creatures
    if !postion.is_within(target_position, 1) && agent.next_walk_tick <= ctx.world_tick {
        match pathfinding::next_step(
            ctx.map,
            ctx.creature,
            &Goal::adjacent(target_position.clone()),
            &Rect::player_viewport(postion),
        ) {
            pathfinding::Step::Move(direction) => {
                return Some(CreatureAction::Walk {
                    agent_key: ctx.creature,
                    direction,
                });
            }
            pathfinding::Step::Arrived => {}
            pathfinding::Step::Unreachable => {
                if let Some(new_target) = search_target(ctx.creature, ctx.map) {
                    if new_target != target_key {
                        return Some(CreatureAction::SetTarget {
                            agent_key: ctx.creature,
                            target: Some(new_target),
                        });
                    } else if !can_throw(ctx.map, postion, target_position, true) {
                        return Some(CreatureAction::SetTarget {
                            agent_key: ctx.creature,
                            target: None,
                        });
                    }
                } else {
                    return Some(CreatureAction::SetTarget {
                        agent_key: ctx.creature,
                        target: None,
                    });
                }
            }
        }
    }

    let ability = use_ability(
        ctx.creature,
        agent,
        ctx.state,
        ctx.world_tick,
        &mut ctx.roll,
    );
    if ability.is_some() {
        return ability;
    }

    // TODO: roll change target

    if let Some(sentence) = say(&mut ctx) {
        return Some(CreatureAction::Say {
            agent_key: ctx.creature,
            message: sentence,
        });
    }

    None
}

fn fleeing(mut ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    let map = ctx.map;
    let agent = map.get_agent(ctx.creature)?;
    let from = map.agent_position(ctx.creature)?;
    let to = map.agent_position(agent.target()?)?;

    if agent.next_walk_tick <= ctx.world_tick {
        let direction = escape_step(map, ctx.creature, from, to, &mut ctx.roll)?;
        return Some(CreatureAction::Walk {
            agent_key: ctx.creature,
            direction,
        });
    }

    use_ability(
        ctx.creature,
        agent,
        ctx.state,
        ctx.world_tick,
        &mut ctx.roll,
    )
}

fn returning(ctx: CreatureBehaviourContext) -> Option<CreatureAction> {
    if let Some(target) = search_target(ctx.creature, ctx.map) {
        return Some(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(target),
        });
    }

    let agent = ctx.map.get_agent(ctx.creature)?;
    let position = ctx.map.agent_position(ctx.creature)?;

    match pathfinding::next_step(
        ctx.map,
        ctx.creature,
        &Goal::within_bounds(
            agent.get_origin().clone(),
            GAME_CONFIG.movement.wander_distance,
        ),
        &Rect::player_viewport(position),
    ) {
        pathfinding::Step::Arrived => {}
        pathfinding::Step::Move(direction) => {
            return Some(CreatureAction::Walk {
                agent_key: ctx.creature,
                direction,
            });
        }
        pathfinding::Step::Unreachable => return idle(ctx),
    }

    None
}

fn wander(ctx: &mut CreatureBehaviourContext) -> Option<Direction> {
    let can_wander = ctx.state.next_wander_tick <= ctx.world_tick;
    if can_wander
        && let Some(creature_pos) = ctx.map.agent_position(ctx.creature)
        && ctx.roll.chance(GAME_CONFIG.movement.wander_chance)
    {
        ctx.state.stamp_wander(ctx.world_tick);
        let available_directions = WANDER_DIRECTIONS
            .into_iter()
            .filter(|dir| {
                let pos = creature_pos.clone() + *dir;
                ctx.map.can_move(&pos, ctx.creature)
            })
            .collect::<Vec<Direction>>();
        let direction = ctx.roll.category_roll(&available_directions);
        if let Some(direction) = direction {
            return Some(*direction);
        }
    }
    None
}

fn say(ctx: &mut CreatureBehaviourContext) -> Option<String> {
    if ctx.state.next_say_tick > ctx.world_tick {
        return None;
    }

    let kind = ctx
        .map
        .get_agent(ctx.creature)
        .and_then(|a| a.get_creature_kind())?;

    ctx.state.stamp_say(ctx.world_tick, kind);

    if ctx.roll.chance(kind.say.chance) {
        return ctx.roll.category_roll(&kind.say.sentences).cloned();
    }

    None
}

fn search_target(creature: AgentKey, map: &GameMap) -> Option<AgentKey> {
    let from = map.agent_position(creature)?;
    let viewport = Rect::player_viewport(from);
    let candidates: Vec<(AgentKey, Position)> = map
        .iter_agents_in_rect(&viewport, from.z)
        .filter(|(key, _)| {
            map.get_agent(*key)
                .is_some_and(|agent| !agent.is_creature())
        })
        .filter(|(_, pos)| can_throw(map, from, pos, true))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let reachable = pathfinding::reachable_from(map, creature, &viewport);
    candidates
        .iter()
        .filter_map(|(key, pos)| {
            let ticks = reachable.cost_to(&Goal::adjacent(pos.clone()))?;
            Some((*key, ticks, from.distance(pos)))
        })
        .min_by_key(|(_, ticks, distance)| (*ticks, *distance))
        .map(|(key, ..)| key)
        .or_else(|| {
            candidates
                .iter()
                .min_by_key(|(_, pos)| from.distance(pos))
                .map(|(key, _)| *key)
        })
}

fn use_ability(
    creature: AgentKey,
    agent: &Agent,
    state: &mut CreatureState,
    world_tick: Tick,
    roll: &mut Rolls,
) -> Option<CreatureAction> {
    let kind = agent.get_creature_kind()?;
    for ability in &kind.abilities {
        if state.next_ability_tick(ability.id) > world_tick
            || agent.next_spell_group_tick(ability.effect.cooldown_group()) > world_tick
        {
            continue;
        }
        state.stamp_ability(world_tick, ability);
        if roll.chance(ability.chance) {
            return Some(CreatureAction::CastAbility {
                agent_key: creature,
                ability_id: ability.id,
            });
        }
    }

    None
}

/// One rung of the escape ladder: up to four candidate steps as `(dx, dy)` offsets.
type Rung = [(i32, i32); 4];

const NO_STEP: (i32, i32) = (0, 0);
const NO_RUNG: Rung = [NO_STEP; 4];

fn escape_step(
    map: &GameMap,
    creature: AgentKey,
    from: &Position,
    to: &Position,
    roll: &mut Rolls,
) -> Option<Direction> {
    let offset_x = from.x as i32 - to.x as i32;
    let offset_y = from.y as i32 - to.y as i32;

    for rung in escape_ladder(offset_x, offset_y) {
        let options: Vec<Direction> = rung
            .into_iter()
            .filter_map(|(dx, dy)| Direction::from_step(dx, dy))
            .filter(|direction| {
                let pos = from.clone() + *direction;
                map.can_move(&pos, creature)
            })
            .collect();
        if let Some(direction) = roll.category_roll(&options) {
            return Some(*direction);
        }
    }

    None
}

fn escape_ladder(offset_x: i32, offset_y: i32) -> [Rung; 5] {
    let (sx, sy) = (offset_x.signum(), offset_y.signum());
    let (dx, dy) = (offset_x.abs(), offset_y.abs());

    // The target is standing on the creature, so any direction is away from it.
    if dx == 0 && dy == 0 {
        return [
            [(0, -1), (0, 1), (1, 0), (-1, 0)],
            NO_RUNG,
            NO_RUNG,
            NO_RUNG,
            NO_RUNG,
        ];
    }

    // A target on a perfect diagonal has two cardinals equally away from it
    if dx == dy {
        return [
            [(sx, 0), (0, sy), NO_STEP, NO_STEP],
            [(sx, sy), NO_STEP, NO_STEP, NO_STEP],
            [(-sx, 0), (0, -sy), NO_STEP, NO_STEP],
            NO_RUNG,
            NO_RUNG,
        ];
    }

    // Otherwise one axis dominates: open that one up, and sidestep along the other.
    let (away, perp, perp_sign) = if dy > dx {
        ((0, sy), (1, 0), sx)
    } else {
        ((sx, 0), (0, 1), sy)
    };
    let side = (perp.0 * perp_sign, perp.1 * perp_sign);

    // A target level on the perpendicular axis makes both sidesteps equally good, so they
    // share one rung; otherwise the one that also opens distance is tried first.
    let (sidestep, giving_ground) = if perp_sign == 0 {
        ([perp, (-perp.0, -perp.1), NO_STEP, NO_STEP], NO_RUNG)
    } else {
        (
            [side, NO_STEP, NO_STEP, NO_STEP],
            [(-side.0, -side.1), NO_STEP, NO_STEP, NO_STEP],
        )
    };

    [
        [away, NO_STEP, NO_STEP, NO_STEP],
        sidestep,
        giving_ground,
        [
            (away.0 + perp.0, away.1 + perp.1),
            (away.0 - perp.0, away.1 - perp.1),
            NO_STEP,
            NO_STEP,
        ],
        [(-away.0, -away.1), NO_STEP, NO_STEP, NO_STEP],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::items::MAX_DROP_CHANCE;
    use crate::entities::Bounds;
    use crate::entities::agent::Agent;
    use crate::entities::combat::CombatElement;
    use crate::entities::creature::{AbilityEffect, CreatureAbility, CreatureAttack};
    use crate::entities::creature::{CreatureAttackDamage, CreatureKind};
    use crate::entities::items::ItemId;
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag};
    use crate::entities::map::MapTile;
    use crate::entities::spells::{SpellGroup, SpellTargetMode};
    use crate::game::TickDelta;
    use crate::persistence::test_fixtures::{
        a_creature_kind, a_test_creature, a_test_creature_that_flees, a_test_snapshot,
    };
    use std::collections::HashSet;
    use std::sync::Arc;

    fn an_item(id: u16, flags: HashSet<ItemFlag>) -> Item {
        an_item_with(id, flags, Vec::new())
    }

    fn an_item_with(id: u16, flags: HashSet<ItemFlag>, attributes: Vec<ItemAttribute>) -> Item {
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

    /// A walkable tile needs both halves: `can_move` refuses a tile with no `Ground` item,
    /// and `walk` — which is what the pathfinder measures against — refuses one with no
    /// friction. A bare `MapTile::new()` is walkable to nothing.
    fn a_ground_tile() -> MapTile {
        let mut tile = MapTile::new();
        tile.push_item(an_item_with(
            1,
            HashSet::from([ItemFlag::Ground]),
            vec![ItemAttribute::TileFriction(100)],
        ));
        tile
    }

    /// A single east-west corridor at y = 10 spanning `x`, every tile grounded. Everything
    /// off that row is void, so a route around anything placed on it does not exist.
    fn a_corridor(x: std::ops::RangeInclusive<u16>) -> GameMap {
        let mut map = GameMap::new();
        for x in x {
            map.insert_tile(Position::new(x, 10, 7), a_ground_tile());
        }
        map
    }

    fn put_creature(map: &mut GameMap, x: u16) -> AgentKey {
        map.insert_agent(a_test_creature("Rat", 10, (1, 2)), &Position::new(x, 10, 7))
            .unwrap()
    }

    fn put_player(map: &mut GameMap, x: u16, id: u32) -> AgentKey {
        map.insert_agent(
            Agent::from_player(a_test_snapshot(id, 1)),
            &Position::new(x, 10, 7),
        )
        .unwrap()
    }

    /// Blocks a creature's walk without blocking its sight — only `Unpass` blocks sight,
    /// and only creatures respect `Avoid`.
    fn block_walking(map: &mut GameMap, x: u16) {
        map.place_item(
            &Position::new(x, 10, 7),
            None,
            None,
            an_item(2, HashSet::from([ItemFlag::Avoid])),
        )
        .unwrap();
    }

    fn block_sight(map: &mut GameMap, x: u16) {
        map.place_item(
            &Position::new(x, 10, 7),
            None,
            None,
            an_item(3, HashSet::from([ItemFlag::Unpass])),
        )
        .unwrap();
    }

    #[test]
    fn an_empty_view_yields_no_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);

        assert_eq!(search_target(rat, &map), None);
    }

    #[test]
    fn other_creatures_are_not_targets() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_creature(&mut map, 16);
        put_creature(&mut map, 12);

        assert_eq!(search_target(rat, &map), None);
    }

    #[test]
    fn the_nearest_player_wins_when_both_can_be_walked_to() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let far = put_player(&mut map, 10, 1);
        let near = put_player(&mut map, 18, 2);

        let found = search_target(rat, &map);

        assert_eq!(found, Some(near));
        assert_ne!(found, Some(far));
    }

    /// The whole point of the walk check: five tiles away and reachable beats three tiles
    /// away behind something a creature will not step on.
    #[test]
    fn a_walkable_player_beats_a_nearer_one_that_cannot_be_reached() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let walkable = put_player(&mut map, 10, 1);
        put_player(&mut map, 18, 2);
        block_walking(&mut map, 17);

        assert_eq!(search_target(rat, &map), Some(walkable));
    }

    /// Nothing is reachable, so the fallback runs and picks on straight-line distance
    /// alone. Without the fallback this would be `None`.
    #[test]
    fn the_nearest_unreachable_player_is_taken_when_none_can_be_reached() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 10, 1);
        let near = put_player(&mut map, 18, 2);
        block_walking(&mut map, 17);
        block_walking(&mut map, 13);

        assert_eq!(search_target(rat, &map), Some(near));
    }

    /// Distance is Chebyshev, not the axis sum: the diagonal player is three tiles out and
    /// the corridor one is five, even though the diagonal is the longer walk to add up.
    #[test]
    fn the_fallback_measures_distance_diagonally() {
        let mut map = a_corridor(5..=25);
        let off_corridor = Position::new(18, 13, 7);
        map.insert_tile(off_corridor.clone(), a_ground_tile());
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 10, 1);
        let diagonal = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &off_corridor)
            .unwrap();
        block_walking(&mut map, 13);

        assert_eq!(search_target(rat, &map), Some(diagonal));
    }

    #[test]
    fn a_player_behind_a_wall_is_not_a_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 18, 1);
        block_sight(&mut map, 17);

        assert_eq!(search_target(rat, &map), None);
    }

    /// The wall hides one player and the other is picked, so the previous test is failing
    /// on sight rather than on an empty candidate list for some unrelated reason.
    #[test]
    fn a_wall_removes_only_the_player_behind_it() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 18, 1);
        let visible = put_player(&mut map, 12, 2);
        block_sight(&mut map, 17);

        assert_eq!(search_target(rat, &map), Some(visible));
    }

    /// The viewport is 19 wide, so x = 25 is one tile past its edge from x = 15.
    #[test]
    fn a_player_outside_the_viewport_is_not_a_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 25, 1);

        assert_eq!(search_target(rat, &map), None);
        assert!(
            Rect::player_viewport(&Position::new(15, 10, 7)).contains(&Position::new(24, 10, 7)),
            "one tile nearer is inside, so the case is testing the edge and not a typo"
        );
    }

    // fleeing

    use crate::entities::position::Direction::*;

    /// An open field, so the ladder is never forced by geometry unless a test says so.
    fn a_field(x: std::ops::RangeInclusive<u16>, y: std::ops::RangeInclusive<u16>) -> GameMap {
        let mut map = GameMap::new();
        for x in x {
            for y in y.clone() {
                map.insert_tile(Position::new(x, y, 7), a_ground_tile());
            }
        }
        map
    }

    fn at(x: u16, y: u16) -> Position {
        Position::new(x, y, 7)
    }

    /// The creature is placed by hand rather than through the shared helper because the
    /// flee tests need one that actually has a threshold to be under.
    fn put_frightened_creature(map: &mut GameMap, x: u16, y: u16) -> AgentKey {
        let key = map
            .insert_agent(a_test_creature_that_flees("Rat", 10, (1, 2), 5), &at(x, y))
            .unwrap();
        map.get_agent_mut(key).unwrap().remove_life(6);
        key
    }

    fn wall(map: &mut GameMap, x: u16, y: u16) {
        map.insert_tile(at(x, y), MapTile::new());
    }

    fn escape_from(map: &GameMap, creature: AgentKey, target: &Position) -> Option<Direction> {
        let from = map.agent_position(creature).unwrap().clone();
        escape_step(map, creature, &from, target, &mut Rolls::new(7))
    }

    #[test]
    fn a_creature_steps_directly_away_from_its_target() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);

        assert_eq!(escape_from(&map, rat, &at(16, 10)), Some(West));
        assert_eq!(escape_from(&map, rat, &at(14, 10)), Some(East));
        assert_eq!(escape_from(&map, rat, &at(15, 11)), Some(North));
        assert_eq!(escape_from(&map, rat, &at(15, 9)), Some(South));
    }

    /// Rung two: with the way out blocked, give ground on the axis the target is not on.
    #[test]
    fn a_creature_with_its_back_to_a_wall_sidesteps() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 14, 10);

        assert!(
            matches!(escape_from(&map, rat, &at(16, 10)), Some(North | South)),
            "expected a sidestep along the free axis"
        );
    }

    /// Rung two prefers the side that also opens distance. The target is north and one
    /// tile east, so west is the sidestep that gains ground and east the one that loses
    /// it; both are walkable, and only the preference decides.
    #[test]
    fn the_sidestep_that_opens_distance_is_taken_first() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 15, 11);

        assert_eq!(escape_from(&map, rat, &at(16, 7)), Some(West));
        assert_eq!(escape_from(&map, rat, &at(14, 7)), Some(East));
    }

    /// Rung five, and the one that makes fleeing a behaviour rather than a stall: a
    /// creature with every retreat blocked charges the thing it is running from.
    #[test]
    fn a_cornered_creature_walks_into_its_target() {
        let mut map = a_field(5..=25, 10..=10);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 14, 10);

        assert_eq!(escape_from(&map, rat, &at(16, 10)), Some(East));
    }

    #[test]
    fn a_creature_with_nowhere_at_all_to_go_stays_put() {
        let mut map = a_field(15..=15, 10..=10);
        let rat = put_frightened_creature(&mut map, 15, 10);

        assert_eq!(escape_from(&map, rat, &at(16, 10)), None);
    }

    /// A target on a perfect diagonal has two cardinals equally away from it, and both
    /// are tried before the diagonal that splits them.
    #[test]
    fn a_diagonal_target_is_escaped_by_a_cardinal_first() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);

        // Target to the north-west, so south and east are the two cardinals away.
        for seed in 0..32 {
            let from = at(15, 10);
            let step = escape_step(&map, rat, &from, &at(14, 9), &mut Rolls::new(seed));
            assert!(
                matches!(step, Some(South | East)),
                "expected a cardinal, got {step:?}"
            );
        }
    }

    #[test]
    fn the_diagonal_is_taken_when_both_cardinals_are_blocked() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 15, 11);
        wall(&mut map, 16, 10);

        assert_eq!(escape_from(&map, rat, &at(14, 9)), Some(SouthEast));
    }

    /// The target is standing on the creature, so no direction is away from it.
    #[test]
    fn a_target_on_the_creatures_own_tile_gets_a_cardinal() {
        let [first, rest @ ..] = escape_ladder(0, 0);

        assert_eq!(first, [(0, -1), (0, 1), (1, 0), (-1, 0)]);
        assert!(rest.iter().all(|rung| rung == &NO_RUNG));
    }

    #[test]
    fn a_hurt_creature_flees_instead_of_fighting() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(16, 10))
            .unwrap();
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 1);
        let mut state = CreatureState::default();

        let action = decide_action(CreatureBehaviourContext {
            creature: rat,
            map: &map,
            roll: Rolls::new(7),
            world_tick: Tick(100),
            state: &mut state,
        });

        assert!(
            matches!(
                action,
                Some(CreatureAction::Walk { agent_key, direction: West }) if agent_key == rat
            ),
            "expected a step away from the player, got {action:?}"
        );
    }

    /// The same creature above its threshold chases instead, so the test above is turning
    /// on the health and not on something incidental to the fixture.
    #[test]
    fn the_same_creature_at_full_health_closes_in() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = map
            .insert_agent(
                a_test_creature_that_flees("Rat", 10, (1, 2), 5),
                &at(15, 10),
            )
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(20, 10))
            .unwrap();
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 1);
        let mut state = CreatureState::default();

        let action = decide_action(CreatureBehaviourContext {
            creature: rat,
            map: &map,
            roll: Rolls::new(7),
            world_tick: Tick(100),
            state: &mut state,
        });

        assert!(
            matches!(
                action,
                Some(CreatureAction::Walk {
                    direction: East,
                    ..
                })
            ),
            "expected a step towards the player, got {action:?}"
        );
    }

    fn an_attack_ability(chance: u32) -> CreatureAbility {
        CreatureAbility {
            id: CreatureAbilityId(0),
            cooldown: TickDelta(40),
            chance,
            effect: AbilityEffect::Attack(CreatureAttack {
                damage: CreatureAttackDamage {
                    element: CombatElement::Energy,
                    value: Bounds { min: 1, max: 2 },
                    condition: None,
                },
                target: SpellTargetMode::Target { range: 1 },
                effect_id: None,
                missile_id: None,
            }),
        }
    }

    /// A caster adjacent to its target, so `in_combat` walks nowhere and falls straight
    /// through to the ability roll.
    fn a_caster_in_melee(map: &mut GameMap, ability: CreatureAbility) -> AgentKey {
        let demon = map
            .insert_agent(
                Agent::from_creature_kind(
                    Arc::new(CreatureKind {
                        abilities: vec![ability],
                        ..a_creature_kind("Demon")
                    }),
                    at(15, 10),
                ),
                &at(15, 10),
            )
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(16, 10))
            .unwrap();
        map.get_agent_mut(demon)
            .unwrap()
            .set_target(Some(player), 1);
        demon
    }

    #[test]
    fn an_ability_that_wins_its_roll_is_cast() {
        let mut map = a_field(5..=25, 5..=15);
        let demon = a_caster_in_melee(&mut map, an_attack_ability(MAX_DROP_CHANCE));
        let mut state = CreatureState::default();

        let action = decide_action(CreatureBehaviourContext {
            creature: demon,
            map: &map,
            roll: Rolls::new(7),
            world_tick: Tick(100),
            state: &mut state,
        });

        assert!(
            matches!(action, Some(CreatureAction::CastAbility { ability_id, .. })
                if ability_id == CreatureAbilityId(0)),
            "expected a cast, got {action:?}"
        );
    }

    /// The bug this pins: the roll used to be retried every decision, which turned a low
    /// `chance` into a few hundred milliseconds of delay instead of a gate.
    #[test]
    fn a_lost_roll_puts_the_ability_back_on_cooldown() {
        let mut map = a_field(5..=25, 5..=15);
        let demon = a_caster_in_melee(&mut map, an_attack_ability(0));
        let mut state = CreatureState::default();

        let action = decide_action(CreatureBehaviourContext {
            creature: demon,
            map: &map,
            roll: Rolls::new(7),
            world_tick: Tick(100),
            state: &mut state,
        });

        assert!(
            !matches!(action, Some(CreatureAction::CastAbility { .. })),
            "a zero chance must never cast, got {action:?}"
        );
        assert_eq!(
            state.next_ability_tick(CreatureAbilityId(0)),
            Tick(100) + TickDelta(40)
        );
    }

    /// An ability the group cooldown blocks was never eligible, so it must keep its roll
    /// rather than burn one the way a lost roll does.
    #[test]
    fn a_group_cooldown_blocks_the_cast_without_spending_the_roll() {
        let mut map = a_field(5..=25, 5..=15);
        let demon = a_caster_in_melee(&mut map, an_attack_ability(MAX_DROP_CHANCE));
        map.get_agent_mut(demon).unwrap().stamp_spell_group(
            Tick(100),
            SpellGroup::Attack,
            Some(TickDelta(40)),
        );
        let mut state = CreatureState::default();

        let action = decide_action(CreatureBehaviourContext {
            creature: demon,
            map: &map,
            roll: Rolls::new(7),
            world_tick: Tick(120),
            state: &mut state,
        });

        assert!(
            !matches!(action, Some(CreatureAction::CastAbility { .. })),
            "the attack group is still on cooldown, got {action:?}"
        );
        assert_eq!(state.next_ability_tick(CreatureAbilityId(0)), Tick(0));
    }
}

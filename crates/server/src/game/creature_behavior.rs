use std::collections::HashSet;

use tracing::error;

use crate::constants::items::MAX_DROP_CHANCE;
use crate::constants::view::{AGENT_DESPAWN_RADIUS, SIGHT_RANGE};
use crate::entities::agent::{Agent, AgentKey, Facing};
use crate::entities::combat::CombatElement;
use crate::entities::conditions::ConditionAttack;
use crate::entities::creature::{
    AbilityEffect, CreatureAbility, CreatureAbilityId, CreatureAttack, CreatureKind,
};
use crate::entities::items::ItemFlag;
use crate::entities::map::GameMap;
use crate::entities::position::{Direction, Position, Rect};
use crate::entities::targeting::TargetMode;
use crate::game::config::GAME_CONFIG;
use crate::game::map_query::{can_throw, face_target, iter_visible_floors};
use crate::game::pathfinding::{self, Goal, TileEntry};
use crate::game::random::Rolls;
use crate::game::{Tick, TickDelta};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockedChase {
    target: AgentKey,
    since: Tick,
    provoked: bool,
}

#[derive(Debug, Default)]
pub struct CreatureState {
    next_wander_tick: Tick,
    next_say_tick: Tick,
    next_dance_tick: Tick,
    next_change_target_tick: Tick,
    ability_cooldowns: Vec<(CreatureAbilityId, Tick)>,
    chase: Option<BlockedChase>,
}

impl CreatureState {
    fn stamp_wander(&mut self, current_tick: Tick) {
        self.next_wander_tick = current_tick + GAME_CONFIG.movement.wander_ticks
    }

    fn stamp_dance(&mut self, current_tick: Tick) {
        self.next_dance_tick = current_tick + GAME_CONFIG.movement.dance_ticks
    }

    fn stamp_say(&mut self, current_tick: Tick, kind: &CreatureKind) {
        self.next_say_tick = current_tick + kind.say.cooldown;
    }

    fn stamp_ability(&mut self, current_tick: Tick, ability: &CreatureAbility) {
        self.ability_cooldowns.retain(|(id, _)| *id != ability.id);
        self.ability_cooldowns
            .push((ability.id, current_tick + ability.cooldown));
    }

    fn stamp_change_target(&mut self, current_tick: Tick, kind: &CreatureKind) {
        self.next_change_target_tick = current_tick + kind.change_target.interval;
    }

    fn ignores_fields(&self, target: AgentKey) -> bool {
        self.chase
            .is_some_and(|chase| chase.target == target && chase.provoked)
    }

    fn keep_chase_on(&mut self, target: Option<AgentKey>) {
        if self.chase.is_some_and(|chase| Some(chase.target) != target) {
            self.chase = None;
        }
    }

    fn provoke(&mut self, target: AgentKey, targets_last_hit: Option<Tick>) {
        if let Some(chase) = self.chase.as_mut()
            && chase.target == target
            && targets_last_hit.is_some_and(|hit| hit > chase.since)
        {
            chase.provoked = true;
        }
    }

    fn note_chase(&mut self, target: AgentKey, step: pathfinding::Step, tick: Tick) {
        let current = self.chase.filter(|chase| chase.target == target);
        self.chase = match step {
            pathfinding::Step::Arrived => None,
            pathfinding::Step::Move(_) => current.filter(|chase| chase.provoked),
            pathfinding::Step::Unreachable => Some(current.unwrap_or(BlockedChase {
                target,
                since: tick,
                provoked: false,
            })),
        };
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
    pub actions: &'a mut Vec<CreatureAction>,
}

#[derive(Debug)]
enum TargetSearchStrategy {
    Nearest,
    Damage,
    Random,
}

/// The creatures worth deciding for: those within `AGENT_DESPAWN_RADIUS` of a player on any
/// floor that player's client draws, plus any creature still holding a target, so a chase does
/// not stop the moment it leaves the box.
///
/// The box is the despawn radius rather than the drawn viewport, and it is applied unshifted to
/// every visible floor. A floor slides one tile per level ([`floor_viewport_rect`]), at most
/// seven on the surface, which the wider box absorbs — so this owes the renderer a floor range
/// and nothing else.
pub fn active_creatures(map: &GameMap) -> HashSet<AgentKey> {
    let mut active = HashSet::new();
    let mut players: Vec<Position> = Vec::new();
    for (key, agent) in map.iter_agents() {
        if agent.is_player() {
            if let Some(position) = map.agent_position(key) {
                players.push(position.clone());
            }
        } else if agent.is_creature() && agent.target().is_some() {
            active.insert(key);
        }
    }

    for position in players {
        let near = Rect::radius(&position, AGENT_DESPAWN_RADIUS);
        for floor in
            iter_visible_floors(position.z).chain([8_u8].into_iter().filter(|_| position.z == 7))
        {
            for (creature, _) in map.iter_agents_in_rect(&near, floor) {
                if map.get_agent(creature).is_some_and(Agent::is_creature) {
                    active.insert(creature);
                }
            }
        }
    }
    active
}

pub fn decide_action(ctx: CreatureBehaviourContext) {
    let target = ctx
        .map
        .get_agent(ctx.creature)
        .and_then(|agent| agent.target());
    ctx.state.keep_chase_on(target);
    act(get_state(&ctx), ctx)
}

// private

pub const WANDER_DIRECTIONS: [Direction; 4] = [
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

    if agent.target().is_some() && agent.is_fleeing(ctx.world_tick) {
        return CreatureBehavior::Fleeing;
    } else if agent.target().is_some() {
        return CreatureBehavior::InCombat;
    } else if agent.get_origin().distance(position) > GAME_CONFIG.movement.wander_distance {
        return CreatureBehavior::Returning;
    }

    CreatureBehavior::Idle
}

fn act(state: CreatureBehavior, ctx: CreatureBehaviourContext) {
    match state {
        CreatureBehavior::Idle => idle(ctx),
        CreatureBehavior::InCombat => in_combat(ctx),
        CreatureBehavior::Fleeing => fleeing(ctx),
        CreatureBehavior::Returning => returning(ctx),
    }
}

fn idle(mut ctx: CreatureBehaviourContext) {
    if let Some(target) = search_target(
        ctx.creature,
        ctx.map,
        TargetSearchStrategy::Nearest,
        ctx.world_tick,
        &mut ctx.roll,
    ) {
        let kind = ctx
            .map
            .get_agent(ctx.creature)
            .and_then(|a| a.get_creature_kind())
            .expect("Agent must be a creature");
        ctx.state.stamp_change_target(ctx.world_tick, kind);
        ctx.actions.push(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(target),
        });
        return;
    }

    if let Some(wander_dir) = wander(&mut ctx) {
        ctx.actions.push(CreatureAction::Walk {
            agent_key: ctx.creature,
            direction: wander_dir,
        });
    }

    if let Some(sentence) = say(&mut ctx) {
        ctx.actions.push(CreatureAction::Say {
            agent_key: ctx.creature,
            message: sentence,
        });
    }
}

fn in_combat(mut ctx: CreatureBehaviourContext) {
    let Some(agent) = ctx.map.get_agent(ctx.creature) else {
        return;
    };
    let Some(position) = ctx.map.agent_position(ctx.creature) else {
        return;
    };
    let Some(target_key) = agent.target() else {
        return;
    };
    let Some(target_position) = ctx.map.agent_position(target_key) else {
        return;
    };
    let target_distance = agent
        .target_distance(ctx.world_tick)
        .expect("Agent must be a creature");
    let challenged = agent.challenge(ctx.world_tick).is_some();

    if agent.next_walk_tick() <= ctx.world_tick {
        let kind = agent.get_creature_kind().expect("Agent must be creature");
        if kind.change_target.chance > 0
            && !challenged
            && let Some(agent_key) = body_blocked(ctx.map, ctx.creature)
        {
            ctx.state.stamp_change_target(ctx.world_tick, kind);
            ctx.actions.push(CreatureAction::SetTarget {
                agent_key: ctx.creature,
                target: Some(agent_key),
            });
        } else {
            ctx.state
                .provoke(target_key, agent.participation().last_hit(target_key));
            let step = approach(
                ctx.map,
                ctx.creature,
                position,
                target_position,
                target_distance,
                ctx.state.ignores_fields(target_key),
                &mut ctx.roll,
            );
            ctx.state.note_chase(target_key, step, ctx.world_tick);
            match step {
                pathfinding::Step::Move(direction) => {
                    ctx.actions.push(CreatureAction::Walk {
                        agent_key: ctx.creature,
                        direction,
                    });
                }
                pathfinding::Step::Arrived => {
                    if let Some(direction) = dance(&mut ctx, position, target_position) {
                        ctx.actions.push(CreatureAction::Walk {
                            agent_key: ctx.creature,
                            direction,
                        });
                    }
                }
                pathfinding::Step::Unreachable => {
                    if let Some(new_target) = search_target(
                        ctx.creature,
                        ctx.map,
                        TargetSearchStrategy::Nearest,
                        ctx.world_tick,
                        &mut ctx.roll,
                    ) {
                        if new_target != target_key {
                            ctx.state.stamp_change_target(
                                ctx.world_tick,
                                agent.get_creature_kind().expect("Agent must be a creature"),
                            );
                            ctx.actions.push(CreatureAction::SetTarget {
                                agent_key: ctx.creature,
                                target: Some(new_target),
                            });
                        } else if !can_throw(ctx.map, position, target_position, true) {
                            ctx.actions.push(CreatureAction::SetTarget {
                                agent_key: ctx.creature,
                                target: None,
                            });
                        }
                    } else {
                        ctx.actions.push(CreatureAction::SetTarget {
                            agent_key: ctx.creature,
                            target: None,
                        });
                    }
                }
            }
        }
    }

    if let Some(new_target) = change_target(&mut ctx) {
        ctx.actions.push(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(new_target),
        });
    }

    if agent.is_hostile()
        && let Some(ability) = use_ability(
            ctx.creature,
            agent,
            ctx.state,
            ctx.world_tick,
            &mut ctx.roll,
        )
    {
        ctx.actions.push(ability);
    }

    if let Some(sentence) = say(&mut ctx) {
        ctx.actions.push(CreatureAction::Say {
            agent_key: ctx.creature,
            message: sentence,
        });
    }
}

fn fleeing(mut ctx: CreatureBehaviourContext) {
    let map = ctx.map;
    let Some(agent) = map.get_agent(ctx.creature) else {
        return;
    };
    let Some(from) = map.agent_position(ctx.creature) else {
        return;
    };
    let Some(target) = agent.target() else {
        return;
    };
    let Some(to) = map.agent_position(target) else {
        return;
    };

    if agent.next_walk_tick() <= ctx.world_tick
        && let Some(direction) = escape_step(map, ctx.creature, from, to, true, &mut ctx.roll)
    {
        ctx.actions.push(CreatureAction::Walk {
            agent_key: ctx.creature,
            direction,
        });
    }

    if let Some(agent_key) = search_target(
        ctx.creature,
        map,
        TargetSearchStrategy::Nearest,
        ctx.world_tick,
        &mut ctx.roll,
    ) && Some(agent_key) != agent.target()
    {
        ctx.actions.push(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(agent_key),
        });
    }

    if let Some(ability) = use_ability(
        ctx.creature,
        agent,
        ctx.state,
        ctx.world_tick,
        &mut ctx.roll,
    ) {
        ctx.actions.push(ability);
    }
}

fn returning(mut ctx: CreatureBehaviourContext) {
    let Some(agent) = ctx.map.get_agent(ctx.creature) else {
        return;
    };
    let Some(position) = ctx.map.agent_position(ctx.creature) else {
        return;
    };

    if let Some(target) = search_target(
        ctx.creature,
        ctx.map,
        TargetSearchStrategy::Nearest,
        ctx.world_tick,
        &mut ctx.roll,
    ) {
        let kind = agent.get_creature_kind().expect("Agent must be a creature");
        ctx.state.stamp_change_target(ctx.world_tick, kind);
        ctx.actions.push(CreatureAction::SetTarget {
            agent_key: ctx.creature,
            target: Some(target),
        });
        return;
    }

    match pathfinding::next_step(
        ctx.map,
        ctx.creature,
        &Goal::within_bounds(
            agent.get_origin().clone(),
            GAME_CONFIG.movement.wander_distance,
        ),
        &Rect::radius(position, SIGHT_RANGE),
        |pos| field_entry(ctx.map, ctx.creature, pos, false),
    ) {
        pathfinding::Step::Arrived => {}
        pathfinding::Step::Move(direction) => {
            ctx.actions.push(CreatureAction::Walk {
                agent_key: ctx.creature,
                direction,
            });
        }
        pathfinding::Step::Unreachable => idle(ctx),
    }
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
                will_enter(ctx.map, ctx.creature, &pos)
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

fn random(roll: &mut Rolls, candidates: Vec<(AgentKey, Position)>) -> Option<AgentKey> {
    roll.category_roll(&candidates).map(|(key, _)| *key)
}

fn search_target(
    creature: AgentKey,
    map: &GameMap,
    strategy: TargetSearchStrategy,
    tick: Tick,
    roll: &mut Rolls,
) -> Option<AgentKey> {
    let from = map.agent_position(creature)?;
    let viewport = Rect::radius(from, SIGHT_RANGE);
    let candidates: Vec<(AgentKey, Position)> = map
        .iter_agents_in_rect(&viewport, from.z)
        .filter(|(key, _)| map.get_agent(*key).is_some_and(Agent::is_player))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    match strategy {
        TargetSearchStrategy::Random => {
            let reachable = pathfinding::reachable_from(map, creature, &viewport, |pos| {
                field_entry(map, creature, pos, false)
            });
            let range = map.get_agent(creature)?.target_distance(tick)?;
            let reachable_candidates: Vec<(AgentKey, Position)> = candidates
                .iter()
                .filter(|(_, pos)| {
                    reachable
                        .cost_to(
                            map,
                            &Goal::InSight {
                                of: pos.clone(),
                                range,
                            },
                        )
                        .is_some()
                })
                .cloned()
                .collect();
            random(roll, reachable_candidates)
        }
        TargetSearchStrategy::Damage => {
            let agent = map.get_agent(creature)?;
            let weighted_candidates: Vec<(AgentKey, u64)> = agent
                .participation()
                .participants()
                .iter()
                .filter(|(part_key, ..)| {
                    candidates.iter().any(|(cand_key, _)| part_key == cand_key)
                })
                .map(|(key, damage, _)| (*key, *damage))
                .collect();
            if !weighted_candidates.is_empty() {
                let weights: Vec<u64> = weighted_candidates.iter().map(|(_, w)| *w).collect();
                let index = roll.weighted(&weights);
                if let Ok(index) = index {
                    return weighted_candidates.get(index).map(|(key, _)| *key);
                }
            }
            random(roll, candidates)
        }
        TargetSearchStrategy::Nearest => {
            let reachable = pathfinding::reachable_from(map, creature, &viewport, |pos| {
                field_entry(map, creature, pos, false)
            });
            let range = map.get_agent(creature)?.target_distance(tick)?;
            candidates
                .iter()
                .filter_map(|(key, pos)| {
                    let ticks = reachable.cost_to(
                        map,
                        &Goal::InSight {
                            of: pos.clone(),
                            range,
                        },
                    )?;
                    Some((*key, ticks, from.distance(pos)))
                })
                .min_by_key(|(_, ticks, distance)| (*ticks, *distance))
                .map(|(key, ..)| key)
                .or_else(|| {
                    candidates
                        .iter()
                        .filter(|(_, pos)| can_throw(map, from, pos, true))
                        .min_by_key(|(_, pos)| from.distance(pos))
                        .map(|(key, _)| *key)
                })
        }
    }
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

fn approach(
    map: &GameMap,
    creature: AgentKey,
    from: &Position,
    target: &Position,
    target_distance: u16,
    ignores_fields: bool,
    roll: &mut Rolls,
) -> pathfinding::Step {
    let bounds = Rect::radius(from, SIGHT_RANGE);
    if target_distance <= 1 {
        return pathfinding::next_step(
            map,
            creature,
            &Goal::adjacent(target.clone()),
            &bounds,
            |pos| field_entry(map, creature, pos, ignores_fields),
        );
    }

    if from.distance(target) < target_distance && can_throw(map, from, target, true) {
        return escape_step(map, creature, from, target, false, roll)
            .map_or(pathfinding::Step::Arrived, pathfinding::Step::Move);
    }

    pathfinding::next_step(
        map,
        creature,
        &Goal::InSight {
            of: target.clone(),
            range: target_distance,
        },
        &bounds,
        |pos| field_entry(map, creature, pos, ignores_fields),
    )
}

fn dance(
    ctx: &mut CreatureBehaviourContext,
    from: &Position,
    target: &Position,
) -> Option<Direction> {
    if ctx.state.next_dance_tick > ctx.world_tick {
        return None;
    }
    ctx.state.stamp_dance(ctx.world_tick);
    let dance_chance = ctx
        .map
        .get_agent(ctx.creature)?
        .get_creature_kind()?
        .dance_chance;
    if !ctx
        .roll
        .chance(u32::from(dance_chance) * MAX_DROP_CHANCE / 100)
    {
        return None;
    }
    dance_step(
        ctx.map,
        ctx.creature,
        from,
        target,
        ctx.world_tick,
        &mut ctx.roll,
    )
}

fn change_target(ctx: &mut CreatureBehaviourContext) -> Option<AgentKey> {
    if ctx
        .map
        .get_agent(ctx.creature)
        .is_some_and(|agent| agent.challenge(ctx.world_tick).is_some())
    {
        return None;
    }
    if ctx.state.next_change_target_tick > ctx.world_tick {
        return None;
    }
    let kind = ctx
        .map
        .get_agent(ctx.creature)
        .and_then(|a| a.get_creature_kind())?;

    ctx.state.stamp_change_target(ctx.world_tick, kind);
    if kind.change_target.interval == TickDelta(0)
        || ctx.roll.uniform(1, 100) > kind.change_target.chance
    {
        return None;
    }

    let strategy = if ctx
        .roll
        .chance(GAME_CONFIG.combat.change_target_random_chance * MAX_DROP_CHANCE / 100)
    {
        TargetSearchStrategy::Random
    } else {
        TargetSearchStrategy::Damage
    };

    let new_target = search_target(
        ctx.creature,
        ctx.map,
        strategy,
        ctx.world_tick,
        &mut ctx.roll,
    );

    if let Some(new_target) = new_target
        && ctx.map.get_agent(ctx.creature).and_then(|a| a.target()) == Some(new_target)
    {
        return None;
    }

    new_target
}

fn body_blocked(map: &GameMap, creature: AgentKey) -> Option<AgentKey> {
    let agent = map.get_agent(creature)?;
    let pos = map.agent_position(creature)?;
    let target = agent.target()?;
    let target_pos = map.agent_position(target)?;

    if pos.distance(target_pos) <= 1 {
        return None;
    }

    let new_pos = match face_target(agent.facing(), pos, target_pos) {
        Facing::North => pos.checked_offset(0, -1)?,
        Facing::East => pos.checked_offset(1, 0)?,
        Facing::South => pos.checked_offset(0, 1)?,
        Facing::West => pos.checked_offset(-1, 0)?,
    };

    map.iter_agents_at(&new_pos)
        .ok()?
        .filter(|key| map.get_agent(**key).is_some_and(|a| a.is_player()))
        .last()
        .copied()
}

/// One rung of the escape ladder: up to four candidate steps as `(dx, dy)` offsets.
type Rung = [(i32, i32); 4];

const NO_STEP: (i32, i32) = (0, 0);
const NO_RUNG: Rung = [NO_STEP; 4];

/// The `escape_ladder` rungs that give ground to the target; only a fleeing creature takes them.
const FLEE_ONLY_RUNGS: [usize; 2] = [2, 4];

fn escape_step(
    map: &GameMap,
    creature: AgentKey,
    from: &Position,
    to: &Position,
    flee: bool,
    roll: &mut Rolls,
) -> Option<Direction> {
    let offset_x = from.x as i32 - to.x as i32;
    let offset_y = from.y as i32 - to.y as i32;

    for (index, rung) in escape_ladder(offset_x, offset_y).into_iter().enumerate() {
        if !flee && FLEE_ONLY_RUNGS.contains(&index) {
            continue;
        }
        let options: Vec<Direction> = rung
            .into_iter()
            .filter_map(|(dx, dy)| Direction::from_step(dx, dy))
            .filter(|direction| {
                let pos = from.clone() + *direction;
                will_enter(map, creature, &pos)
            })
            .collect();
        if let Some(direction) = roll.category_roll(&options) {
            return Some(*direction);
        }
    }

    None
}

fn dance_step(
    map: &GameMap,
    creature: AgentKey,
    from: &Position,
    target: &Position,
    tick: Tick,
    roll: &mut Rolls,
) -> Option<Direction> {
    let agent = map.get_agent(creature)?;
    let distance = from.distance(target);
    if distance < agent.target_distance(tick)? {
        return None;
    }
    let attacking = can_attack_from(map, agent, from, target);
    let offset_x = from.x as i32 - target.x as i32;
    let offset_y = from.y as i32 - target.y as i32;

    let options: Vec<Direction> = [
        (Direction::North, offset_y >= 0),
        (Direction::South, offset_y <= 0),
        (Direction::East, offset_x <= 0),
        (Direction::West, offset_x >= 0),
    ]
    .into_iter()
    .filter(|(_, toward_target)| *toward_target)
    .map(|(direction, _)| direction)
    .filter(|direction| {
        let pos = from.clone() + *direction;
        pos.distance(target) == distance
            && will_enter(map, creature, &pos)
            && (!attacking || can_attack_from(map, agent, &pos, target))
    })
    .collect();
    roll.category_roll(&options).copied()
}

fn can_attack_from(map: &GameMap, agent: &Agent, from: &Position, target: &Position) -> bool {
    if !agent.is_hostile() {
        return true;
    }
    let Some(kind) = agent.get_creature_kind() else {
        return false;
    };
    let distance = from.distance(target);
    let in_range = distance <= 1
        || kind.abilities.iter().any(|ability| match &ability.effect {
            AbilityEffect::Attack(CreatureAttack {
                target: TargetMode::Target { range },
                ..
            })
            | AbilityEffect::Condition(ConditionAttack {
                target: TargetMode::Target { range },
                ..
            }) => distance <= *range,
            _ => false,
        });
    in_range && can_throw(map, from, target, true)
}

fn field_entry(map: &GameMap, walker: AgentKey, pos: &Position, ignores_fields: bool) -> TileEntry {
    let Ok(items) = map.iter_items(pos) else {
        return TileEntry::Free;
    };
    items
        .map(|item| match item.config.attr_field() {
            Some(spec) if spec.deals_damage() => {
                field_hazard(map, walker, spec.element, ignores_fields)
            }
            _ if item.config.has_flag(ItemFlag::Avoid) => TileEntry::Refused,
            _ => TileEntry::Free,
        })
        .max()
        .unwrap_or(TileEntry::Free)
}

fn field_hazard(
    map: &GameMap,
    walker: AgentKey,
    element: CombatElement,
    ignores_fields: bool,
) -> TileEntry {
    let Some(agent) = map.get_agent(walker) else {
        return TileEntry::Refused;
    };
    let unharmed = agent
        .get_creature_kind()
        .is_some_and(|kind| kind.damage_modifier(element) == 0.0 || kind.walks_on(element));
    if unharmed {
        TileEntry::Free
    } else if !ignores_fields {
        TileEntry::Refused
    } else if agent.conditions().has_damage_over_time(element) {
        TileEntry::Free
    } else {
        TileEntry::Harmful
    }
}

pub fn will_enter(map: &GameMap, creature: AgentKey, pos: &Position) -> bool {
    map.can_move(pos, creature) && field_entry(map, creature, pos, false) == TileEntry::Free
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

    #[test]
    fn an_npc_talking_to_a_player_is_not_an_active_creature() {
        use crate::entities::{map::GameMap, position::Position};
        use crate::persistence::test_fixtures::{a_test_snapshot, an_npc};

        let here = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        let mut npc = Agent::from_npc(an_npc());
        npc.set_target(Some(player), 0);
        let npc = map.insert_agent(npc, &here).unwrap();

        assert!(!active_creatures(&map).contains(&npc));
    }
    use crate::entities::Bounds;
    use crate::entities::agent::{Agent, Challenge, Pool};
    use crate::entities::combat::CombatElement;
    use crate::entities::conditions::{ConditionSpec, DamageOverTime, SpecSchedule};
    use crate::entities::creature::{AbilityEffect, ChangeTarget, CreatureAbility, CreatureAttack};
    use crate::entities::creature::{CreatureAttackDamage, CreatureFlag, CreatureKind};
    use crate::entities::items::ItemId;
    use crate::entities::items::{Item, ItemAttribute, ItemConfig, ItemFlag};
    use crate::entities::map::MapTile;
    use crate::entities::spells::SpellGroup;
    use crate::entities::targeting::TargetMode;
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
            an_item(3, HashSet::from([ItemFlag::Unpass, ItemFlag::Unsight])),
        )
        .unwrap();
    }

    fn a_field_item(element: CombatElement, damage: u32, flags: HashSet<ItemFlag>) -> Item {
        an_item_with(
            4,
            flags,
            vec![ItemAttribute::Field(Arc::new(ConditionSpec {
                element,
                interval: TickDelta(20),
                schedule: SpecSchedule::Flat {
                    damage: Bounds {
                        min: damage,
                        max: damage,
                    },
                    count: 3,
                },
                delayed: false,
            }))],
        )
    }

    fn lay(map: &mut GameMap, x: u16, item: Item) {
        map.place_item(&Position::new(x, 10, 7), None, None, item)
            .unwrap();
    }

    fn burn(map: &mut GameMap, x: u16) {
        lay(
            map,
            x,
            a_field_item(CombatElement::Fire, 10, HashSet::new()),
        );
    }

    fn put_kind(map: &mut GameMap, x: u16, kind: CreatureKind) -> AgentKey {
        let at = Position::new(x, 10, 7);
        map.insert_agent(Agent::from_creature_kind(Arc::new(kind), at.clone()), &at)
            .unwrap()
    }

    fn entry_at(map: &GameMap, walker: AgentKey, x: u16, ignores_fields: bool) -> TileEntry {
        field_entry(map, walker, &Position::new(x, 10, 7), ignores_fields)
    }

    fn wander_from(map: &GameMap, creature: AgentKey, seed: u64) -> Option<Direction> {
        let mut state = CreatureState::default();
        wander(&mut CreatureBehaviourContext {
            creature,
            map,
            roll: Rolls::new(seed),
            world_tick: Tick(100),
            state: &mut state,
            actions: &mut Vec::new(),
        })
    }

    fn decide_at(
        map: &GameMap,
        creature: AgentKey,
        state: &mut CreatureState,
        tick: Tick,
        seed: u64,
    ) -> Vec<CreatureAction> {
        let mut actions = Vec::new();
        decide_action(CreatureBehaviourContext {
            creature,
            map,
            roll: Rolls::new(seed),
            world_tick: tick,
            state,
            actions: &mut actions,
        });
        actions
    }

    fn is_walk(actions: &[CreatureAction]) -> bool {
        actions
            .iter()
            .any(|action| matches!(action, CreatureAction::Walk { .. }))
    }

    fn is_cast(actions: &[CreatureAction]) -> bool {
        actions
            .iter()
            .any(|action| matches!(action, CreatureAction::CastAbility { .. }))
    }

    fn is_set_target(actions: &[CreatureAction]) -> bool {
        actions
            .iter()
            .any(|action| matches!(action, CreatureAction::SetTarget { .. }))
    }

    /// The target the world is left holding: it applies a decision's actions in order.
    fn new_target(actions: &[CreatureAction]) -> Option<AgentKey> {
        actions
            .iter()
            .rev()
            .find_map(|action| match action {
                CreatureAction::SetTarget { target, .. } => Some(*target),
                _ => None,
            })
            .flatten()
    }

    fn nearest_target(creature: AgentKey, map: &GameMap) -> Option<AgentKey> {
        search_target(
            creature,
            map,
            TargetSearchStrategy::Nearest,
            Tick(0),
            &mut Rolls::new(0),
        )
    }

    #[test]
    fn a_creature_wanders_onto_open_ground() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);

        assert!((0..64).any(|seed| wander_from(&map, rat, seed).is_some()));
    }

    #[test]
    fn a_creature_never_wanders_onto_a_tile_it_avoids() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);
        block_walking(&mut map, 14);
        block_walking(&mut map, 16);

        assert!((0..64).all(|seed| wander_from(&map, rat, seed).is_none()));
    }

    #[test]
    fn a_damaging_field_is_refused_by_a_creature_that_has_not_been_provoked() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);
        burn(&mut map, 16);

        assert_eq!(entry_at(&map, rat, 16, false), TileEntry::Refused);
    }

    #[test]
    fn a_provoked_creature_finds_a_damaging_field_harmful() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);
        burn(&mut map, 16);

        assert_eq!(entry_at(&map, rat, 16, true), TileEntry::Harmful);
    }

    #[test]
    fn a_provoked_creature_already_burning_crosses_fire_freely() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);
        burn(&mut map, 16);
        map.get_agent_mut(rat)
            .unwrap()
            .conditions_mut()
            .apply_damage_over_time(DamageOverTime::flat(
                CombatElement::Fire,
                None,
                TickDelta(20),
                10,
                3,
            ));

        assert_eq!(entry_at(&map, rat, 16, true), TileEntry::Free);
        assert_eq!(entry_at(&map, rat, 16, false), TileEntry::Refused);
    }

    #[test]
    fn a_creature_that_walks_on_fire_enters_a_fire_field() {
        let mut map = a_corridor(14..=16);
        let walker = put_kind(
            &mut map,
            15,
            CreatureKind {
                flags: vec![CreatureFlag::WalkOnFire],
                ..a_creature_kind("Imp")
            },
        );
        burn(&mut map, 16);

        assert_eq!(entry_at(&map, walker, 16, false), TileEntry::Free);
    }

    #[test]
    fn a_creature_immune_to_fire_enters_a_fire_field() {
        let mut map = a_corridor(14..=16);
        let elemental = put_kind(
            &mut map,
            15,
            CreatureKind {
                elemental_modifiers: vec![(CombatElement::Fire, 0.0)],
                ..a_creature_kind("Fire Elemental")
            },
        );
        burn(&mut map, 16);

        assert_eq!(entry_at(&map, elemental, 16, false), TileEntry::Free);
    }

    #[test]
    fn a_damaging_field_is_judged_by_its_element_even_when_flagged_avoid() {
        let mut map = a_corridor(14..=16);
        let walker = put_kind(
            &mut map,
            15,
            CreatureKind {
                flags: vec![CreatureFlag::WalkOnEnergy],
                ..a_creature_kind("Spark")
            },
        );
        lay(
            &mut map,
            16,
            a_field_item(CombatElement::Energy, 10, HashSet::from([ItemFlag::Avoid])),
        );

        assert_eq!(entry_at(&map, walker, 16, false), TileEntry::Free);
    }

    #[test]
    fn a_harmless_field_falls_back_to_the_avoid_flag() {
        let mut map = a_corridor(14..=17);
        let rat = put_creature(&mut map, 15);
        lay(
            &mut map,
            16,
            a_field_item(CombatElement::Fire, 0, HashSet::new()),
        );
        lay(
            &mut map,
            17,
            a_field_item(CombatElement::Fire, 0, HashSet::from([ItemFlag::Avoid])),
        );

        assert_eq!(entry_at(&map, rat, 16, false), TileEntry::Free);
        assert_eq!(entry_at(&map, rat, 17, false), TileEntry::Refused);
    }

    #[test]
    fn a_creature_never_wanders_onto_a_damaging_field() {
        let mut map = a_corridor(14..=16);
        let rat = put_creature(&mut map, 15);
        burn(&mut map, 14);
        burn(&mut map, 16);

        assert!((0..64).all(|seed| wander_from(&map, rat, seed).is_none()));
    }

    fn a_key(n: u64) -> AgentKey {
        AgentKey::from(slotmap::KeyData::from_ffi((1 << 32) | n))
    }

    fn provoked_against(target: AgentKey) -> CreatureState {
        let mut state = CreatureState::default();
        state.note_chase(target, pathfinding::Step::Unreachable, Tick(100));
        state.provoke(target, Some(Tick(101)));
        assert!(state.ignores_fields(target));
        state
    }

    #[test]
    fn a_path_found_before_any_hit_forgets_when_the_block_began() {
        let mage = a_key(1);
        let mut state = CreatureState::default();
        state.note_chase(mage, pathfinding::Step::Unreachable, Tick(100));
        state.note_chase(mage, pathfinding::Step::Move(Direction::East), Tick(102));
        state.note_chase(mage, pathfinding::Step::Unreachable, Tick(110));

        state.provoke(mage, Some(Tick(105)));

        assert!(!state.ignores_fields(mage));
    }

    #[test]
    fn a_provoked_creature_keeps_ignoring_fields_while_it_closes_in() {
        let mage = a_key(1);
        let mut state = provoked_against(mage);

        state.note_chase(mage, pathfinding::Step::Move(Direction::East), Tick(104));

        assert!(state.ignores_fields(mage));
    }

    #[test]
    fn arriving_ends_the_provocation() {
        let mage = a_key(1);
        let mut state = provoked_against(mage);

        state.note_chase(mage, pathfinding::Step::Arrived, Tick(110));

        assert!(!state.ignores_fields(mage));
    }

    #[test]
    fn a_target_change_ends_the_provocation() {
        let (mage, knight) = (a_key(1), a_key(2));
        let mut state = provoked_against(mage);

        state.keep_chase_on(Some(knight));
        state.keep_chase_on(Some(mage));

        assert!(!state.ignores_fields(mage));
    }

    #[test]
    fn losing_the_target_ends_the_provocation() {
        let mage = a_key(1);
        let mut state = provoked_against(mage);

        state.keep_chase_on(None);

        assert!(!state.ignores_fields(mage));
    }

    /// A rat at 15 chasing a mage at 18 down a corridor with fire at 16, so the field is the
    /// only way. Fire does not block sight, so the rat keeps the mage as its target.
    fn a_walled_off_chase() -> (GameMap, AgentKey, AgentKey) {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let mage = put_player(&mut map, 18, 1);
        burn(&mut map, 16);
        map.get_agent_mut(rat).unwrap().set_target(Some(mage), 1);
        (map, rat, mage)
    }

    fn steps_east(actions: &[CreatureAction]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                CreatureAction::Walk {
                    direction: Direction::East,
                    ..
                }
            )
        })
    }

    #[test]
    fn a_walled_off_creature_waits_at_the_field() {
        let (map, rat, _) = a_walled_off_chase();
        let mut state = CreatureState::default();

        assert!(!steps_east(&decide_at(&map, rat, &mut state, Tick(100), 7)));
        assert!(!steps_east(&decide_at(&map, rat, &mut state, Tick(102), 7)));
    }

    #[test]
    fn a_creature_hit_by_its_target_while_walled_off_crosses() {
        let (mut map, rat, mage) = a_walled_off_chase();
        let mut state = CreatureState::default();
        decide_at(&map, rat, &mut state, Tick(100), 7);

        map.get_agent_mut(rat)
            .unwrap()
            .record_damage(mage, 5, Tick(101));

        assert!(steps_east(&decide_at(&map, rat, &mut state, Tick(102), 7)));
    }

    #[test]
    fn a_hit_taken_before_the_mage_stepped_behind_the_field_does_not_count() {
        let (mut map, rat, mage) = a_walled_off_chase();
        map.get_agent_mut(rat)
            .unwrap()
            .record_damage(mage, 5, Tick(90));
        let mut state = CreatureState::default();

        decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!steps_east(&decide_at(&map, rat, &mut state, Tick(102), 7)));
    }

    #[test]
    fn a_hit_from_someone_other_than_its_target_does_not_send_it_through() {
        let (mut map, rat, _) = a_walled_off_chase();
        let knight = put_player(&mut map, 5, 2);
        let mut state = CreatureState::default();
        decide_at(&map, rat, &mut state, Tick(100), 7);

        map.get_agent_mut(rat)
            .unwrap()
            .record_damage(knight, 5, Tick(101));

        assert!(!steps_east(&decide_at(&map, rat, &mut state, Tick(102), 7)));
    }

    #[test]
    fn a_creature_that_walks_on_fire_crosses_without_being_hit() {
        let mut map = a_corridor(5..=25);
        let imp = put_kind(
            &mut map,
            15,
            CreatureKind {
                flags: vec![CreatureFlag::WalkOnFire],
                ..a_creature_kind("Imp")
            },
        );
        let mage = put_player(&mut map, 18, 1);
        burn(&mut map, 16);
        map.get_agent_mut(imp).unwrap().set_target(Some(mage), 1);

        assert!(steps_east(&decide_at(
            &map,
            imp,
            &mut CreatureState::default(),
            Tick(100),
            7
        )));
    }

    #[test]
    fn an_empty_view_yields_no_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);

        assert_eq!(nearest_target(rat, &map), None);
    }

    #[test]
    fn other_creatures_are_not_targets() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_creature(&mut map, 16);
        put_creature(&mut map, 12);

        assert_eq!(nearest_target(rat, &map), None);
    }

    #[test]
    fn the_nearest_player_wins_when_both_can_be_walked_to() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let far = put_player(&mut map, 10, 1);
        let near = put_player(&mut map, 18, 2);

        let found = nearest_target(rat, &map);

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

        assert_eq!(nearest_target(rat, &map), Some(walkable));
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

        assert_eq!(nearest_target(rat, &map), Some(near));
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

        assert_eq!(nearest_target(rat, &map), Some(diagonal));
    }

    #[test]
    fn a_player_behind_a_wall_is_not_a_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 18, 1);
        block_sight(&mut map, 17);

        assert_eq!(nearest_target(rat, &map), None);
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

        assert_eq!(nearest_target(rat, &map), Some(visible));
    }

    /// Sight reaches 8 tiles, so x = 24 is one past it from x = 15.
    #[test]
    fn a_player_out_of_sight_is_not_a_target() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        put_player(&mut map, 24, 1);

        assert_eq!(nearest_target(rat, &map), None);
        assert!(
            Rect::radius(&Position::new(15, 10, 7), SIGHT_RANGE)
                .contains(&Position::new(23, 10, 7)),
            "one tile nearer is inside, so the case is testing the edge and not a typo"
        );
    }

    /// The term the inversion removes, measured against the term it adds, on the shipped map.
    #[test]
    #[ignore = "timing, not a pass/fail assertion"]
    fn target_search_cost_on_the_shipped_map() {
        const PLAYERS: usize = 5;
        let items = crate::persistence::items::load_items(
            "assets/items",
            &crate::persistence::areas::AREA_SHAPES,
        )
        .expect("items load");
        let mut map =
            crate::persistence::map::load_map("assets/map1.otbm", &items).expect("map loads");
        let spawns =
            crate::persistence::spawns::load_spawns("assets/spawns.yaml").expect("spawns load");

        let mut creatures: Vec<(AgentKey, Position)> = Vec::new();
        for spawn in &spawns {
            let Some(kind) = crate::persistence::creatures::CREATURE_KINDS
                .get(&spawn.kind)
                .cloned()
            else {
                continue;
            };
            let agent = Agent::respawning(kind, spawn.position.clone(), spawn.respawn_ticks);
            if let Ok(key) = map.insert_agent(agent, &spawn.position) {
                creatures.push((key, spawn.position.clone()));
            }
        }
        for (_, pos) in creatures
            .iter()
            .step_by(creatures.len() / PLAYERS)
            .take(PLAYERS)
            .cloned()
            .collect::<Vec<_>>()
        {
            let _ = map.insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos);
        }
        println!("{} creatures, {PLAYERS} players", creatures.len());

        let start = std::time::Instant::now();
        let mut seen = 0usize;
        for (_, pos) in &creatures {
            seen += map
                .iter_agents_in_rect(&Rect::radius(pos, SIGHT_RANGE), pos.z)
                .filter(|(key, _)| map.get_agent(*key).is_some_and(Agent::is_player))
                .count();
        }
        std::hint::black_box(seen);
        println!(
            "ungated sweep    : {:>9.1?}  (every creature searches)",
            start.elapsed()
        );

        let start = std::time::Instant::now();
        let active = active_creatures(&map);
        println!(
            "active_creatures : {:>9.1?}  ({} of {} creatures)",
            start.elapsed(),
            active.len(),
            creatures.len()
        );

        let mut states: std::collections::HashMap<AgentKey, CreatureState> =
            std::collections::HashMap::new();
        let mut actions = Vec::new();
        let start = std::time::Instant::now();
        for key in &active {
            let state = states.entry(*key).or_default();
            decide_action(CreatureBehaviourContext {
                creature: *key,
                map: &map,
                roll: Rolls::new(1),
                world_tick: Tick(100),
                state,
                actions: &mut actions,
            });
        }
        std::hint::black_box(actions);
        println!("gated decide pass: {:>9.1?}", start.elapsed());
    }

    /// The mountain case: the client draws the floor above, so a creature standing on it has to
    /// keep moving — but it is still not something to attack from down here.
    #[test]
    fn a_creature_one_floor_up_is_active_but_not_a_target() {
        let mut map = a_field(5..=25, 5..=15);
        for x in 5..=25u16 {
            for y in 5..=15u16 {
                map.insert_tile(Position::new(x, y, 6), a_ground_tile());
            }
        }
        let upstairs = map
            .insert_agent(
                a_test_creature("Rat", 10, (1, 2)),
                &Position::new(15, 10, 6),
            )
            .unwrap();
        map.insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(15, 10))
            .unwrap();

        assert!(
            active_creatures(&map).contains(&upstairs),
            "a creature the player can see must keep deciding"
        );
        assert_eq!(nearest_target(upstairs, &map), None);
    }

    /// Without this a creature stops mid-chase the moment it leaves the box, which reads as the
    /// same freeze as the floor bug and is not one.
    #[test]
    fn a_creature_holding_a_target_stays_active_with_no_player_near() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15);
        let far_away = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(25, 15))
            .unwrap();
        map.get_agent_mut(rat)
            .unwrap()
            .set_target(Some(far_away), 1);
        map.remove_agent(far_away);

        assert!(active_creatures(&map).contains(&rat));
    }

    #[test]
    fn a_creature_with_no_player_anywhere_is_not_active() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_creature(&mut map, 15);

        assert!(!active_creatures(&map).contains(&rat));
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

    fn avoided(map: &mut GameMap, x: u16, y: u16) {
        map.place_item(
            &at(x, y),
            None,
            None,
            an_item(2, HashSet::from([ItemFlag::Avoid])),
        )
        .unwrap();
    }

    fn escape_from(map: &GameMap, creature: AgentKey, target: &Position) -> Option<Direction> {
        let from = map.agent_position(creature).unwrap().clone();
        escape_step(map, creature, &from, target, true, &mut Rolls::new(7))
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

    #[test]
    fn a_creature_does_not_flee_onto_a_tile_it_avoids() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        avoided(&mut map, 14, 10);

        assert!(
            matches!(escape_from(&map, rat, &at(16, 10)), Some(North | South)),
            "expected a sidestep past the avoided tile"
        );
    }

    #[test]
    fn a_creature_hemmed_in_by_tiles_it_avoids_stays_put() {
        let mut map = a_field(14..=15, 10..=10);
        let rat = put_frightened_creature(&mut map, 15, 10);
        avoided(&mut map, 14, 10);

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
            let step = escape_step(&map, rat, &from, &at(14, 9), true, &mut Rolls::new(seed));
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

    fn keep_distance_from(
        map: &GameMap,
        creature: AgentKey,
        target: &Position,
    ) -> Option<Direction> {
        let from = map.agent_position(creature).unwrap().clone();
        escape_step(map, creature, &from, target, false, &mut Rolls::new(7))
    }

    #[test]
    fn a_creature_keeping_its_distance_never_walks_into_its_target() {
        let mut map = a_field(5..=25, 10..=10);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 14, 10);

        assert_eq!(keep_distance_from(&map, rat, &at(16, 10)), None);
        assert_eq!(escape_from(&map, rat, &at(16, 10)), Some(East));
    }

    /// The target is north and one tile east, with south and west walled: a fleeing creature
    /// gives ground eastward, one keeping its distance takes a diagonal away.
    #[test]
    fn a_creature_keeping_its_distance_does_not_give_ground_sideways() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 15, 11);
        wall(&mut map, 14, 10);

        for seed in 0..32 {
            let step = escape_step(
                &map,
                rat,
                &at(15, 10),
                &at(16, 7),
                false,
                &mut Rolls::new(seed),
            );
            assert!(matches!(step, Some(SouthEast | SouthWest)), "got {step:?}");
        }
        assert_eq!(escape_from(&map, rat, &at(16, 7)), Some(East));
    }

    #[test]
    fn a_diagonal_target_is_not_approached_by_a_creature_keeping_its_distance() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        wall(&mut map, 15, 11);
        wall(&mut map, 16, 10);
        wall(&mut map, 16, 11);

        assert_eq!(keep_distance_from(&map, rat, &at(14, 9)), None);
        assert!(matches!(
            escape_from(&map, rat, &at(14, 9)),
            Some(North | West)
        ));
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

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(
            actions.iter().any(|action| matches!(
                action,
                CreatureAction::Walk { agent_key, direction: West } if *agent_key == rat
            )),
            "expected a step away from the player, got {actions:?}"
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

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(
            actions.iter().any(|action| matches!(
                action,
                CreatureAction::Walk {
                    direction: East,
                    ..
                }
            )),
            "expected a step towards the player, got {actions:?}"
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
                target: TargetMode::Target { range: 1 },
                effect_id: None,
                missile_id: None,
            }),
        }
    }

    /// A caster adjacent to its target, so `in_combat` walks nowhere and falls straight
    /// through to the ability roll.
    fn a_caster_in_melee(map: &mut GameMap, ability: CreatureAbility) -> AgentKey {
        a_kind_in_melee(
            map,
            CreatureKind {
                abilities: vec![ability],
                ..a_creature_kind("Demon")
            },
        )
    }

    fn a_kind_in_melee(map: &mut GameMap, kind: CreatureKind) -> AgentKey {
        let creature = map
            .insert_agent(
                Agent::from_creature_kind(Arc::new(kind), at(15, 10)),
                &at(15, 10),
            )
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(16, 10))
            .unwrap();
        map.get_agent_mut(creature)
            .unwrap()
            .set_target(Some(player), 1);
        creature
    }

    #[test]
    fn an_ability_that_wins_its_roll_is_cast() {
        let mut map = a_field(5..=25, 5..=15);
        let demon = a_caster_in_melee(&mut map, an_attack_ability(MAX_DROP_CHANCE));
        let mut state = CreatureState::default();

        let actions = decide_at(&map, demon, &mut state, Tick(100), 7);

        assert!(
            actions.iter().any(
                |action| matches!(action, CreatureAction::CastAbility { ability_id, .. }
                if *ability_id == CreatureAbilityId(0))
            ),
            "expected a cast, got {actions:?}"
        );
    }

    /// The bug this pins: the roll used to be retried every decision, which turned a low
    /// `chance` into a few hundred milliseconds of delay instead of a gate.
    #[test]
    fn a_lost_roll_puts_the_ability_back_on_cooldown() {
        let mut map = a_field(5..=25, 5..=15);
        let demon = a_caster_in_melee(&mut map, an_attack_ability(0));
        let mut state = CreatureState::default();

        let actions = decide_at(&map, demon, &mut state, Tick(100), 7);

        assert!(
            !is_cast(&actions),
            "a zero chance must never cast, got {actions:?}"
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

        let actions = decide_at(&map, demon, &mut state, Tick(120), 7);

        assert!(
            !is_cast(&actions),
            "the attack group is still on cooldown, got {actions:?}"
        );
        assert_eq!(state.next_ability_tick(CreatureAbilityId(0)), Tick(0));
    }

    #[test]
    fn a_passive_creature_casts_nothing() {
        let mut map = a_field(5..=25, 5..=15);
        let deer = a_kind_in_melee(
            &mut map,
            CreatureKind {
                abilities: vec![an_attack_ability(MAX_DROP_CHANCE)],
                flags: vec![CreatureFlag::Passive],
                ..a_creature_kind("Deer")
            },
        );
        let mut state = CreatureState::default();

        let actions = decide_at(&map, deer, &mut state, Tick(100), 7);

        assert!(
            !is_cast(&actions),
            "a passive creature must never cast, got {actions:?}"
        );
    }

    #[test]
    fn a_passive_creature_still_takes_a_target() {
        let mut map = a_corridor(5..=25);
        let deer = map
            .insert_agent(
                Agent::from_creature_kind(
                    Arc::new(CreatureKind {
                        flags: vec![CreatureFlag::Passive],
                        ..a_creature_kind("Deer")
                    }),
                    Position::new(15, 10, 7),
                ),
                &Position::new(15, 10, 7),
            )
            .unwrap();
        let player = put_player(&mut map, 18, 1);
        let mut state = CreatureState::default();

        let actions = decide_at(&map, deer, &mut state, Tick(100), 7);

        assert!(
            new_target(&actions) == Some(player),
            "expected the deer to target the player, got {actions:?}"
        );
    }

    // keeping distance

    const ARCHER_DISTANCE: u16 = 4;

    fn put_hunter(
        map: &mut GameMap,
        kind: CreatureKind,
        at: Position,
        target: Position,
    ) -> AgentKey {
        let key = map
            .insert_agent(Agent::from_creature_kind(Arc::new(kind), at.clone()), &at)
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &target)
            .unwrap();
        map.get_agent_mut(key).unwrap().set_target(Some(player), 1);
        key
    }

    fn put_archer_and_target(map: &mut GameMap, archer: Position, target: Position) -> AgentKey {
        put_hunter(
            map,
            CreatureKind {
                target_distance: ARCHER_DISTANCE,
                ..a_creature_kind("Archer")
            },
            archer,
            target,
        )
    }

    fn walked(map: &GameMap, creature: AgentKey) -> Option<Direction> {
        let actions = decide_at(map, creature, &mut CreatureState::default(), Tick(100), 7);
        actions.iter().find_map(|action| match action {
            CreatureAction::Walk { direction, .. } => Some(*direction),
            _ => None,
        })
    }

    #[test]
    fn a_ranged_creature_out_of_range_closes_in() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(21, 10));

        assert_eq!(walked(&map, archer), Some(East));
    }

    #[test]
    fn a_ranged_creature_at_its_distance_holds() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(19, 10));

        assert_eq!(walked(&map, archer), None);
    }

    #[test]
    fn a_ranged_creature_too_close_backs_off() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(17, 10));

        assert_eq!(walked(&map, archer), Some(West));
    }

    #[test]
    fn a_cornered_ranged_creature_holds() {
        let mut map = a_field(15..=17, 10..=10);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(17, 10));

        assert_eq!(walked(&map, archer), None);
    }

    #[test]
    fn a_ranged_creature_at_its_distance_behind_a_wall_moves() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(19, 10), at(15, 10));
        block_sight(&mut map, 17);

        assert!(walked(&map, archer).is_some());
    }

    #[test]
    fn a_melee_creature_two_tiles_away_closes_in() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = map
            .insert_agent(a_test_creature("Rat", 10, (1, 2)), &at(15, 10))
            .unwrap();
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(17, 10))
            .unwrap();
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 1);

        assert_eq!(walked(&map, rat), Some(East));
    }

    // dancing

    fn sorted(mut directions: Vec<Direction>) -> Vec<Direction> {
        directions.sort_by_key(|direction| *direction as u8);
        directions.dedup();
        directions
    }

    fn dance_options(map: &GameMap, creature: AgentKey) -> Vec<Direction> {
        let from = map.agent_position(creature).unwrap();
        let target = map
            .agent_position(map.get_agent(creature).unwrap().target().unwrap())
            .unwrap();
        sorted(
            (0..64)
                .filter_map(|seed| {
                    dance_step(map, creature, from, target, Tick(0), &mut Rolls::new(seed))
                })
                .collect(),
        )
    }

    fn a_shooter(range: u16) -> CreatureKind {
        CreatureKind {
            target_distance: ARCHER_DISTANCE,
            abilities: vec![CreatureAbility {
                id: CreatureAbilityId(0),
                cooldown: TickDelta(40),
                chance: 0,
                effect: AbilityEffect::Attack(CreatureAttack {
                    damage: CreatureAttackDamage {
                        element: CombatElement::Physical,
                        value: Bounds { min: 1, max: 2 },
                        condition: None,
                    },
                    target: TargetMode::Target { range },
                    effect_id: None,
                    missile_id: None,
                }),
            }],
            ..a_creature_kind("Archer")
        }
    }

    #[test]
    fn a_melee_creature_on_a_diagonal_slides_to_a_side() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_hunter(&mut map, a_creature_kind("Rat"), at(15, 10), at(16, 11));

        assert_eq!(dance_options(&map, rat), sorted(vec![South, East]));
    }

    #[test]
    fn a_melee_creature_on_a_side_slides_to_a_diagonal() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_hunter(&mut map, a_creature_kind("Rat"), at(15, 10), at(16, 10));

        assert_eq!(dance_options(&map, rat), sorted(vec![North, South]));
    }

    #[test]
    fn a_ranged_creature_dances_along_its_distance() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(19, 10));

        assert_eq!(dance_options(&map, archer), sorted(vec![North, South]));
    }

    #[test]
    fn a_ranged_creature_inside_its_distance_does_not_dance() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(17, 10));

        assert!(dance_options(&map, archer).is_empty());
    }

    #[test]
    fn a_creature_never_dances_onto_a_tile_it_cannot_or_will_not_enter() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_hunter(&mut map, a_creature_kind("Rat"), at(15, 10), at(16, 10));
        map.place_item(
            &at(15, 9),
            None,
            None,
            an_item(2, HashSet::from([ItemFlag::Avoid])),
        )
        .unwrap();

        assert_eq!(dance_options(&map, rat), sorted(vec![South]));
    }

    fn wall_off_the_flanks(map: &mut GameMap) {
        for x in 16..=18 {
            for y in [9, 11] {
                map.place_item(
                    &at(x, y),
                    None,
                    None,
                    an_item(3, HashSet::from([ItemFlag::Unpass, ItemFlag::Unsight])),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn a_creature_that_can_shoot_only_dances_where_it_still_can() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_hunter(&mut map, a_shooter(7), at(15, 10), at(19, 10));
        wall_off_the_flanks(&mut map);
        assert!(!can_throw(&map, &at(15, 9), &at(19, 10), true));
        assert!(can_throw(&map, &at(15, 11), &at(19, 10), true));

        assert_eq!(dance_options(&map, archer), sorted(vec![South]));
    }

    #[test]
    fn a_creature_that_cannot_shoot_anyway_dances_freely() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(19, 10));
        wall_off_the_flanks(&mut map);

        assert_eq!(dance_options(&map, archer), sorted(vec![North, South]));
    }

    #[test]
    fn a_passive_creature_ignores_its_line_of_fire() {
        let mut map = a_field(5..=25, 5..=15);
        let deer = put_hunter(
            &mut map,
            CreatureKind {
                flags: vec![CreatureFlag::Passive],
                ..a_shooter(7)
            },
            at(15, 10),
            at(19, 10),
        );
        wall_off_the_flanks(&mut map);

        assert_eq!(dance_options(&map, deer), sorted(vec![North, South]));
    }

    fn a_dancer(map: &mut GameMap, dance_chance: u8) -> AgentKey {
        put_hunter(
            map,
            CreatureKind {
                dance_chance,
                ..a_creature_kind("Rat")
            },
            at(15, 10),
            at(16, 10),
        )
    }

    #[test]
    fn a_creature_that_always_stands_never_dances() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = a_dancer(&mut map, 0);

        assert!((0..64).all(|seed| {
            !is_walk(&decide_at(
                &map,
                rat,
                &mut CreatureState::default(),
                Tick(100),
                seed,
            ))
        }));
    }

    #[test]
    fn a_dance_waits_out_its_cooldown() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = a_dancer(&mut map, 100);
        let mut state = CreatureState::default();
        let due = Tick(100) + GAME_CONFIG.movement.dance_ticks;

        assert!(is_walk(&decide_at(&map, rat, &mut state, Tick(100), 7)));
        assert!(!is_walk(&decide_at(&map, rat, &mut state, Tick(101), 7)));
        assert!(is_walk(&decide_at(&map, rat, &mut state, due, 7)));
    }

    #[test]
    fn a_creature_mid_step_does_not_dance() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = a_dancer(&mut map, 100);
        map.get_agent_mut(rat).unwrap().stamp_walk(Tick(200));

        assert!(!is_walk(&decide_at(
            &map,
            rat,
            &mut CreatureState::default(),
            Tick(100),
            7
        )));
    }

    fn search(
        map: &GameMap,
        creature: AgentKey,
        strategy: TargetSearchStrategy,
        seed: u64,
    ) -> Option<AgentKey> {
        search_target(creature, map, strategy, Tick(0), &mut Rolls::new(seed))
    }

    #[test]
    fn a_random_search_reaches_every_player_it_can_walk_to() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let west = put_player(&mut map, 12, 1);
        let east = put_player(&mut map, 18, 2);

        let picked: HashSet<AgentKey> = (0..64)
            .filter_map(|seed| search(&map, rat, TargetSearchStrategy::Random, seed))
            .collect();

        assert_eq!(picked, HashSet::from([west, east]));
    }

    #[test]
    fn a_random_search_skips_a_player_it_cannot_walk_to() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let walkable = put_player(&mut map, 12, 1);
        put_player(&mut map, 18, 2);
        block_walking(&mut map, 17);

        assert!(
            (0..64).all(|seed| search(&map, rat, TargetSearchStrategy::Random, seed)
                == Some(walkable))
        );
    }

    #[test]
    fn a_damage_search_picks_among_those_who_dealt_damage() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let attacker = put_player(&mut map, 12, 1);
        put_player(&mut map, 18, 2);
        map.get_agent_mut(rat)
            .unwrap()
            .record_damage(attacker, 50, Tick(0));

        assert!(
            (0..64).all(|seed| search(&map, rat, TargetSearchStrategy::Damage, seed)
                == Some(attacker))
        );
    }

    #[test]
    fn a_damage_search_favours_whoever_dealt_more() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let scratch = put_player(&mut map, 12, 1);
        let heavy = put_player(&mut map, 18, 2);
        let agent = map.get_agent_mut(rat).unwrap();
        agent.record_damage(scratch, 1, Tick(0));
        agent.record_damage(heavy, 99, Tick(0));

        let picks: Vec<AgentKey> = (0..256)
            .filter_map(|seed| search(&map, rat, TargetSearchStrategy::Damage, seed))
            .collect();
        let heavy_picks = picks.iter().filter(|key| **key == heavy).count();

        assert_eq!(picks.len(), 256);
        assert!(
            heavy_picks > 200,
            "99 of every 100 damage should win nearly every roll, won {heavy_picks} of 256"
        );
    }

    #[test]
    fn a_damage_search_ignores_an_attacker_it_cannot_see() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let visible = put_player(&mut map, 12, 1);
        let hidden = put_player(&mut map, 19, 2);
        block_sight(&mut map, 17);
        map.get_agent_mut(rat)
            .unwrap()
            .record_damage(hidden, 50, Tick(0));

        assert!(
            (0..64)
                .all(|seed| search(&map, rat, TargetSearchStrategy::Damage, seed) == Some(visible))
        );
    }

    const CHANGE_INTERVAL: TickDelta = TickDelta(80);

    /// A melee creature at (15, 10) holding the player to its east, so `in_combat` walks
    /// nowhere and falls through to the target change. Returns the creature and its target.
    fn a_switcher(map: &mut GameMap, chance: u32, interval: TickDelta) -> (AgentKey, AgentKey) {
        let creature = a_kind_in_melee(
            map,
            CreatureKind {
                change_target: ChangeTarget { interval, chance },
                ..a_creature_kind("Rat")
            },
        );
        let target = map.get_agent(creature).unwrap().target().unwrap();
        (creature, target)
    }

    fn put_bystander(map: &mut GameMap) -> AgentKey {
        map.insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &at(14, 10))
            .unwrap()
    }

    fn decisions(map: &GameMap, creature: AgentKey, tick: Tick) -> Vec<Vec<CreatureAction>> {
        (0..64)
            .map(|seed| decide_at(map, creature, &mut CreatureState::default(), tick, seed))
            .collect()
    }

    #[test]
    fn a_won_roll_switches_to_another_player_and_never_to_the_current_one() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 100, CHANGE_INTERVAL);
        let bystander = put_bystander(&mut map);

        let decided = decisions(&map, rat, Tick(100));

        assert!(decided.iter().any(|actions| is_set_target(actions)));
        assert!(
            decided
                .iter()
                .filter(|actions| is_set_target(actions))
                .all(|actions| new_target(actions) == Some(bystander))
        );
    }

    #[test]
    fn a_zero_chance_never_changes_target() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 0, CHANGE_INTERVAL);
        put_bystander(&mut map);

        assert!(
            !decisions(&map, rat, Tick(100))
                .iter()
                .any(|actions| is_set_target(actions))
        );
    }

    #[test]
    fn a_zero_interval_never_changes_target() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 100, TickDelta(0));
        put_bystander(&mut map);

        assert!(
            !decisions(&map, rat, Tick(100))
                .iter()
                .any(|actions| is_set_target(actions))
        );
    }

    #[test]
    fn a_creature_with_one_player_in_view_keeps_it() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 100, CHANGE_INTERVAL);

        assert!(
            !decisions(&map, rat, Tick(100))
                .iter()
                .any(|actions| is_set_target(actions))
        );
    }

    #[test]
    fn a_lost_roll_spends_the_interval() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 0, CHANGE_INTERVAL);
        put_bystander(&mut map);
        let mut state = CreatureState::default();

        decide_at(&map, rat, &mut state, Tick(100), 7);

        assert_eq!(state.next_change_target_tick, Tick(100) + CHANGE_INTERVAL);
    }

    #[test]
    fn a_target_change_waits_out_its_interval() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, _) = a_switcher(&mut map, 100, CHANGE_INTERVAL);
        put_bystander(&mut map);
        let due = Tick(100) + CHANGE_INTERVAL;
        let decide = |tick: Tick, seed: u64| {
            let mut state = CreatureState {
                next_change_target_tick: due,
                ..CreatureState::default()
            };
            decide_at(&map, rat, &mut state, tick, seed)
        };

        assert!(!(0..64).any(|seed| is_set_target(&decide(Tick(due.0 - 1), seed))));
        assert!((0..64).any(|seed| is_set_target(&decide(due, seed))));
    }

    fn change_interval_of(map: &GameMap, creature: AgentKey) -> TickDelta {
        map.get_agent(creature)
            .unwrap()
            .get_creature_kind()
            .unwrap()
            .change_target
            .interval
    }

    #[test]
    fn taking_a_target_while_idle_starts_the_interval() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let player = put_player(&mut map, 17, 1);
        let mut state = CreatureState::default();

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert_eq!(new_target(&actions), Some(player));
        assert_eq!(
            state.next_change_target_tick,
            Tick(100) + change_interval_of(&map, rat)
        );
    }

    #[test]
    fn taking_a_target_on_the_way_home_starts_the_interval() {
        let mut map = a_corridor(5..=25);
        let origin = Position::new(15 + GAME_CONFIG.movement.wander_distance + 1, 10, 7);
        let rat = map
            .insert_agent(
                Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), origin),
                &at(15, 10),
            )
            .unwrap();
        let player = put_player(&mut map, 13, 1);
        let mut state = CreatureState::default();

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert_eq!(new_target(&actions), Some(player));
        assert_eq!(
            state.next_change_target_tick,
            Tick(100) + change_interval_of(&map, rat)
        );
    }

    #[test]
    fn a_creature_that_cannot_reach_its_target_takes_one_it_can_and_starts_the_interval() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let walkable = put_player(&mut map, 12, 1);
        let walled_off = put_player(&mut map, 18, 2);
        block_walking(&mut map, 17);
        map.get_agent_mut(rat)
            .unwrap()
            .set_target(Some(walled_off), 1);
        let mut state = CreatureState::default();

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert_eq!(new_target(&actions), Some(walkable));
        assert_eq!(
            state.next_change_target_tick,
            Tick(100) + change_interval_of(&map, rat)
        );
    }

    /// Mid-step, so `fleeing` has no escape step to take and reaches its retarget.
    fn a_frightened_creature_mid_step(map: &mut GameMap, target: AgentKey) -> AgentKey {
        let rat = put_frightened_creature(map, 15, 10);
        let agent = map.get_agent_mut(rat).unwrap();
        agent.set_target(Some(target), 1);
        agent.stamp_walk(Tick(200));
        rat
    }

    #[test]
    fn a_fleeing_creature_runs_from_whoever_is_nearest() {
        let mut map = a_field(5..=25, 5..=15);
        let far = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(20, 10))
            .unwrap();
        let near = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &at(13, 10))
            .unwrap();
        let rat = a_frightened_creature_mid_step(&mut map, far);

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert_eq!(new_target(&actions), Some(near));
    }

    #[test]
    fn a_fleeing_creature_that_sees_nobody_keeps_its_target() {
        let mut map = a_field(5..=25, 5..=15);
        let hidden = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(18, 10))
            .unwrap();
        map.place_item(
            &at(16, 10),
            None,
            None,
            an_item(3, HashSet::from([ItemFlag::Unpass, ItemFlag::Unsight])),
        )
        .unwrap();
        let rat = a_frightened_creature_mid_step(&mut map, hidden);

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
    }

    #[test]
    fn a_fleeing_creature_with_nowhere_to_run_still_casts() {
        let mut map = a_field(15..=16, 10..=10);
        let rat = a_kind_in_melee(
            &mut map,
            CreatureKind {
                life: Pool {
                    current: 10,
                    maximum: 10,
                },
                flee_threshold: Some(5),
                abilities: vec![an_attack_ability(MAX_DROP_CHANCE)],
                ..a_creature_kind("Rat")
            },
        );
        map.get_agent_mut(rat).unwrap().remove_life(6);

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert!(is_cast(&actions), "expected a cast, got {actions:?}");
    }

    /// A hunter at (15, 10) chasing a player four tiles east, with no target change due, so
    /// any `SetTarget` it decides on came from someone standing in its way.
    fn a_chaser(map: &mut GameMap, kind: CreatureKind) -> (AgentKey, CreatureState) {
        let creature = put_hunter(map, kind, at(15, 10), at(19, 10));
        let state = CreatureState {
            next_change_target_tick: Tick(u64::MAX),
            ..CreatureState::default()
        };
        (creature, state)
    }

    fn put_blocker(map: &mut GameMap, x: u16, y: u16) -> AgentKey {
        map.insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &at(x, y))
            .unwrap()
    }

    #[test]
    fn a_player_standing_in_the_way_takes_the_target_and_starts_the_interval() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(&mut map, a_creature_kind("Rat"));
        let blocker = put_blocker(&mut map, 16, 10);

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(
            matches!(
                actions.first(),
                Some(CreatureAction::SetTarget { agent_key, target })
                    if *agent_key == rat && *target == Some(blocker)
            ),
            "got {actions:?}"
        );
        assert!(!is_walk(&actions), "got {actions:?}");
        assert_eq!(
            state.next_change_target_tick,
            Tick(100) + change_interval_of(&map, rat)
        );
    }

    #[test]
    fn a_body_blocked_creature_still_casts() {
        let mut map = a_field(5..=25, 5..=15);
        let (demon, mut state) = a_chaser(
            &mut map,
            CreatureKind {
                abilities: vec![an_attack_ability(MAX_DROP_CHANCE)],
                ..a_creature_kind("Demon")
            },
        );
        let blocker = put_blocker(&mut map, 16, 10);

        let actions = decide_at(&map, demon, &mut state, Tick(100), 7);

        assert!(
            matches!(
                actions.as_slice(),
                [
                    CreatureAction::SetTarget { target, .. },
                    CreatureAction::CastAbility { .. },
                    ..
                ] if *target == Some(blocker)
            ),
            "the cast must follow the switch, got {actions:?}"
        );
    }

    #[test]
    fn a_player_beside_the_way_is_walked_past() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(&mut map, a_creature_kind("Rat"));
        put_blocker(&mut map, 16, 11);

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
        assert!(is_walk(&actions), "got {actions:?}");
    }

    #[test]
    fn a_creature_that_never_changes_target_walks_around_a_player_in_the_way() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(
            &mut map,
            CreatureKind {
                change_target: ChangeTarget {
                    interval: CHANGE_INTERVAL,
                    chance: 0,
                },
                ..a_creature_kind("Rat")
            },
        );
        put_blocker(&mut map, 16, 10);

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
        assert!(is_walk(&actions), "got {actions:?}");
    }

    #[test]
    fn another_creature_in_the_way_does_not_take_the_target() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(&mut map, a_creature_kind("Rat"));
        map.insert_agent(
            Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), at(16, 10)),
            &at(16, 10),
        )
        .unwrap();

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
    }

    #[test]
    fn a_creature_beside_its_target_ignores_the_player_next_to_it() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_hunter(&mut map, a_creature_kind("Rat"), at(15, 10), at(16, 11));
        put_blocker(&mut map, 16, 10);
        map.insert_agent(Agent::from_player(a_test_snapshot(3, 1)), &at(15, 11))
            .unwrap();
        let mut state = CreatureState {
            next_change_target_tick: Tick(u64::MAX),
            ..CreatureState::default()
        };

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
    }

    #[test]
    fn a_ranged_creature_holding_its_distance_is_still_body_blocked() {
        let mut map = a_field(5..=25, 5..=15);
        let (archer, mut state) = a_chaser(
            &mut map,
            CreatureKind {
                target_distance: ARCHER_DISTANCE,
                ..a_creature_kind("Archer")
            },
        );
        let blocker = put_blocker(&mut map, 16, 10);

        let actions = decide_at(&map, archer, &mut state, Tick(100), 7);

        assert_eq!(new_target(&actions), Some(blocker), "got {actions:?}");
    }

    #[test]
    fn a_creature_mid_step_is_not_body_blocked() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(&mut map, a_creature_kind("Rat"));
        put_blocker(&mut map, 16, 10);
        map.get_agent_mut(rat).unwrap().stamp_walk(Tick(200));

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
    }

    #[test]
    fn a_creature_that_sees_but_cannot_reach_its_only_target_still_casts() {
        let mut map = a_corridor(5..=25);
        let pos = Position::new(15, 10, 7);
        let rat = map
            .insert_agent(
                Agent::from_creature_kind(
                    Arc::new(CreatureKind {
                        abilities: vec![an_attack_ability(MAX_DROP_CHANCE)],
                        ..a_creature_kind("Rat")
                    }),
                    pos.clone(),
                ),
                &pos,
            )
            .unwrap();
        let player = put_player(&mut map, 18, 1);
        block_walking(&mut map, 17);
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 1);

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
        assert!(is_cast(&actions), "got {actions:?}");
    }

    fn challenge(map: &mut GameMap, creature: AgentKey, by: AgentKey, until: Tick) {
        map.get_agent_mut(creature)
            .unwrap()
            .set_challenge(Some(Challenge {
                by,
                since: Tick(0),
                until,
            }));
    }

    #[test]
    fn a_challenged_creature_does_not_roll_to_change_target_until_the_challenge_ends() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, target) = a_switcher(&mut map, 100, CHANGE_INTERVAL);
        put_bystander(&mut map);
        challenge(&mut map, rat, target, Tick(200));

        assert!(
            !decisions(&map, rat, Tick(100))
                .iter()
                .any(|actions| is_set_target(actions))
        );
        assert!(
            decisions(&map, rat, Tick(200))
                .iter()
                .any(|actions| is_set_target(actions))
        );
    }

    #[test]
    fn a_challenged_creature_walks_round_a_player_in_its_way() {
        let mut map = a_field(5..=25, 5..=15);
        let (rat, mut state) = a_chaser(&mut map, a_creature_kind("Rat"));
        let challenger = map.get_agent(rat).unwrap().target().unwrap();
        put_blocker(&mut map, 16, 10);
        challenge(&mut map, rat, challenger, Tick(200));

        let actions = decide_at(&map, rat, &mut state, Tick(100), 7);

        assert!(!is_set_target(&actions), "got {actions:?}");
        assert!(is_walk(&actions), "got {actions:?}");
    }

    #[test]
    fn a_challenged_creature_fights_instead_of_fleeing() {
        let mut map = a_field(5..=25, 5..=15);
        let rat = put_frightened_creature(&mut map, 15, 10);
        let player = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &at(16, 10))
            .unwrap();
        map.get_agent_mut(rat).unwrap().set_target(Some(player), 1);
        challenge(&mut map, rat, player, Tick(200));

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert!(
            !actions.iter().any(|action| matches!(
                action,
                CreatureAction::Walk {
                    direction: West,
                    ..
                }
            )),
            "got {actions:?}"
        );
    }

    #[test]
    fn a_challenged_creature_that_cannot_reach_its_challenger_takes_one_it_can() {
        let mut map = a_corridor(5..=25);
        let rat = put_creature(&mut map, 15);
        let challenger = put_player(&mut map, 20, 1);
        let reachable = put_player(&mut map, 12, 2);
        block_sight(&mut map, 18);
        map.get_agent_mut(rat)
            .unwrap()
            .set_target(Some(challenger), 1);
        challenge(&mut map, rat, challenger, Tick(200));

        let actions = decide_at(&map, rat, &mut CreatureState::default(), Tick(100), 7);

        assert_eq!(new_target(&actions), Some(reachable), "got {actions:?}");
    }

    #[test]
    fn a_ranged_creature_forced_into_melee_closes_in() {
        let mut map = a_field(5..=25, 5..=15);
        let archer = put_archer_and_target(&mut map, at(15, 10), at(19, 10));
        map.get_agent_mut(archer).unwrap().force_melee(Tick(200));

        assert_eq!(walked(&map, archer), Some(East));
    }
}

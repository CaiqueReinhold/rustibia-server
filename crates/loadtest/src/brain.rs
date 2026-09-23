use std::time::{Duration, Instant};

use rustibia_server::config::CONFIG;
use rustibia_server::constants::items::{
    CARRIED_SEARCH_FLAG, CONTAINER_COORD_FLAG, INVENTORY_COORD_FLAG,
};
use rustibia_server::entities::agent::{AgentId, walk_ticks};
use rustibia_server::entities::items::{ClientItemRef, ContainerId, ItemFlag, ItemId};
use rustibia_server::entities::position::{ALL_DIRECTIONS, Direction, Position};
use rustibia_server::entities::spells::{SpellGroup, SpellId, SpellTarget};
use rustibia_server::messages::ClientMessage;

#[cfg(test)]
use crate::config::Coords;
use crate::config::{Behaviour, Route};
use crate::world::World;

const HEALTH_POTION: ItemId = ItemId(266);
const MANA_POTION: ItemId = ItemId(268);

const CORPSE_ABANDON: Duration = Duration::from_secs(2);
const CORPSE_OPEN_ATTEMPTS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DryPotion {
    Health,
    Mana,
}

pub fn step_duration(speed: u16, tile_friction: u16, diagonal: bool) -> Duration {
    let tick_ms = CONFIG.tick_duration.as_millis() as u64;
    Duration::from_millis(walk_ticks(speed, tile_friction, diagonal).0 * tick_ms)
}

fn below_pct(current: u32, maximum: u32, pct: u32) -> bool {
    maximum > 0 && current * 100 / maximum < pct
}

/// The two compass directions either side of `direction` in `ALL_DIRECTIONS`.
fn adjacent_directions(direction: Direction) -> [Direction; 2] {
    let len = ALL_DIRECTIONS.len();
    let index = ALL_DIRECTIONS
        .iter()
        .position(|&d| d == direction)
        .expect("every Direction is in ALL_DIRECTIONS");
    [
        ALL_DIRECTIONS[(index + len - 1) % len],
        ALL_DIRECTIONS[(index + 1) % len],
    ]
}

/// Prefers the direction that closes both axes at once, and when it's
/// blocked, the two directions next to it on the compass before any other —
/// each of those still makes progress on the axis the preferred direction
/// was serving, where a fixed compass scan can pick a direction that makes
/// none and repeat it forever.
fn choose_direction(world: &World, to: Position) -> Option<Direction> {
    let from = &world.position;
    if from.z != to.z || *from == to {
        return None;
    }

    let dx = (to.x as i32 - from.x as i32).signum();
    let dy = (to.y as i32 - from.y as i32).signum();
    let preferred = Direction::from_step(dx, dy)?;

    if world.is_walkable(from.clone() + preferred) {
        return Some(preferred);
    }

    let [left, right] = adjacent_directions(preferred);
    [left, right]
        .into_iter()
        .chain(
            ALL_DIRECTIONS
                .into_iter()
                .filter(|&d| d != preferred && d != left && d != right),
        )
        .find(|&direction| world.is_walkable(from.clone() + direction))
}

/// The destination coordinate the server's `resolve_client_coord` reads for
/// a container placement: `x` is `CONTAINER_COORD_FLAG`, `y` the destination
/// `ContainerId`, `z` the slot index within it.
fn backpack_destination(backpack: ContainerId) -> Position {
    Position::new(CONTAINER_COORD_FLAG, backpack.0, 0)
}

struct TargetState {
    id: AgentId,
    seq: u32,
    last_seen: Position,
}

enum LootJob {
    Awaiting {
        corpse: Position,
        since: Instant,
    },
    Opening {
        corpse: Position,
        since: Instant,
        attempts: u8,
    },
    Emptying {
        container: ContainerId,
        looted: usize,
    },
}

pub struct Brain {
    behaviour: Behaviour,
    ignore_prefix: String,
    waypoints: Vec<Position>,
    waypoint_index: usize,
    next_step_at: Option<Instant>,
    target: Option<TargetState>,
    next_target_seq: u32,
    loot: Option<LootJob>,
    health_dry_reported: bool,
    mana_dry_reported: bool,
    dry_events: Vec<DryPotion>,
}

impl Brain {
    pub fn new(route: Route, behaviour: Behaviour, ignore_prefix: String) -> Self {
        let waypoints = route
            .waypoints
            .iter()
            .map(|c| Position::new(c.x as u16, c.y as u16, c.z as u8))
            .collect();

        Self {
            behaviour,
            ignore_prefix,
            waypoints,
            waypoint_index: 0,
            next_step_at: None,
            target: None,
            next_target_seq: 0,
            loot: None,
            health_dry_reported: false,
            mana_dry_reported: false,
            dry_events: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn waypoint_index(&self) -> usize {
        self.waypoint_index
    }

    /// `bot` schedules its own decision and ping timers from this — jitter and
    /// cadence are its concern, not `Brain`'s, since `Brain::decide` only paces
    /// the travel steps it returns.
    pub fn behaviour(&self) -> &Behaviour {
        &self.behaviour
    }

    /// The configured spell names (`heal_spell`, `attack_spell`) that resolve
    /// against no entry in `world.spells` — a name the server never taught
    /// this bot, which would otherwise mean it silently never heals or never
    /// attacks for the whole run.
    pub fn missing_spells(&self, world: &World) -> Vec<String> {
        [&self.behaviour.heal_spell, &self.behaviour.attack_spell]
            .into_iter()
            .filter(|words| self.known_spell(world, words).is_none())
            .cloned()
            .collect()
    }

    pub fn take_dry_events(&mut self) -> Vec<DryPotion> {
        std::mem::take(&mut self.dry_events)
    }

    pub fn decide(&mut self, world: &World, now: Instant) -> Option<ClientMessage> {
        self.observe_target(world, now);

        self.drink_health(world)
            .or_else(|| self.cast_heal(world, now))
            .or_else(|| self.drink_mana(world))
            .or_else(|| self.loot(world, now))
            .or_else(|| self.acquire_target(world))
            .or_else(|| self.cast_attack(world, now))
            .or_else(|| self.close(world, now))
            .or_else(|| self.travel(world, now))
    }

    /// The one place the held target's fate is resolved. Presence in
    /// `world.agents` decides everything: gone is a kill, whatever `TargetLost`
    /// did or didn't say, because the server sends a `TargetLost` for the
    /// killer too (`death::reap` clears every agent targeting the deceased,
    /// including the one who did it) before the `RemoveAgent` that follows.
    /// `TargetLost` only ever clears a target that is *still visible* — the
    /// real out-of-range/stale-seq case — and only when its `seq` names the
    /// target currently held, so a late one for an already-superseded target
    /// can't cancel the new one.
    fn observe_target(&mut self, world: &World, now: Instant) {
        let Some(state) = self.target.as_mut() else {
            return;
        };
        let id = state.id;
        let seq = state.seq;

        match world.agents.get(&id) {
            Some(agent) => {
                state.last_seen = agent.position.clone();
                if world.target_lost_seq == Some(seq) {
                    self.target = None;
                }
            }
            None => {
                let corpse = state.last_seen.clone();
                self.loot = Some(LootJob::Awaiting { corpse, since: now });
                self.target = None;
            }
        }
    }

    fn drink_health(&mut self, world: &World) -> Option<ClientMessage> {
        if !below_pct(
            world.life.current,
            world.life.maximum,
            self.behaviour.health_potion_pct,
        ) {
            return None;
        }
        if world.carried_amount(HEALTH_POTION) == 0 {
            if !self.health_dry_reported {
                self.health_dry_reported = true;
                self.dry_events.push(DryPotion::Health);
            }
            return None;
        }
        Some(self.drink(world, HEALTH_POTION))
    }

    fn cast_heal(&self, world: &World, now: Instant) -> Option<ClientMessage> {
        if !below_pct(
            world.life.current,
            world.life.maximum,
            self.behaviour.heal_spell_pct,
        ) {
            return None;
        }
        let (id, group) = self.known_spell(world, &self.behaviour.heal_spell)?;
        if !world.spell_ready(id, group, now) {
            return None;
        }
        Some(ClientMessage::CastSpell {
            spell_id: id,
            target: SpellTarget::None,
            param: None,
        })
    }

    fn drink_mana(&mut self, world: &World) -> Option<ClientMessage> {
        if !below_pct(
            world.mana.current,
            world.mana.maximum,
            self.behaviour.mana_potion_pct,
        ) {
            return None;
        }
        if world.carried_amount(MANA_POTION) == 0 {
            if !self.mana_dry_reported {
                self.mana_dry_reported = true;
                self.dry_events.push(DryPotion::Mana);
            }
            return None;
        }
        Some(self.drink(world, MANA_POTION))
    }

    fn drink(&self, world: &World, potion: ItemId) -> ClientMessage {
        ClientMessage::UseItemWith {
            source: ClientItemRef {
                position: Position::new(INVENTORY_COORD_FLAG, CARRIED_SEARCH_FLAG, 0),
                item_id: potion,
                stack_index: 0,
            },
            target: ClientItemRef {
                position: world.position.clone(),
                item_id: ItemId(0),
                stack_index: 0,
            },
            target_agent: Some(world.agent_id),
        }
    }

    fn known_spell(&self, world: &World, words: &str) -> Option<(SpellId, SpellGroup)> {
        world
            .spells
            .iter()
            .find(|(_, spell)| spell.words == words)
            .map(|(id, spell)| (*id, spell.group))
    }

    fn acquire_target(&mut self, world: &World) -> Option<ClientMessage> {
        if self.target.is_some() {
            return None;
        }

        let (&id, agent) = world
            .agents
            .iter()
            .filter(|(_, agent)| !agent.name.starts_with(&self.ignore_prefix))
            .filter(|(_, agent)| {
                world
                    .position
                    .is_within(&agent.position, self.behaviour.engage_radius)
            })
            .min_by_key(|(_, agent)| world.position.distance(&agent.position))?;

        self.next_target_seq += 1;
        let seq = self.next_target_seq;
        self.target = Some(TargetState {
            id,
            seq,
            last_seen: agent.position.clone(),
        });
        Some(ClientMessage::SetTarget {
            agent_id: Some(id),
            seq,
        })
    }

    fn cast_attack(&self, world: &World, now: Instant) -> Option<ClientMessage> {
        let state = self.target.as_ref()?;
        let target = world.agents.get(&state.id)?;
        if !world
            .position
            .is_within(&target.position, self.behaviour.engage_radius)
        {
            return None;
        }
        let (id, group) = self.known_spell(world, &self.behaviour.attack_spell)?;
        if !world.spell_ready(id, group, now) {
            return None;
        }
        Some(ClientMessage::CastSpell {
            spell_id: id,
            target: SpellTarget::Agent(state.id),
            param: None,
        })
    }

    fn close(&mut self, world: &World, now: Instant) -> Option<ClientMessage> {
        let destination = {
            let state = self.target.as_ref()?;
            let target = world.agents.get(&state.id)?;
            if world.position.is_within(&target.position, 1) {
                return None;
            }
            target.position.clone()
        };
        self.step_toward(world, now, destination)
    }

    /// Holding a target and walking the route at the same time turns every
    /// cooldown gap into a travel step — the fight either drags along or the
    /// bot walks out of viewport into a re-acquire churn. Standing still
    /// between attacks is the correct trade for a combat-load generator.
    fn travel(&mut self, world: &World, now: Instant) -> Option<ClientMessage> {
        if self.target.is_some() {
            return None;
        }
        let waypoint = self.waypoints.get(self.waypoint_index)?.clone();
        if world.position == waypoint {
            self.waypoint_index = (self.waypoint_index + 1) % self.waypoints.len();
        }
        let destination = self.waypoints[self.waypoint_index].clone();
        self.step_toward(world, now, destination)
    }

    fn step_toward(
        &mut self,
        world: &World,
        now: Instant,
        destination: Position,
    ) -> Option<ClientMessage> {
        if self.next_step_at.is_some_and(|at| now < at) {
            return None;
        }
        let direction = choose_direction(world, destination)?;
        let landing = world.position.clone() + direction;
        let friction = world.friction_at(landing)?;
        self.next_step_at =
            Some(now + step_duration(world.speed, friction, direction.is_diagonal()));
        Some(ClientMessage::MovePlayer { direction })
    }

    fn tile_has_container(world: &World, position: &Position) -> bool {
        world.tiles.get(position).is_some_and(|tile| {
            tile.iter().flatten().any(|(id, _)| {
                world
                    .items
                    .get(id)
                    .is_some_and(|c| c.has_flag(ItemFlag::Container))
            })
        })
    }

    fn corpse_item_ref(world: &World, position: &Position) -> Option<ClientItemRef> {
        let tile = world.tiles.get(position)?;
        let (index, item_id) = tile.iter().enumerate().find_map(|(index, slot)| {
            let (item_id, _) = (*slot)?;
            world
                .items
                .get(&item_id)?
                .has_flag(ItemFlag::Container)
                .then_some((index, item_id))
        })?;
        Some(ClientItemRef {
            position: position.clone(),
            item_id,
            stack_index: index as u8,
        })
    }

    fn loot(&mut self, world: &World, now: Instant) -> Option<ClientMessage> {
        let backpack = world.backpack()?;

        match self.loot.take()? {
            LootJob::Awaiting { corpse, since } => {
                if Self::tile_has_container(world, &corpse) {
                    let item = Self::corpse_item_ref(world, &corpse);
                    self.loot = Some(LootJob::Opening {
                        corpse,
                        since: now,
                        attempts: 1,
                    });
                    item.map(|item| ClientMessage::UseItem { item })
                } else if now.duration_since(since) <= CORPSE_ABANDON {
                    self.loot = Some(LootJob::Awaiting { corpse, since });
                    None
                } else {
                    None
                }
            }
            LootJob::Opening {
                corpse,
                since,
                attempts,
            } => {
                if now.duration_since(since) > CORPSE_ABANDON {
                    None
                } else if attempts >= CORPSE_OPEN_ATTEMPTS {
                    self.loot = Some(LootJob::Opening {
                        corpse,
                        since,
                        attempts,
                    });
                    None
                } else {
                    let item = Self::corpse_item_ref(world, &corpse);
                    self.loot = Some(LootJob::Opening {
                        corpse,
                        since,
                        attempts: attempts + 1,
                    });
                    item.map(|item| ClientMessage::UseItem { item })
                }
            }
            LootJob::Emptying { container, looted } => {
                let next_item = world.containers.get(&container).and_then(|slots| {
                    slots
                        .iter()
                        .enumerate()
                        .find_map(|(index, slot)| slot.map(|item| (index, item)))
                });

                match next_item {
                    Some((index, (item_id, amount))) if looted < self.behaviour.loot_items_max => {
                        self.loot = Some(LootJob::Emptying {
                            container,
                            looted: looted + 1,
                        });
                        let cumulative = world
                            .items
                            .get(&item_id)
                            .is_some_and(|c| c.has_flag(ItemFlag::Cumulative));
                        Some(ClientMessage::MoveItem {
                            item: ClientItemRef {
                                position: Position::new(
                                    CONTAINER_COORD_FLAG,
                                    container.0,
                                    index as u8,
                                ),
                                item_id,
                                stack_index: 0,
                            },
                            amount: if cumulative { amount } else { 1 },
                            to: backpack_destination(backpack),
                        })
                    }
                    _ => Some(ClientMessage::CloseContainer {
                        container_id: container,
                    }),
                }
            }
        }
    }

    pub fn corpse_opened(&mut self, container: ContainerId) {
        if let Some(LootJob::Opening { .. }) = &self.loot {
            self.loot = Some(LootJob::Emptying {
                container,
                looted: 0,
            });
        }
    }

    #[cfg(test)]
    pub fn set_target(&mut self, id: AgentId) {
        self.target = Some(TargetState {
            id,
            seq: 0,
            last_seen: Position::default(),
        });
    }

    #[cfg(test)]
    pub fn opened_corpse(&mut self, container: ContainerId) {
        self.loot = Some(LootJob::Emptying {
            container,
            looted: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use rustibia_server::entities::spells::SpellGroup;
    use rustibia_server::messages::ServerMessage;

    /// The first container id a fresh session ever mints (`FIRST_INDEX = 1`,
    /// `GEN_BITS = 4` in the server's `local_id.rs`) — used here only to
    /// stand in for "the bot has a backpack", not to assert on that formula.
    const A_BACKPACK: ContainerId = ContainerId(16);

    fn a_brain() -> Brain {
        Brain::new(
            Route {
                name: "test".to_string(),
                waypoints: vec![Coords {
                    x: 102,
                    y: 100,
                    z: 7,
                }],
            },
            Behaviour::test_default(),
            "Loadbot ".to_string(),
        )
    }

    #[test]
    fn travel_steps_toward_the_waypoint() {
        let world = a_world_on_open_ground();
        let mut brain = a_brain();

        let action = brain.decide(&world, Instant::now());

        assert_eq!(
            action,
            Some(ClientMessage::MovePlayer {
                direction: Direction::East
            })
        );
    }

    #[test]
    fn a_multi_waypoint_route_advances_to_the_next_one() {
        let mut world = a_world_on_open_ground();
        world.place_self(AgentId(1), Position::new(102, 100, 7), 100);
        let mut brain = Brain::new(
            Route {
                name: "test".to_string(),
                waypoints: vec![
                    Coords {
                        x: 102,
                        y: 100,
                        z: 7,
                    },
                    Coords {
                        x: 100,
                        y: 100,
                        z: 7,
                    },
                ],
            },
            Behaviour::test_default(),
            "Loadbot ".to_string(),
        );

        let action = brain.decide(&world, Instant::now());

        assert_eq!(brain.waypoint_index(), 1);
        assert_eq!(
            action,
            Some(ClientMessage::MovePlayer {
                direction: Direction::West
            })
        );
    }

    #[test]
    fn no_step_is_sent_before_the_previous_one_has_had_time_to_land() {
        let world = a_world_on_open_ground();
        let mut brain = a_brain();
        let now = Instant::now();

        brain.decide(&world, now);

        assert_eq!(brain.decide(&world, now + Duration::from_millis(10)), None);
    }

    #[test]
    fn a_step_is_sent_once_the_walk_duration_has_elapsed() {
        let world = a_world_on_open_ground();
        let mut brain = a_brain();
        let now = Instant::now();
        let destination_friction = world.friction_at(Position::new(101, 100, 7)).unwrap();

        brain.decide(&world, now);

        let elapsed =
            step_duration(world.speed, destination_friction, false) + Duration::from_millis(1);

        assert!(brain.decide(&world, now + elapsed).is_some());
    }

    #[test]
    fn pacing_uses_the_destination_tiles_friction_not_the_origins() {
        let mut world = a_world_on_open_ground();
        world.set_tile(Position::new(101, 100, 7), stack(&[ItemId(2)]));
        let mut brain = a_brain();
        let now = Instant::now();

        let origin_friction = world.friction_at(world.position.clone()).unwrap();
        let destination_friction = world.friction_at(Position::new(101, 100, 7)).unwrap();
        assert_ne!(origin_friction, destination_friction);

        let action = brain.decide(&world, now);
        assert_eq!(
            action,
            Some(ClientMessage::MovePlayer {
                direction: Direction::East
            })
        );

        let using_origin =
            now + step_duration(world.speed, origin_friction, false) + Duration::from_millis(1);
        assert_eq!(
            brain.decide(&world, using_origin),
            None,
            "the origin's cheaper friction must not gate the wait"
        );

        let using_destination = now
            + step_duration(world.speed, destination_friction, false)
            + Duration::from_millis(1);
        assert!(brain.decide(&world, using_destination).is_some());
    }

    #[test]
    fn a_blocked_direction_is_sidestepped_toward_the_waypoint() {
        let mut world = a_world_on_open_ground();
        world.block(Position::new(101, 100, 7));
        let mut brain = a_brain();
        let before = world.position.clone();
        let waypoint = Position::new(102, 100, 7);

        let Some(ClientMessage::MovePlayer { direction }) = brain.decide(&world, Instant::now())
        else {
            panic!("expected a step");
        };

        let after = before.clone() + direction;
        assert!(
            after.distance(&waypoint) < before.distance(&waypoint),
            "the sidestep must still make progress toward the waypoint"
        );
    }

    #[test]
    fn standing_on_the_only_waypoint_sends_no_step() {
        let mut world = a_world_on_open_ground();
        world.place_self(AgentId(1), Position::new(102, 100, 7), 100);
        let mut brain = a_brain();

        assert_eq!(brain.decide(&world, Instant::now()), None);
        assert_eq!(
            brain.waypoint_index(),
            0,
            "a one-waypoint route wraps to itself"
        );
    }

    #[test]
    fn the_nearest_creature_in_range_is_targeted() {
        let mut world = a_world_on_open_ground();
        world.see_creature(AgentId(2), Position::new(104, 100, 7));
        world.see_creature(AgentId(3), Position::new(101, 100, 7));
        let mut brain = a_brain();

        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::SetTarget {
                agent_id: Some(AgentId(3)),
                seq: 1
            })
        );
    }

    #[test]
    fn engage_radius_is_inclusive_at_its_boundary() {
        let mut world = a_world_on_open_ground();
        world.see_creature(AgentId(2), Position::new(105, 100, 7));
        let mut brain = a_brain();

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::SetTarget {
                agent_id: Some(AgentId(2)),
                ..
            })
        ));
    }

    #[test]
    fn one_tile_past_the_engage_radius_is_ignored() {
        let mut world = a_world_on_open_ground();
        world.see_creature(AgentId(2), Position::new(106, 100, 7));
        let mut brain = a_brain();

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::MovePlayer { .. })
        ));
    }

    #[test]
    fn a_bot_shaped_name_is_never_targeted() {
        let mut world = a_world_on_open_ground();
        world.see_named_creature(AgentId(9), Position::new(101, 100, 7), "Loadbot Zzz");
        let mut brain = a_brain();

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::MovePlayer { .. })
        ));
    }

    #[test]
    fn a_held_target_out_of_melee_range_is_approached() {
        let mut world = a_world_on_open_ground();
        world.see_creature(AgentId(3), Position::new(100, 102, 7));
        let mut brain = a_brain();
        brain.set_target(AgentId(3));

        let action = brain.decide(&world, Instant::now());

        assert_eq!(
            action,
            Some(ClientMessage::MovePlayer {
                direction: Direction::South
            })
        );
    }

    #[test]
    fn a_bot_stands_and_fights_rather_than_travel_while_a_target_is_held() {
        let world = engaged_world();
        let mut brain = a_brain();
        brain.set_target(AgentId(3));

        assert_eq!(
            brain.decide(&world, Instant::now()),
            None,
            "adjacent to its target, with no attack spell known, the bot must stand still"
        );
    }

    #[test]
    fn an_in_range_target_is_attacked_with_the_configured_spell() {
        let mut world = engaged_world();
        world.knows_spell(SpellId(9), "exori flam", SpellGroup::Attack);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));

        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CastSpell {
                spell_id: SpellId(9),
                target: SpellTarget::Agent(AgentId(3)),
                param: None,
            })
        );
    }

    #[test]
    fn a_heal_spell_targets_no_one() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        world.knows_spell(SpellId(1), "exura", SpellGroup::Healing);
        let mut brain = a_brain();

        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CastSpell {
                spell_id: SpellId(1),
                target: SpellTarget::None,
                param: None,
            })
        );
    }

    #[test]
    fn low_life_drinks_a_health_potion_before_anything_else() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        world.carry(ItemId(266), 5);
        let mut brain = a_brain();

        let action = brain.decide(&world, Instant::now()).unwrap();

        assert!(matches!(
            action,
            ClientMessage::UseItemWith { ref source, target_agent: Some(id), .. }
                if source.item_id == ItemId(266) && id == world.agent_id
        ));
    }

    #[test]
    fn drinking_beats_healing_when_both_are_available() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        world.carry(ItemId(266), 5);
        world.knows_spell(SpellId(1), "exura", SpellGroup::Healing);
        let mut brain = a_brain();

        let action = brain.decide(&world, Instant::now()).unwrap();

        assert!(
            matches!(action, ClientMessage::UseItemWith { .. }),
            "the potion must win over the spell when both remedies are available: {action:?}"
        );
    }

    #[test]
    fn a_potion_is_addressed_by_the_carried_search_coordinate() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        world.carry(ItemId(266), 5);
        let mut brain = a_brain();

        let Some(ClientMessage::UseItemWith { source, .. }) = brain.decide(&world, Instant::now())
        else {
            panic!("expected a drink")
        };

        assert_eq!(source.position.x, INVENTORY_COORD_FLAG);
        assert_eq!(source.position.y, CARRIED_SEARCH_FLAG);
    }

    #[test]
    fn an_empty_bag_falls_through_to_the_heal_spell() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        world.carry(ItemId(266), 0);
        world.knows_spell(SpellId(1), "exura", SpellGroup::Healing);
        let mut brain = a_brain();

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CastSpell {
                spell_id: SpellId(1),
                ..
            })
        ));
    }

    #[test]
    fn a_dry_health_potion_is_reported_once_per_bot() {
        let mut world = engaged_world();
        world.set_life(100, 500);
        let mut brain = a_brain();

        brain.decide(&world, Instant::now());
        brain.decide(&world, Instant::now());
        assert_eq!(brain.take_dry_events(), vec![DryPotion::Health]);

        brain.decide(&world, Instant::now());
        assert_eq!(brain.take_dry_events(), vec![]);
    }

    #[test]
    fn low_mana_drinks_a_mana_potion_when_life_is_fine() {
        let mut world = engaged_world();
        world.set_mana(50, 400);
        world.carry(ItemId(268), 5);
        let mut brain = a_brain();

        assert!(matches!(
            brain.decide(&world, Instant::now()).unwrap(),
            ClientMessage::UseItemWith { ref source, .. } if source.item_id == ItemId(268)
        ));
    }

    #[test]
    fn a_spell_on_cooldown_is_not_cast() {
        let mut world = engaged_world();
        world.knows_spell(SpellId(5), "exori flam", SpellGroup::Attack);
        let now = Instant::now();
        world.start_cooldown(SpellId(5), SpellGroup::Attack, now, 2000, 2000);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));

        assert_eq!(
            brain.decide(&world, now + Duration::from_millis(500)),
            None,
            "adjacent, on cooldown, no route to travel while a target is held: nothing to do"
        );
    }

    #[test]
    fn missing_spells_names_what_the_bot_cannot_cast() {
        let world = a_world_on_open_ground();
        let brain = a_brain();

        let missing = brain.missing_spells(&world);
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"exura".to_string()));
        assert!(missing.contains(&"exori flam".to_string()));
    }

    #[test]
    fn a_known_spell_is_not_reported_missing() {
        let mut world = a_world_on_open_ground();
        world.knows_spell(SpellId(1), "exura", SpellGroup::Healing);
        world.knows_spell(SpellId(2), "exori flam", SpellGroup::Attack);
        let brain = a_brain();

        assert!(brain.missing_spells(&world).is_empty());
    }

    #[test]
    fn a_target_lost_for_the_current_target_is_immediately_reacquired_with_a_new_seq() {
        let mut world = engaged_world();
        let mut brain = a_brain();

        let first = brain.decide(&world, Instant::now());
        assert_eq!(
            first,
            Some(ClientMessage::SetTarget {
                agent_id: Some(AgentId(3)),
                seq: 1
            })
        );

        world.apply(&ServerMessage::TargetLost { seq: 1 });

        let second = brain.decide(&world, Instant::now());
        assert_eq!(
            second,
            Some(ClientMessage::SetTarget {
                agent_id: Some(AgentId(3)),
                seq: 2
            })
        );
    }

    #[test]
    fn a_stale_target_lost_does_not_cancel_a_newer_target() {
        let mut world = engaged_world();
        world.knows_spell(SpellId(9), "exori flam", SpellGroup::Attack);
        let mut brain = a_brain();

        brain.decide(&world, Instant::now()); // seq 1 -> AgentId(3)

        world.remove_agent(AgentId(3));
        world.see_creature(AgentId(4), Position::new(101, 100, 7));
        brain.decide(&world, Instant::now()); // (3) is gone; (4) acquired as seq 2

        world.apply(&ServerMessage::TargetLost { seq: 1 }); // stale: names the old target

        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CastSpell {
                spell_id: SpellId(9),
                target: SpellTarget::Agent(AgentId(4)),
                param: None,
            })
        );
    }

    #[test]
    fn a_dead_targets_kill_is_reported_as_target_lost_then_remove_agent() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();

        brain.decide(&world, Instant::now()); // seq 1 -> AgentId(3)

        // the server's own order: TargetLost for the killer, then RemoveAgent
        world.apply(&ServerMessage::TargetLost { seq: 1 });
        world.remove_agent(AgentId(3));
        let corpse = Position::new(101, 100, 7);
        world.set_tile(corpse.clone(), stack(&[ItemId(1), ItemId(CORPSE_ID)]));

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::UseItem { ref item }) if item.position == corpse
        ));
    }

    #[test]
    fn a_corpse_that_appears_a_tick_later_is_still_looted() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));
        brain.decide(&world, Instant::now());

        world.remove_agent(AgentId(3));
        let before_the_tile_updates = brain.decide(&world, Instant::now());
        assert!(!matches!(
            before_the_tile_updates,
            Some(ClientMessage::UseItem { .. })
        ));

        let corpse = Position::new(101, 100, 7);
        world.set_tile(corpse.clone(), stack(&[ItemId(1), ItemId(CORPSE_ID)]));

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::UseItem { ref item }) if item.position == corpse
        ));
    }

    #[test]
    fn a_corpse_confirmed_after_the_grace_window_is_not_looted() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));
        let now = Instant::now();
        brain.decide(&world, now);

        world.remove_agent(AgentId(3));
        let death_detected_at = now + Duration::from_millis(1);
        brain.decide(&world, death_detected_at); // Awaiting starts its own clock here

        let too_late = death_detected_at + CORPSE_ABANDON + Duration::from_millis(1);
        brain.decide(&world, too_late); // abandoned: no container ever confirmed in time; may also send an unrelated travel step, which paces the next one

        world.set_tile(
            Position::new(101, 100, 7),
            stack(&[ItemId(1), ItemId(CORPSE_ID)]),
        );

        assert!(matches!(
            brain.decide(&world, too_late + Duration::from_secs(1)),
            Some(ClientMessage::MovePlayer { .. })
        ));
    }

    #[test]
    fn without_a_backpack_a_dead_targets_corpse_is_left_alone() {
        let mut world = engaged_world();
        let mut brain = a_brain();
        brain.set_target(AgentId(3));
        brain.decide(&world, Instant::now());

        world.remove_agent(AgentId(3));
        world.set_tile(
            Position::new(101, 100, 7),
            stack(&[ItemId(1), ItemId(CORPSE_ID)]),
        );

        assert_eq!(world.backpack(), None);
        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::MovePlayer {
                direction: Direction::East
            }),
            "no backpack means no loot at all; the bot falls through to travel"
        );
    }

    #[test]
    fn a_container_that_was_never_the_targets_corpse_is_not_looted() {
        let mut world = a_world_on_open_ground();
        world.mark_carried(A_BACKPACK);
        world.set_tile(
            Position::new(101, 100, 7),
            stack(&[ItemId(1), ItemId(CORPSE_ID)]),
        );
        let mut brain = a_brain();

        assert!(!matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::UseItem { .. })
        ));
    }

    #[test]
    fn the_corpse_use_item_is_retried_at_most_once() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));
        brain.decide(&world, Instant::now());

        world.remove_agent(AgentId(3));
        let corpse = Position::new(101, 100, 7);
        world.set_tile(corpse.clone(), stack(&[ItemId(1), ItemId(CORPSE_ID)]));

        let first = brain.decide(&world, Instant::now());
        assert!(matches!(first, Some(ClientMessage::UseItem { .. })));

        let second = brain.decide(&world, Instant::now());
        assert!(
            matches!(second, Some(ClientMessage::UseItem { .. })),
            "one retry is allowed"
        );

        let third = brain.decide(&world, Instant::now());
        assert!(
            !matches!(third, Some(ClientMessage::UseItem { .. })),
            "no further resends until corpse_opened or the deadline: {third:?}"
        );
    }

    #[test]
    fn the_use_item_open_container_chain_leads_to_looting() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.set_target(AgentId(3));
        brain.decide(&world, Instant::now());

        let corpse = Position::new(101, 100, 7);
        world.remove_agent(AgentId(3));
        world.set_tile(corpse.clone(), stack(&[ItemId(1), ItemId(CORPSE_ID)]));

        let opened = brain.decide(&world, Instant::now());
        assert!(matches!(opened, Some(ClientMessage::UseItem { .. })));

        world.apply(&ServerMessage::OpenContainer {
            container_id: ContainerId(7),
            capacity: 20,
            has_parent: false,
            title: "a corpse".to_string(),
            items: vec![Some((ItemId(3031), 1))].into_boxed_slice(),
        });
        brain.corpse_opened(ContainerId(7));

        assert!(matches!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::MoveItem { .. })
        ));
    }

    #[test]
    fn an_emptied_corpse_is_closed() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.opened_corpse(ContainerId(1));
        world.open_container(
            ContainerId(1),
            &[Some((ItemId(3031), 20)), Some((ItemId(3264), 1))],
        );

        let first = brain.decide(&world, Instant::now());
        assert!(matches!(first, Some(ClientMessage::MoveItem { .. })));

        world.open_container(ContainerId(1), &[]);
        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CloseContainer {
                container_id: ContainerId(1)
            })
        );
    }

    #[test]
    fn looting_stops_at_the_configured_limit_even_with_items_left() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.opened_corpse(ContainerId(1));
        world.open_container(
            ContainerId(1),
            &[
                Some((ItemId(3031), 1)),
                Some((ItemId(3032), 1)),
                Some((ItemId(3033), 1)),
                Some((ItemId(3034), 1)),
                Some((ItemId(3035), 1)),
            ],
        );

        for _ in 0..Behaviour::test_default().loot_items_max {
            assert!(matches!(
                brain.decide(&world, Instant::now()),
                Some(ClientMessage::MoveItem { .. })
            ));
        }

        assert_eq!(
            brain.decide(&world, Instant::now()),
            Some(ClientMessage::CloseContainer {
                container_id: ContainerId(1)
            }),
            "the container still has an item left, but the limit was reached"
        );
    }

    #[test]
    fn a_non_cumulative_item_is_looted_as_a_single_unit() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.opened_corpse(ContainerId(1));
        world.open_container(ContainerId(1), &[Some((ItemId(9999), 5))]);

        let Some(ClientMessage::MoveItem { amount, .. }) = brain.decide(&world, Instant::now())
        else {
            panic!("expected a loot move");
        };

        assert_eq!(
            amount, 1,
            "the byte in the container slot may be a fluid id, not a count"
        );
    }

    #[test]
    fn a_cumulative_item_keeps_its_real_amount() {
        let mut world = engaged_world();
        world.mark_carried(A_BACKPACK);
        let mut brain = a_brain();
        brain.opened_corpse(ContainerId(1));
        world.open_container(ContainerId(1), &[Some((ItemId(STACKABLE_ID), 20))]);

        let Some(ClientMessage::MoveItem { amount, .. }) = brain.decide(&world, Instant::now())
        else {
            panic!("expected a loot move");
        };

        assert_eq!(amount, 20);
    }
}

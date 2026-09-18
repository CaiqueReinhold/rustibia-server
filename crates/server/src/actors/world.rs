use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use std::collections::binary_heap::BinaryHeap;

use std::{sync::Arc, time::Duration};
use strum::Display;
use tokio::{
    select,
    sync::mpsc,
    sync::{oneshot, watch},
    time,
};
use tracing::{debug, error, info, warn};

use crate::actors::{
    message_router::{MessageRouterActorHandle, MessageRouterGuard},
    session::SessionActorHandle,
};
use crate::config::CONFIG;
use crate::entities::{
    agent::{Agent, AgentKey, Facing},
    combat::{CombatDamage, CombatElement},
    creature::{CreatureAbilityId, CreatureKind},
    items::ItemRef,
    map::GameMap,
    position::{Direction, ItemPlacement, Position},
    spells::{ChainAttack, SpellDelivery, SpellId},
    targeting::AreaTarget,
};
use crate::game::{
    Tick, TickCtx, TickDelta, chat, conditions, config::GAME_CONFIG, creature_abilities,
    creature_behavior::CreatureAction, events, events::BroadcastMessage, item_action,
    item_movement, item_multi_action, item_multi_action::UseTarget, movement, random::Rolls,
    spells, support, systems, targeting,
};
use crate::online_registry::RegistryGuard;
use crate::persistence::{creatures::CREATURE_KINDS, spawns::SpawnPoint, spells::SPELLS};

#[derive(Debug, Display)]
pub enum WorldCommand {
    SpawnPlayer {
        player: Box<Agent>,
        session: SessionActorHandle,
        tx: oneshot::Sender<(AgentKey, MessageRouterGuard)>,
    },
    Walk {
        agent_key: AgentKey,
        direction: Direction,
    },
    MoveItem {
        agent: AgentKey,
        source: ItemRef,
        amount: u8,
        to: ItemPlacement,
    },
    UseItem {
        agent: AgentKey,
        item: ItemRef,
    },
    UseItemWith {
        agent: AgentKey,
        source: ItemRef,
        target: UseTarget,
    },
    ChangeDirection {
        agent: AgentKey,
        facing: Facing,
    },
    DespawnPlayer {
        agent_key: AgentKey,
        registry: Option<RegistryGuard>,
        give_up_at: Option<Tick>,
    },
    SpawnCreature {
        kind: Arc<CreatureKind>,
        position: Position,
        respawn_ticks: Option<TickDelta>,
    },
    RequestLogout {
        agent_key: AgentKey,
    },
    DecayItem {
        item: ItemRef,
    },
    Say {
        agent_key: AgentKey,
        message: String,
    },
    SetTarget {
        agent_key: AgentKey,
        target: Option<AgentKey>,
        seq: u32,
    },
    CastSpell {
        agent_key: AgentKey,
        spell: SpellId,
        target: AreaTarget,
        param: Option<String>,
    },
    CastAbility {
        agent_key: AgentKey,
        ability_id: CreatureAbilityId,
    },
    ChainAttack {
        attacker: AgentKey,
        sources: Vec<(Position, CombatDamage)>,
        chain: ChainAttack,
        targeted: Vec<AgentKey>,
    },
    RegeneratePlayer {
        agent_key: AgentKey,
        generation: u32,
    },
    DamageOverTimeTick {
        agent_key: AgentKey,
        element: CombatElement,
        generation: u32,
    },
    SpeedExpired {
        agent_key: AgentKey,
        generation: u32,
    },
}

impl WorldCommand {
    pub fn from_creature_action(action: CreatureAction) -> Self {
        match action {
            CreatureAction::Say { agent_key, message } => WorldCommand::Say { agent_key, message },
            CreatureAction::SetTarget { agent_key, target } => WorldCommand::SetTarget {
                agent_key,
                target,
                seq: 0,
            },
            CreatureAction::Walk {
                agent_key,
                direction,
            } => WorldCommand::Walk {
                agent_key,
                direction,
            },
            CreatureAction::CastAbility {
                agent_key,
                ability_id,
            } => WorldCommand::CastAbility {
                agent_key,
                ability_id,
            },
        }
    }
}

#[derive(Debug)]
pub struct ScheduledCommand {
    pub at_tick: Tick,
    pub command: WorldCommand,
}

impl PartialEq for ScheduledCommand {
    fn eq(&self, other: &Self) -> bool {
        self.at_tick == other.at_tick
    }
}

impl Eq for ScheduledCommand {}

impl PartialOrd for ScheduledCommand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledCommand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.at_tick > other.at_tick {
            std::cmp::Ordering::Less
        } else if self.at_tick < other.at_tick {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorldActorHandle {
    tx: mpsc::Sender<(WorldCommand, Option<TickDelta>)>,
}

impl WorldActorHandle {
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<(WorldCommand, Option<TickDelta>)>) {
        let (tx, rx) = mpsc::channel(64);
        (Self { tx }, rx)
    }

    pub async fn send(&self, command: WorldCommand) {
        let _ = self.tx.send((command, None)).await;
    }

    pub async fn send_delayed(&self, command: WorldCommand, after: TickDelta) {
        let _ = self.tx.send((command, Some(after))).await;
    }

    pub async fn spawn_player(
        &self,
        player: Agent,
        session: SessionActorHandle,
    ) -> Result<(AgentKey, MessageRouterGuard)> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send((
                WorldCommand::SpawnPlayer {
                    player: Box::new(player),
                    session,
                    tx,
                },
                None,
            ))
            .await;
        Ok(rx.await?)
    }
}

pub struct WorldActor {
    rx: mpsc::Receiver<(WorldCommand, Option<TickDelta>)>,
    message_router: MessageRouterActorHandle,
    command_queue: BinaryHeap<ScheduledCommand>,
    map: GameMap,
    shared_map: Arc<ArcSwap<GameMap>>,
    tick: Tick,
    tick_duration: Duration,
    tick_tx: watch::Sender<Tick>,
    roll: Rolls,
}

impl WorldActor {
    pub fn start(
        map: GameMap,
        shared_map: Arc<ArcSwap<GameMap>>,
        message_router: MessageRouterActorHandle,
        seed: u64,
        spawns: &[SpawnPoint],
    ) -> (WorldActorHandle, watch::Receiver<Tick>) {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);
        let (tick_tx, tick_rx) = watch::channel(Tick(0));

        let mut actor = Self {
            rx,
            message_router,
            command_queue: BinaryHeap::with_capacity(CONFIG.max_queue_size),
            map,
            shared_map,
            tick: Tick(0),
            tick_duration: CONFIG.tick_duration,
            tick_tx,
            roll: Rolls::new(seed),
        };
        actor.seed_spawn_points(spawns);

        tokio::spawn(actor.run());

        (WorldActorHandle { tx }, tick_rx)
    }

    fn seed_spawn_points(&mut self, spawns: &[SpawnPoint]) {
        for spawn in spawns {
            let Some(kind) = CREATURE_KINDS.get(&spawn.kind).cloned() else {
                error!(
                    "Unknown creature kind '{}' for the spawn at {:?}",
                    spawn.kind, spawn.position
                );
                continue;
            };
            self.command_queue.push(ScheduledCommand {
                at_tick: Tick(1),
                command: WorldCommand::SpawnCreature {
                    kind,
                    position: spawn.position.clone(),
                    respawn_ticks: Some(spawn.respawn_ticks),
                },
            });
        }
        info!("Seeded {} spawn points", self.command_queue.len());
    }

    pub async fn run(mut self) {
        let mut ticker = time::interval(self.tick_duration);

        info!("Starting world loop");
        loop {
            debug!("World: receiving messages");
            loop {
                select! {
                    biased;
                    _ = ticker.tick() => {
                        break
                    },
                    Some((command, after)) = self.rx.recv() => {
                        if let Some(after) = after {
                            self.command_queue.push(ScheduledCommand { at_tick: self.tick + after, command });
                        } else {
                            self.command_queue.push(ScheduledCommand { at_tick: self.tick + TickDelta(1), command });
                        }

                    }
                }
            }

            let tick_start = time::Instant::now();
            self.tick += TickDelta(1);
            debug!("World: starting tick {}", self.tick);

            let mut broadcast_messages: Vec<BroadcastMessage> = Vec::new();

            if !self.command_queue.is_empty() {
                info!(
                    "Starting tick {} with {} commands",
                    self.tick,
                    self.command_queue.len()
                );
            }

            while let Some(scheduled) = self.command_queue.peek() {
                if scheduled.at_tick <= self.tick {
                    let scheduled = self.command_queue.pop().unwrap();
                    self.handle_command(scheduled.command, &mut broadcast_messages);
                } else {
                    break;
                }
            }

            self.run_systems(&mut broadcast_messages);

            self.end_tick(broadcast_messages).await;

            let elapsed = tick_start.elapsed();
            debug!("Tick {} took {} ms", self.tick, elapsed.as_millis());
            if elapsed > self.tick_duration {
                warn!(
                    "Tick {} overran budget by {:?}",
                    self.tick,
                    elapsed - self.tick_duration
                );
            }
        }
    }

    async fn end_tick(&mut self, mut broadcast_messages: Vec<BroadcastMessage>) {
        events::dedupe_refreshes(&mut broadcast_messages);

        let snapshot = self.map.clone();
        self.shared_map.store(Arc::new(snapshot));
        let _ = self.tick_tx.send(self.tick);
        self.message_router.broadcast(broadcast_messages).await;
    }

    /// Lends the tick's five writable things to `game/` functions as one `TickCtx`, then queues
    /// whatever they scheduled.
    fn with_ctx<T>(
        &mut self,
        broadcast_messages: &mut Vec<BroadcastMessage>,
        f: impl FnOnce(&mut TickCtx) -> T,
    ) -> T {
        let mut scheduled = Vec::new();
        let result = f(&mut TickCtx {
            map: &mut self.map,
            events: broadcast_messages,
            scheduled: &mut scheduled,
            roll: &mut self.roll,
            tick: self.tick,
        });
        for cmd in scheduled {
            self.command_queue.push(cmd);
        }
        result
    }

    fn run_systems(&mut self, broadcast_messages: &mut Vec<BroadcastMessage>) {
        self.with_ctx(broadcast_messages, |ctx| {
            systems::combat_system(ctx);
        });
    }

    fn handle_command(
        &mut self,
        command: WorldCommand,
        broadcast_messages: &mut Vec<BroadcastMessage>,
    ) {
        info!("Executing command: {}", command);
        match command {
            WorldCommand::SpawnPlayer {
                player,
                session,
                tx,
            } => {
                if let Err(e) = self.spawn_player(*player, session, tx, broadcast_messages) {
                    error!("Error spawning player: {e}");
                }
            }
            WorldCommand::Walk {
                direction,
                agent_key,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    movement::walk(ctx, direction, agent_key)
                });
            }
            WorldCommand::MoveItem {
                agent,
                source,
                amount,
                to,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    item_movement::move_item(ctx, agent, source, amount, to)
                });
            }
            WorldCommand::UseItem { agent, item } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    item_action::use_item(ctx, agent, item)
                });
            }
            WorldCommand::UseItemWith {
                agent,
                source,
                target,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    item_multi_action::use_item_with(ctx, agent, source, target)
                });
            }
            WorldCommand::ChangeDirection { agent, facing } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    movement::change_direction(ctx, agent, facing)
                });
            }
            WorldCommand::SetTarget {
                agent_key,
                target,
                seq,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    targeting::set_target(ctx, agent_key, target, seq)
                });
            }
            WorldCommand::DespawnPlayer {
                agent_key,
                registry,
                give_up_at,
            } => {
                let give_up_at =
                    give_up_at.unwrap_or(self.tick + GAME_CONFIG.disconnect_linger_cap_ticks);
                let blocked = self
                    .map
                    .get_agent(agent_key)
                    .is_some_and(|agent| agent.conditions().is_logout_blocked(self.tick));

                if blocked && self.tick < give_up_at {
                    self.command_queue.push(ScheduledCommand {
                        at_tick: self.tick + TickDelta(1),
                        command: WorldCommand::DespawnPlayer {
                            agent_key,
                            registry,
                            give_up_at: Some(give_up_at),
                        },
                    });
                } else if let Some((_, position)) = self.map.remove_agent(agent_key) {
                    info!("Player {:?} despawned after disconnect", agent_key);
                    broadcast_messages.push(BroadcastMessage::AgentDespawned {
                        agent_key,
                        snapshot: None,
                        position,
                    });
                }
            }
            WorldCommand::SpawnCreature {
                kind,
                position,
                respawn_ticks,
            } => {
                let agent = match respawn_ticks {
                    Some(ticks) => Agent::respawning(kind, position.clone(), ticks),
                    None => Agent::from_creature_kind(kind, position.clone()),
                };
                match self.map.insert_agent(agent, &position) {
                    Ok(agent_key) => {
                        broadcast_messages.push(BroadcastMessage::PlayerSpawned {
                            agent_key,
                            position: position.clone(),
                        });
                    }
                    Err(e) => {
                        error!("Failed to spawn creature at {:?}: {:?}", position, e);
                    }
                };
            }
            WorldCommand::RequestLogout { agent_key } => {
                if let Err(e) = self.handle_request_logout(agent_key, broadcast_messages) {
                    error!("Failed to logout player {agent_key:?}: {e}");
                }
            }
            WorldCommand::DecayItem { item } => {
                self.with_ctx(broadcast_messages, |ctx| item_action::decay_item(ctx, item));
            }
            WorldCommand::Say { agent_key, message } => {
                self.with_ctx(broadcast_messages, |ctx| chat::say(ctx, agent_key, message));
            }
            WorldCommand::CastSpell {
                agent_key,
                spell,
                target,
                param,
            } => {
                self.with_ctx(broadcast_messages, |ctx| match SPELLS.get(&spell) {
                    Some(spell) => {
                        let _ = spells::cast_spell(
                            ctx,
                            agent_key,
                            spell,
                            target,
                            param,
                            spells::CastSource::Words,
                        );
                    }
                    None => {
                        let Some(position) = ctx.map.agent_position(agent_key).cloned() else {
                            return;
                        };
                        ctx.events.push(BroadcastMessage::SpellDenied {
                            agent_key,
                            position,
                            reason: spells::SpellCastingDenyReason::IdNotFound,
                            delivery: SpellDelivery::Words,
                        });
                    }
                });
            }
            WorldCommand::CastAbility {
                agent_key,
                ability_id,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    creature_abilities::cast_ability(ctx, agent_key, ability_id)
                });
            }
            WorldCommand::ChainAttack {
                attacker,
                sources,
                chain,
                targeted,
            } => {
                self.with_ctx(broadcast_messages, |ctx| {
                    spells::chain_attack(ctx, attacker, sources, chain, targeted)
                });
            }
            WorldCommand::RegeneratePlayer {
                agent_key,
                generation,
            } => self.with_ctx(broadcast_messages, |ctx| {
                conditions::regenerate_life_mana(ctx, agent_key, generation)
            }),
            WorldCommand::DamageOverTimeTick {
                agent_key,
                element,
                generation,
            } => self.with_ctx(broadcast_messages, |ctx| {
                conditions::tick_damage_over_time(ctx, agent_key, element, generation)
            }),
            WorldCommand::SpeedExpired {
                agent_key,
                generation,
            } => self.with_ctx(broadcast_messages, |ctx| {
                support::expire_speed(ctx, agent_key, generation)
            }),
        };
    }

    fn handle_request_logout(
        &mut self,
        agent_key: AgentKey,
        broadcast_messages: &mut Vec<BroadcastMessage>,
    ) -> Result<()> {
        let Some(agent) = self.map.get_agent(agent_key) else {
            return Ok(());
        };

        if agent.conditions().is_logout_blocked(self.tick) {
            broadcast_messages.push(BroadcastMessage::LogoutDenied { agent_key });
            return Ok(());
        }

        if !agent.can_logout(self.tick) {
            let next_tick = agent.next_walk_tick;
            self.command_queue.push(ScheduledCommand {
                at_tick: next_tick,
                command: WorldCommand::RequestLogout { agent_key },
            });
            broadcast_messages.push(BroadcastMessage::LogoutDenied { agent_key });
            return Ok(());
        }

        let position = self.map.agent_position(agent_key).cloned();
        let snapshot = position.clone().and_then(|pos| agent.to_snapshot(pos));
        self.map.remove_agent(agent_key);
        broadcast_messages.push(BroadcastMessage::AgentDespawned {
            agent_key,
            snapshot,
            position: position.unwrap_or_default(),
        });
        Ok(())
    }

    fn spawn_player(
        &mut self,
        agent: Agent,
        session: SessionActorHandle,
        tx: oneshot::Sender<(AgentKey, MessageRouterGuard)>,
        broadcast_messages: &mut Vec<BroadcastMessage>,
    ) -> Result<()> {
        let player = agent
            .get_player()
            .ok_or(anyhow!("Agent {:?} is not a player", agent))?;
        let origin = agent.get_origin().clone();
        let position = player.last_logout_position().clone();

        let agent_key = self
            .map
            .insert_agent(agent.clone(), &position)
            .or_else(|_| self.map.insert_agent(agent, &origin))?;

        let Some(spawn_pos) = self.map.agent_position(agent_key).cloned() else {
            let agent = self.map.remove_agent(agent_key);
            return Err(anyhow!("Player {:?} failed to spawn", agent));
        };

        let guard = self.message_router.subscribe(agent_key, session)?;

        if tx.send((agent_key, guard)).is_err() {
            self.map.remove_agent(agent_key);
            return Err(anyhow!("Failed to return spawned player result"));
        }

        broadcast_messages.push(BroadcastMessage::PlayerSpawned {
            agent_key,
            position: spawn_pos,
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::message_router::MessageRouterCommand;
    use crate::entities::combat::{CombatDamage, CombatElement};
    use crate::entities::inventory::InventorySlot;
    use crate::entities::map::MapTile;
    use crate::persistence::test_fixtures::{
        a_creature_kind, a_player_with_a_full_backpack, a_test_creature, a_test_snapshot,
    };

    /// Builds a `WorldActor` from bare fields, the same way `SessionActorHandle::for_test`
    /// (session.rs) fabricates a channel-backed handle for tests. The `rx` half of the
    /// command channel and the `message_router`/`tick_tx` handles are never driven; only
    /// `handle_command` is called directly.
    fn a_test_world_actor(map: GameMap) -> WorldActor {
        let (_tx, rx) = mpsc::channel(1);
        let (message_router, _router_rx) = MessageRouterActorHandle::for_test();
        let (tick_tx, _tick_rx) = watch::channel(Tick(0));
        WorldActor {
            rx,
            message_router,
            command_queue: BinaryHeap::new(),
            map,
            shared_map: Arc::new(ArcSwap::from_pointee(GameMap::new())),
            tick: Tick(0),
            tick_duration: Duration::from_millis(50),
            tick_tx,
            roll: Rolls::new(1),
        }
    }

    fn a_spawn_point(kind: &str, position: Position, respawn_ticks: TickDelta) -> SpawnPoint {
        SpawnPoint {
            position,
            kind: kind.to_string(),
            respawn_ticks,
        }
    }

    /// Nothing tracks spawn slots any more, so the table has exactly one effect: it seeds the
    /// first population. A kind the catalogue does not carry is dropped rather than retried.
    #[test]
    fn the_spawn_table_is_queued_for_the_first_tick_and_unknown_kinds_are_dropped() {
        let mut actor = a_test_world_actor(GameMap::new());
        actor.command_queue.clear();

        actor.seed_spawn_points(&[
            a_spawn_point("elf", Position::new(10, 10, 7), TickDelta(600)),
            a_spawn_point("nosuchcreature", Position::new(11, 10, 7), TickDelta(600)),
        ]);

        assert_eq!(actor.command_queue.len(), 1);
        let queued = actor.command_queue.pop().unwrap();
        assert_eq!(queued.at_tick, Tick(1));
        assert!(matches!(
            queued.command,
            WorldCommand::SpawnCreature {
                respawn_ticks: Some(TickDelta(600)),
                ..
            }
        ));
    }

    /// The delay has to survive the dispatch onto the agent, because that is the only copy of
    /// it left once the spawn table has been read: `death::reap` reads it back off the corpse.
    #[tokio::test]
    async fn a_spawned_creature_carries_the_delay_that_will_replace_it() {
        let pos = Position::new(10, 10, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let mut actor = a_test_world_actor(map);

        actor.handle_command(
            WorldCommand::SpawnCreature {
                kind: Arc::new(a_creature_kind("rat")),
                position: pos.clone(),
                respawn_ticks: Some(TickDelta(600)),
            },
            &mut Vec::new(),
        );

        let (_, agent) = actor.map.iter_agents().next().expect("nothing spawned");
        assert_eq!(agent.respawn_ticks(), Some(TickDelta(600)));
        assert_eq!(*agent.get_origin(), pos);
    }

    /// Pins the call site rather than the collapsing itself — `dedupe_refreshes` has its
    /// own tests in `game::events`. Verified by hand: dropping the call from `end_tick`
    /// makes this fail with `left: 3, right: 2`.
    #[tokio::test]
    async fn the_tick_hands_the_router_one_refresh_per_tile() {
        let mut actor = a_test_world_actor(GameMap::new());
        let (message_router, mut router_rx) = MessageRouterActorHandle::for_test();
        actor.message_router = message_router;
        let position = Position::new(10, 10, 7);

        actor
            .end_tick(vec![
                BroadcastMessage::TileChanged {
                    position: position.clone(),
                },
                BroadcastMessage::DamageTaken {
                    source: None,
                    target: AgentKey::default(),
                    position: position.clone(),
                    blood_type: None,
                    damage: CombatDamage {
                        element: CombatElement::Physical,
                        value: 5,
                        blocked_shield: false,
                        blocked_armor: false,
                    },
                },
                BroadcastMessage::TileChanged {
                    position: position.clone(),
                },
            ])
            .await;

        let Some(MessageRouterCommand::Broadcast { messages }) = router_rx.recv().await else {
            panic!("the tick did not broadcast");
        };
        assert_eq!(messages.len(), 2, "{messages:?}");
    }

    /// Goes through the real `WorldCommand::SetTarget` dispatch arm in
    /// `handle_command`, not `game::targeting::set_target` directly. Verified by
    /// hand: gutting the dispatch arm to a no-op `Ok(())` makes this test fail on
    /// `assert_eq!(actor.map.get_agent(attacker).unwrap().target(), Some(victim))`
    /// (left: `None`, right: `Some(victim)`); restoring the arm makes it pass again.
    #[test]
    fn a_blocked_disconnect_re_queues_instead_of_despawning() {
        let mut map = GameMap::new();
        let position = Position::new(5, 5, 7);
        map.insert_tile(position.clone(), MapTile::new());
        let agent_key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &position)
            .unwrap();
        map.get_agent_mut(agent_key)
            .unwrap()
            .conditions_mut()
            .reset_logout_block(Tick(0));

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();
        actor.handle_command(
            WorldCommand::DespawnPlayer {
                agent_key,
                registry: None,
                give_up_at: None,
            },
            &mut broadcasts,
        );

        assert!(actor.map.get_agent(agent_key).is_some());
        assert_eq!(actor.command_queue.len(), 1);
        assert!(broadcasts.is_empty());
    }

    #[test]
    fn the_linger_cap_despawns_a_still_blocked_player() {
        let mut map = GameMap::new();
        let position = Position::new(5, 5, 7);
        map.insert_tile(position.clone(), MapTile::new());
        let agent_key = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &position)
            .unwrap();
        map.get_agent_mut(agent_key)
            .unwrap()
            .conditions_mut()
            .reset_logout_block(Tick(0));

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();
        actor.handle_command(
            WorldCommand::DespawnPlayer {
                agent_key,
                registry: None,
                give_up_at: Some(Tick(0)),
            },
            &mut broadcasts,
        );

        assert!(actor.map.get_agent(agent_key).is_none());
        assert!(actor.command_queue.is_empty());
    }

    #[tokio::test]
    async fn set_target_command_dispatches_through_handle_command() {
        let mut map = GameMap::new();
        let pos = Position::new(5, 5, 7);
        map.insert_tile(pos.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &pos)
            .unwrap();
        let victim = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &pos)
            .unwrap();

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();

        actor.handle_command(
            WorldCommand::SetTarget {
                agent_key: attacker,
                target: Some(victim),
                seq: 0,
            },
            &mut broadcasts,
        );

        assert_eq!(
            actor.map.get_agent(attacker).unwrap().target(),
            Some(victim)
        );
        assert!(
            broadcasts.is_empty(),
            "a successful set is silent; the client already applied it"
        );
    }

    /// The walk applies before the sweep in the same tick, so the drop lands with
    /// the movement rather than a tick behind it.
    #[tokio::test]
    async fn a_target_out_of_view_is_dropped_by_the_loop() {
        let here = Position::new(100, 100, 7);
        let far = Position::new(120, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());
        map.insert_tile(far.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        let victim = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &far)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(victim), 4);

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();

        actor.run_systems(&mut broadcasts);

        assert_eq!(actor.map.get_agent(attacker).unwrap().target(), None);
        assert!(matches!(
            broadcasts.as_slice(),
            [BroadcastMessage::AgentLostTarget { agent_key, seq: 4 }]
                if *agent_key == attacker
        ));
    }

    #[tokio::test]
    async fn a_reachable_target_is_not_dropped_by_the_loop() {
        let here = Position::new(100, 100, 7);
        let next = Position::new(101, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(here.clone(), MapTile::new());
        map.insert_tile(next.clone(), MapTile::new());
        let attacker = map
            .insert_agent(Agent::from_player(a_test_snapshot(1, 1)), &here)
            .unwrap();
        let victim = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &next)
            .unwrap();
        map.get_agent_mut(attacker)
            .unwrap()
            .set_target(Some(victim), 4);

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();

        actor.run_systems(&mut broadcasts);

        assert_eq!(
            actor.map.get_agent(attacker).unwrap().target(),
            Some(victim)
        );
        assert!(
            !broadcasts
                .iter()
                .any(|m| matches!(m, BroadcastMessage::AgentLostTarget { .. }))
        );
    }

    /// The behavioural claim of the whole change: a creature killed earlier in the pass
    /// does not get its swing. Insertion order fixes who goes first — `iter_agents` walks
    /// slots in index order — so `killer` is asked before `victim`.
    #[tokio::test]
    async fn a_creature_killed_this_pass_does_not_swing_back() {
        let a = Position::new(5, 5, 7);
        let b = Position::new(6, 5, 7);
        let mut map = GameMap::new();
        map.insert_tile(a.clone(), MapTile::new());
        map.insert_tile(b.clone(), MapTile::new());
        let killer = map
            .insert_agent(a_test_creature("Killer", 100, (5, 5)), &a)
            .unwrap();
        let victim = map
            .insert_agent(a_test_creature("Victim", 1, (7, 7)), &b)
            .unwrap();
        map.get_agent_mut(killer)
            .unwrap()
            .set_target(Some(victim), 0);
        map.get_agent_mut(victim)
            .unwrap()
            .set_target(Some(killer), 0);

        let mut actor = a_test_world_actor(map);
        let mut broadcasts = Vec::new();

        actor.run_systems(&mut broadcasts);

        assert!(actor.map.get_agent(victim).is_none());
        assert_eq!(actor.map.get_agent(killer).unwrap().life().current, 100);
    }

    /// Covers the publish seam — `shared_map.store(Arc::new(self.map.clone()))` against a
    /// real `WorldActor` and a real `ArcSwap` — not a full tick. No `WorldCommand` is
    /// dispatched; `entities/map.rs`'s tests cover the clone itself.
    #[tokio::test]
    async fn a_published_snapshot_does_not_see_later_inventory_writes() {
        let pos = Position::new(5, 5, 7);
        let mut map = GameMap::new();
        map.insert_tile(pos.clone(), MapTile::new());
        let key = map
            .insert_agent(
                Agent::from_player(a_player_with_a_full_backpack(1, 1)),
                &pos,
            )
            .unwrap();

        let mut actor = a_test_world_actor(map);
        actor.shared_map.store(Arc::new(actor.map.clone()));
        let published = actor.shared_map.load_full();

        actor
            .map
            .get_player_mut(key)
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Backpack);

        assert!(
            published
                .get_player(key)
                .unwrap()
                .inventory()
                .get(&InventorySlot::Backpack)
                .is_some()
        );
        assert!(
            actor
                .map
                .get_player(key)
                .unwrap()
                .inventory()
                .get(&InventorySlot::Backpack)
                .is_none()
        );
    }
}

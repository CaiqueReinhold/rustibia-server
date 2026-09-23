use anyhow::Result;
use arc_swap::ArcSwap;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tracing::{Instrument, info, info_span, warn};

use crate::{
    actors::session::{SessionActorHandle, SessionCommand},
    config::CONFIG,
    entities::{
        agent::AgentKey,
        chat::ChannelId,
        map::GameMap,
        position::{Position, Rect},
        world_delta::WorldDelta,
    },
    game::{
        Tick,
        events::{BroadcastMessage, Routing},
        map_query::iter_visible_floors,
    },
    telemetry,
};

#[derive(Debug)]
pub enum MessageRouterCommand {
    Subscribe {
        agent_key: AgentKey,
        session: SessionActorHandle,
    },
    Unsubscribe {
        agent_key: AgentKey,
    },
    Tick {
        events: Vec<BroadcastMessage>,
        delta: Arc<WorldDelta>,
        tick: Tick,
    },
    DeliverPrivateMessage {
        author: AgentKey,
        recipient: AgentKey,
        message: String,
    },
    DeliverChannelMessage {
        author: AgentKey,
        recipients: Vec<AgentKey>,
        channel_id: ChannelId,
        message: String,
        tx: oneshot::Sender<Vec<AgentKey>>,
    },
}

#[derive(Debug)]
pub struct MessageRouterGuard {
    agent_key: AgentKey,
    handle: MessageRouterActorHandle,
}

#[derive(Clone, Debug)]
pub struct MessageRouterActorHandle {
    tx: mpsc::Sender<MessageRouterCommand>,
}

#[derive(Debug)]
pub struct MessageRouterActor {
    rx: mpsc::Receiver<MessageRouterCommand>,
    shared_map: Arc<ArcSwap<GameMap>>,
    session_map: HashMap<AgentKey, SessionActorHandle>,
}

impl Drop for MessageRouterGuard {
    fn drop(&mut self) {
        self.handle.unsubscribe(self.agent_key);
    }
}

impl MessageRouterActorHandle {
    pub fn subscribe(
        &self,
        agent_key: AgentKey,
        session: SessionActorHandle,
    ) -> Result<MessageRouterGuard> {
        if self
            .tx
            .try_send(MessageRouterCommand::Subscribe { agent_key, session })
            .is_ok()
        {
            return Ok(MessageRouterGuard {
                agent_key,
                handle: MessageRouterActorHandle {
                    tx: self.tx.clone(),
                },
            });
        }

        Err(anyhow::anyhow!("Failed to subscribe"))
    }

    pub fn unsubscribe(&self, agent_key: AgentKey) {
        let _ = self
            .tx
            .try_send(MessageRouterCommand::Unsubscribe { agent_key });
    }

    pub async fn tick(&self, events: Vec<BroadcastMessage>, delta: Arc<WorldDelta>, tick: Tick) {
        let _ = self
            .tx
            .send(MessageRouterCommand::Tick {
                events,
                delta,
                tick,
            })
            .await;
    }

    pub async fn deliver_private_message(
        &self,
        author: AgentKey,
        recipient: AgentKey,
        message: String,
    ) {
        let _ = self
            .tx
            .send(MessageRouterCommand::DeliverPrivateMessage {
                author,
                recipient,
                message,
            })
            .await;
    }

    /// Delivers one channel message to every recipient and returns the keys it could not
    /// reach, so the caller can prune them. Batched on purpose: this actor is also the
    /// fan-out path for every world broadcast, so one round-trip per message keeps chat
    /// traffic off the critical path for movement and tile updates.
    pub async fn deliver_channel_message(
        &self,
        author: AgentKey,
        recipients: Vec<AgentKey>,
        channel_id: ChannelId,
        message: String,
    ) -> Vec<AgentKey> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(MessageRouterCommand::DeliverChannelMessage {
                author,
                recipients,
                channel_id,
                message,
                tx,
            })
            .await
            .is_err()
        {
            warn!("Router is gone; channel message dropped and no members pruned");
            return Vec::new();
        }
        match rx.await {
            Ok(dead) => dead,
            Err(_) => {
                warn!("Router dropped the reply for a channel message; no members pruned");
                Vec::new()
            }
        }
    }

    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<MessageRouterCommand>) {
        let (tx, rx) = mpsc::channel(64);
        (Self { tx }, rx)
    }
}

impl MessageRouterActor {
    pub fn start(shared_map: Arc<ArcSwap<GameMap>>) -> MessageRouterActorHandle {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);
        telemetry::observe_channel("rustibia.router.inbox.depth", &tx);

        tokio::spawn(async move {
            let actor = Self {
                rx,
                shared_map: shared_map.clone(),
                session_map: HashMap::new(),
            };
            actor.run().await;
        });

        MessageRouterActorHandle { tx }
    }

    pub async fn run(mut self) {
        info!("Message router actor started");
        loop {
            let command = self.rx.recv().await;
            match command {
                Some(command) => self.handle_command(command).await,
                None => break,
            }
        }
    }

    async fn handle_command(&mut self, command: MessageRouterCommand) {
        match command {
            MessageRouterCommand::Subscribe { agent_key, session } => {
                self.subscribe(agent_key, session)
            }
            MessageRouterCommand::Unsubscribe { agent_key } => self.unsubscribe(agent_key),
            MessageRouterCommand::Tick {
                events,
                delta,
                tick,
            } => {
                self.tick(events, delta)
                    .instrument(info_span!(parent: None, "route", tick = tick.0 as i64))
                    .await
            }
            MessageRouterCommand::DeliverPrivateMessage {
                author,
                recipient,
                message,
            } => self.deliver_private_message(author, recipient, message),
            MessageRouterCommand::DeliverChannelMessage {
                author,
                recipients,
                channel_id,
                message,
                tx,
            } => {
                let dead = self.deliver_channel_message(author, recipients, channel_id, message);
                let _ = tx.send(dead);
            }
        }
    }

    fn subscribe(&mut self, agent_key: AgentKey, session: SessionActorHandle) {
        if self.session_map.contains_key(&agent_key) {
            return;
        }

        self.session_map.insert(agent_key, session);
    }

    fn unsubscribe(&mut self, agent_key: AgentKey) {
        self.session_map.remove(&agent_key);
    }

    async fn tick(&mut self, events: Vec<BroadcastMessage>, delta: Arc<WorldDelta>) {
        let event_count = events.len();
        self.broadcast(events)
            .instrument(info_span!("broadcast", events = event_count as i64))
            .await;
        telemetry::metrics().record_sessions_active(self.session_map.len() as u64);
        if delta.is_empty() {
            return;
        }
        let sessions: Vec<(AgentKey, SessionActorHandle)> = self
            .session_map
            .iter()
            .map(|(key, session)| (*key, session.clone()))
            .collect();
        let _deltas = info_span!("deltas", sessions = sessions.len() as i64).entered();
        for (agent_key, session) in sessions {
            telemetry::metrics().record_session_queue_depth(session.queue_depth() as u64);
            let result = session.receive_delta(delta.clone());
            self.handle_send_result(agent_key, &session, result);
        }
    }

    async fn broadcast(&mut self, messages: Vec<BroadcastMessage>) {
        let map = self.shared_map.load();
        let players = self.player_positions(&map);
        for message in messages {
            self.route_to_recipients(&message, &players);
        }
    }

    fn player_positions(&self, map: &GameMap) -> Vec<(AgentKey, Position)> {
        self.session_map
            .keys()
            .filter_map(|key| Some((*key, map.agent_position(*key)?.clone())))
            .collect()
    }

    fn route_to_recipients(
        &mut self,
        message: &BroadcastMessage,
        players: &[(AgentKey, Position)],
    ) {
        match message.routing() {
            Routing::Agent(agent_key) => self.send_to(message, &agent_key),
            Routing::Viewport { at, same_floor } => {
                self.send_to_rect(
                    message,
                    players,
                    Rect::player_viewport(at),
                    at.z,
                    same_floor,
                    None,
                );
            }
            Routing::EitherViewport(positions) => {
                let regions = positions.map(|at| (Rect::player_viewport(at), at.z));
                self.send_to_rects(message, players, &regions);
            }
            Routing::ViewportAndAgent { at, agent } => {
                self.send_to_rect(
                    message,
                    players,
                    Rect::player_viewport(at),
                    at.z,
                    false,
                    None,
                );
                self.send_to(message, &agent);
            }
            Routing::Move { from, to, mover } => {
                let (a, b) = (Rect::player_viewport(from), Rect::player_viewport(to));
                self.send_to_rect(
                    message,
                    players,
                    Rect::new(
                        u16::min(a.min_x(), b.min_x()),
                        u16::min(a.min_y(), b.min_y()),
                        u16::max(a.max_x(), b.max_x()),
                        u16::max(a.max_y(), b.max_y()),
                    ),
                    to.z,
                    false,
                    Some(mover),
                );
                self.send_to(message, &mover);
            }
        }
    }

    fn send_to_rect(
        &mut self,
        message: &BroadcastMessage,
        players: &[(AgentKey, Position)],
        rect: Rect,
        floor: u8,
        same_floor: bool,
        originator: Option<AgentKey>,
    ) {
        for (agent_key, position) in players {
            if Some(*agent_key) != originator && sees(position, &rect, floor, same_floor) {
                self.send_to(message, agent_key);
            }
        }
    }

    fn send_to_rects(
        &mut self,
        message: &BroadcastMessage,
        players: &[(AgentKey, Position)],
        regions: &[(Rect, u8)],
    ) {
        for (agent_key, position) in players {
            if regions
                .iter()
                .any(|(rect, floor)| sees(position, rect, *floor, false))
            {
                self.send_to(message, agent_key);
            }
        }
    }

    /// The single place a failed send to a session is interpreted. Returns whether the
    /// message was delivered, so callers that track membership can prune.
    fn handle_send_result(
        &mut self,
        agent_key: AgentKey,
        session: &SessionActorHandle,
        result: Result<(), TrySendError<SessionCommand>>,
    ) -> bool {
        match result {
            Ok(()) => true,
            Err(TrySendError::Closed(..)) => {
                telemetry::metrics().record_session_evicted("closed");
                self.unsubscribe(agent_key);
                false
            }
            Err(TrySendError::Full(..)) => {
                telemetry::metrics().record_session_evicted("full");
                session.close();
                self.unsubscribe(agent_key);
                false
            }
        }
    }

    fn send_to(&mut self, message: &BroadcastMessage, agent_key: &AgentKey) {
        let Some(session) = self.session_map.get(agent_key).cloned() else {
            return;
        };
        let result = session.receive_broadcast(message.clone());
        self.handle_send_result(*agent_key, &session, result);
    }

    fn deliver_private_message(&mut self, author: AgentKey, recipient: AgentKey, message: String) {
        let Some(session) = self.session_map.get(&recipient).cloned() else {
            return;
        };
        let result = session.receive_chat_private(author, message);
        self.handle_send_result(recipient, &session, result);
    }

    /// Returns the recipients that could not be reached.
    fn deliver_channel_message(
        &mut self,
        author: AgentKey,
        recipients: Vec<AgentKey>,
        channel_id: ChannelId,
        message: String,
    ) -> Vec<AgentKey> {
        let mut dead = Vec::new();
        for recipient in recipients {
            let Some(session) = self.session_map.get(&recipient).cloned() else {
                dead.push(recipient);
                continue;
            };
            let result = session.receive_chat_channel(author, channel_id, message.clone());
            if !self.handle_send_result(recipient, &session, result) {
                dead.push(recipient);
            }
        }
        dead
    }
}

/// Whether a player standing at `at` sees an event in `rect` on `floor`: the same rect on every
/// floor visible from `floor`, or on `floor` alone when `same_floor`.
fn sees(at: &Position, rect: &Rect, floor: u8, same_floor: bool) -> bool {
    rect.contains(at)
        && if same_floor {
            at.z == floor
        } else {
            iter_visible_floors(floor).any(|z| z == at.z)
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::MapTile;
    use crate::entities::position::Direction;
    use crate::entities::position::Position;
    use crate::entities::world_delta::WorldDelta;
    use crate::entities::world_map::WorldMap;
    use crate::game::Tick;
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};
    use crate::telemetry::testing::capture_spans;
    use opentelemetry::{KeyValue, trace::SpanId};

    #[tokio::test]
    async fn routing_a_tick_is_its_own_trace_tagged_with_the_tick() {
        let captured = capture_spans();
        let mut router = a_router();
        let _world = tracing::info_span!("tick").entered();

        router
            .handle_command(MessageRouterCommand::Tick {
                events: Vec::new(),
                delta: Arc::new(WorldDelta::default()),
                tick: Tick(7),
            })
            .await;

        let spans = captured.finished();
        let find = |name: &str| spans.iter().find(|s| s.name == name).unwrap();
        let (route, broadcast) = (find("route"), find("broadcast"));
        assert_eq!(route.parent_span_id, SpanId::INVALID);
        assert!(route.attributes.contains(&KeyValue::new("tick", 7_i64)));
        assert_eq!(broadcast.parent_span_id, route.span_context.span_id());
    }

    fn a_router() -> MessageRouterActor {
        let (_tx, rx) = mpsc::channel(64);
        MessageRouterActor {
            rx,
            shared_map: Arc::new(ArcSwap::from_pointee(GameMap::new())),
            session_map: HashMap::new(),
        }
    }

    /// A map with one player standing at `at`, plus the tile under them.
    fn map_with_player(at: &Position) -> (GameMap, AgentKey) {
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let agent = Agent::from_player(a_test_snapshot(1, 1));
        let key = map.insert_agent(agent, at).unwrap();
        (map, key)
    }

    /// Seats a subscribed player at each position and returns their keys and inboxes, in order.
    fn seat_watchers(
        router: &mut MessageRouterActor,
        map: &mut GameMap,
        at: &[Position],
    ) -> (Vec<AgentKey>, Vec<mpsc::Receiver<SessionCommand>>) {
        at.iter()
            .enumerate()
            .map(|(i, position)| {
                map.insert_tile(position.clone(), MapTile::new());
                let player = Agent::from_player(a_test_snapshot(100 + i as u32, 1));
                let key = map.insert_agent(player, position).unwrap();
                let (handle, rx) = SessionActorHandle::for_test();
                router.session_map.insert(key, handle);
                (key, rx)
            })
            .unzip()
    }

    fn received(inboxes: &mut [mpsc::Receiver<SessionCommand>]) -> Vec<usize> {
        inboxes
            .iter_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).count())
            .collect()
    }

    fn route(router: &mut MessageRouterActor, map: &GameMap, message: &BroadcastMessage) {
        let players = router.player_positions(map);
        router.route_to_recipients(message, &players);
    }

    #[test]
    fn a_viewport_event_reaches_players_in_its_rect_on_every_floor_it_is_seen_from() {
        let mut map = GameMap::new();
        let mut router = a_router();
        let (_, mut inboxes) = seat_watchers(
            &mut router,
            &mut map,
            &[
                Position::new(109, 107, 7),
                Position::new(110, 100, 7),
                Position::new(100, 100, 5),
                Position::new(100, 100, 8),
            ],
        );
        let lair = Position::new(101, 101, 7);
        map.insert_tile(lair.clone(), MapTile::new());
        map.insert_agent(a_test_creature("rat", 10, (0, 0)), &lair)
            .unwrap();

        route(
            &mut router,
            &map,
            &BroadcastMessage::AttackMissed {
                position: Position::new(100, 100, 7),
            },
        );

        assert_eq!(received(&mut inboxes), [1, 0, 1, 0]);
    }

    #[test]
    fn a_same_floor_event_skips_players_on_the_floors_above() {
        let mut map = GameMap::new();
        let mut router = a_router();
        let (_, mut inboxes) = seat_watchers(
            &mut router,
            &mut map,
            &[Position::new(101, 100, 7), Position::new(101, 100, 6)],
        );

        route(
            &mut router,
            &map,
            &BroadcastMessage::AgentSaid {
                agent_key: AgentKey::default(),
                position: Position::new(100, 100, 7),
                message: "hi".to_owned(),
            },
        );

        assert_eq!(received(&mut inboxes), [1, 0]);
    }

    #[test]
    fn a_move_reaches_both_viewports_once_and_the_mover_once() {
        let mut map = GameMap::new();
        let mut router = a_router();
        let (from, to) = (Position::new(100, 100, 7), Position::new(101, 100, 7));
        let (keys, mut inboxes) = seat_watchers(
            &mut router,
            &mut map,
            &[
                to.clone(),
                Position::new(91, 100, 7),
                Position::new(110, 100, 7),
                Position::new(130, 100, 7),
            ],
        );
        let mover = keys[0];

        route(
            &mut router,
            &map,
            &BroadcastMessage::AgentMoved {
                agent_key: mover,
                direction: Direction::East,
                from_position: from,
                to_position: to,
            },
        );

        assert_eq!(received(&mut inboxes), [1, 1, 1, 0]);
    }

    #[test]
    fn a_teleport_seen_from_both_ends_arrives_once() {
        let mut map = GameMap::new();
        let mut router = a_router();
        let (_, mut inboxes) = seat_watchers(
            &mut router,
            &mut map,
            &[
                Position::new(103, 100, 7),
                Position::new(112, 100, 7),
                Position::new(130, 100, 7),
            ],
        );

        route(
            &mut router,
            &map,
            &BroadcastMessage::AgentTeleported {
                agent_key: AgentKey::default(),
                from_position: Position::new(100, 100, 7),
                to_position: Position::new(105, 100, 7),
            },
        );

        assert_eq!(received(&mut inboxes), [1, 1, 0]);
    }

    #[test]
    #[ignore = "timing, not a pass/fail assertion"]
    fn routing_the_startup_spawn_burst_on_the_shipped_map() {
        let items = crate::persistence::items::load_items(
            "assets/items",
            &crate::persistence::areas::AREA_SHAPES,
        )
        .expect("items load");
        let mut map =
            crate::persistence::map::load_map("assets/map1.otbm", &items).expect("map loads");
        let spawns =
            crate::persistence::spawns::load_spawns("assets/spawns.yaml").expect("spawns load");
        let events: Vec<BroadcastMessage> = spawns
            .iter()
            .filter_map(|spawn| {
                let agent_key = map
                    .insert_agent(a_test_creature("rat", 10, (0, 0)), &spawn.position)
                    .ok()?;
                Some(BroadcastMessage::PlayerSpawned {
                    agent_key,
                    position: spawn.position.clone(),
                })
            })
            .collect();
        let mut router = a_router();
        let (_, _inboxes) = seat_watchers(&mut router, &mut map, &[spawns[0].position.clone()]);

        let start = std::time::Instant::now();
        let players = router.player_positions(&map);
        for event in &events {
            router.route_to_recipients(event, &players);
        }

        println!(
            "{} spawn events routed in {:?}",
            events.len(),
            start.elapsed()
        );
    }

    #[test]
    fn speech_reaches_the_speaker_and_a_nearby_listener() {
        let pos = Position::new(100, 100, 7);
        let (mut map, speaker) = map_with_player(&pos);

        let listener_pos = Position::new(101, 100, 7);
        map.insert_tile(listener_pos.clone(), MapTile::new());
        let listener = map
            .insert_agent(Agent::from_player(a_test_snapshot(2, 1)), &listener_pos)
            .unwrap();

        let mut router = a_router();
        let (speaker_handle, mut speaker_rx) = SessionActorHandle::for_test();
        let (listener_handle, mut listener_rx) = SessionActorHandle::for_test();
        router.session_map.insert(speaker, speaker_handle);
        router.session_map.insert(listener, listener_handle);

        let message = BroadcastMessage::AgentSaid {
            agent_key: speaker,
            position: pos.clone(),
            message: "hello".to_owned(),
        };
        route(&mut router, &map, &message);

        assert!(
            speaker_rx.try_recv().is_ok(),
            "a speaker must hear their own speech"
        );
        assert!(
            listener_rx.try_recv().is_ok(),
            "a listener in the viewport must hear the speech"
        );
    }

    /// The speaker leaves the map in the same tick it spoke -- a logout right after a
    /// goodbye. Until the tile rode on the message the fan-out asked the map where the
    /// speaker was, found nothing, and dropped the line; the same shape as the reaped
    /// target in `session/combat.rs`.
    #[test]
    fn speech_still_reaches_a_listener_when_the_speaker_has_left_the_map() {
        let spoken_at = Position::new(100, 100, 7);
        let listener_pos = Position::new(101, 100, 7);
        let (mut map, listener) = map_with_player(&listener_pos);
        map.insert_tile(spoken_at.clone(), MapTile::new());

        let mut router = a_router();
        let (handle, mut rx) = SessionActorHandle::for_test();
        router.session_map.insert(listener, handle);

        let message = BroadcastMessage::AgentSaid {
            agent_key: AgentKey::default(),
            position: spoken_at,
            message: "bye".to_owned(),
        };
        route(&mut router, &map, &message);

        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn a_closed_session_is_unsubscribed_and_reported_dead() {
        let mut router = a_router();
        let (handle, rx) = SessionActorHandle::for_test();
        let key = AgentKey::default();
        router.session_map.insert(key, handle);

        drop(rx); // the session actor is gone

        let dead = router.deliver_channel_message(key, vec![key], ChannelId(1), "hello".to_owned());

        assert_eq!(dead, vec![key], "a closed session must be reported dead");
        assert!(
            !router.session_map.contains_key(&key),
            "a closed session must also be unsubscribed"
        );
    }

    #[tokio::test]
    async fn a_tick_delivers_its_events_before_its_delta() {
        let pos = Position::new(100, 100, 7);
        let (map, key) = map_with_player(&pos);
        let mut router = a_router();
        router.shared_map.store(Arc::new(map));
        let (handle, mut rx) = SessionActorHandle::for_test();
        router.session_map.insert(key, handle);
        let mut world = WorldMap::new(GameMap::new());
        world.insert_tile(pos.clone(), MapTile::new());

        router
            .tick(
                vec![BroadcastMessage::AgentSaid {
                    agent_key: key,
                    position: pos.clone(),
                    message: "hi".to_owned(),
                }],
                Arc::new(world.take_delta()),
            )
            .await;

        assert!(matches!(
            rx.try_recv(),
            Ok(SessionCommand::Broadcast(
                BroadcastMessage::AgentSaid { .. }
            ))
        ));
        assert!(matches!(rx.try_recv(), Ok(SessionCommand::WorldDelta(_))));
    }

    #[tokio::test]
    async fn an_empty_delta_is_not_sent() {
        let pos = Position::new(100, 100, 7);
        let (map, key) = map_with_player(&pos);
        let mut router = a_router();
        router.shared_map.store(Arc::new(map));
        let (handle, mut rx) = SessionActorHandle::for_test();
        router.session_map.insert(key, handle);

        router
            .tick(Vec::new(), Arc::new(WorldDelta::default()))
            .await;

        assert!(rx.try_recv().is_err());
    }
}

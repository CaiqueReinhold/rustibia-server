use anyhow::Result;
use arc_swap::ArcSwap;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{
    actors::session::{SessionActorHandle, SessionCommand},
    config::CONFIG,
    entities::{
        agent::AgentKey, chat::ChannelId, map::GameMap, position::Rect, world_delta::WorldDelta,
    },
    game::{
        events::{BroadcastMessage, Routing},
        map_query::iter_visible_floors,
    },
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

    pub async fn tick(&self, events: Vec<BroadcastMessage>, delta: Arc<WorldDelta>) {
        let _ = self
            .tx
            .send(MessageRouterCommand::Tick { events, delta })
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
            MessageRouterCommand::Tick { events, delta } => self.tick(events, delta).await,
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
        self.broadcast(events).await;
        if delta.is_empty() {
            return;
        }
        let sessions: Vec<(AgentKey, SessionActorHandle)> = self
            .session_map
            .iter()
            .map(|(key, session)| (*key, session.clone()))
            .collect();
        for (agent_key, session) in sessions {
            let result = session.receive_delta(delta.clone());
            self.handle_send_result(agent_key, &session, result);
        }
    }

    async fn broadcast(&mut self, messages: Vec<BroadcastMessage>) {
        let map = self.shared_map.load();
        for message in messages {
            self.route_to_recipients(&message, &map);
        }
    }

    fn route_to_recipients(&mut self, message: &BroadcastMessage, map: &GameMap) {
        match message.routing() {
            Routing::Agent(agent_key) => self.send_to(message, &agent_key),
            Routing::Viewport { at, same_floor } => {
                self.send_to_rect(
                    message,
                    map,
                    Rect::player_viewport(at),
                    at.z,
                    same_floor,
                    None,
                );
            }
            Routing::EitherViewport(positions) => {
                let regions = positions.map(|at| (Rect::player_viewport(at), at.z));
                self.send_to_rects(message, map, &regions);
            }
            Routing::ViewportAndAgent { at, agent } => {
                self.send_to_rect(message, map, Rect::player_viewport(at), at.z, false, None);
                self.send_to(message, &agent);
            }
            Routing::Move { from, to, mover } => {
                let (a, b) = (Rect::player_viewport(from), Rect::player_viewport(to));
                self.send_to_rect(
                    message,
                    map,
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
        map: &GameMap,
        rect: Rect,
        floor: u8,
        same_floor: bool,
        originator: Option<AgentKey>,
    ) {
        iter_visible_floors(floor)
            .filter(|z| !same_floor || floor == *z)
            .flat_map(|floor| map.iter_agents_in_rect(&rect, floor))
            .for_each(|(agent_key, _)| {
                if Some(agent_key) != originator {
                    self.send_to(message, &agent_key)
                }
            });
    }

    /// Deliver `message` once to every agent whose viewport intersects any of
    /// `regions`. A single rect (or the per-floor expansion of one) can never
    /// yield a duplicate — tiles and floors partition space — so dups only
    /// arise where two regions overlap; the `seen` set collapses those.
    fn send_to_rects(&mut self, message: &BroadcastMessage, map: &GameMap, regions: &[(Rect, u8)]) {
        let mut seen: HashSet<AgentKey> = HashSet::new();
        for (rect, z) in regions {
            for floor in iter_visible_floors(*z) {
                for (agent_key, _) in map.iter_agents_in_rect(rect, floor) {
                    if seen.insert(agent_key) {
                        self.send_to(message, &agent_key);
                    }
                }
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
                self.unsubscribe(agent_key);
                false
            }
            Err(TrySendError::Full(..)) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::agent::Agent;
    use crate::entities::map::MapTile;
    use crate::entities::position::Position;
    use crate::entities::world_delta::WorldDelta;
    use crate::entities::world_map::WorldMap;
    use crate::persistence::test_fixtures::a_test_snapshot;

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
        router.route_to_recipients(&message, &map);

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
        router.route_to_recipients(&message, &map);

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

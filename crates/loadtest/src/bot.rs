use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rustibia_server::constants::items::INVENTORY_COORD_FLAG;
use rustibia_server::entities::chat::{ChannelId, SayTarget};
use rustibia_server::entities::inventory::InventorySlot;
use rustibia_server::entities::items::ClientItemRef;
use rustibia_server::entities::position::Position;
use rustibia_server::entities::spells::SpellGroup;
use rustibia_server::messages::{ClientMessage, ServerMessage, TextMessageType};
use tokio::sync::mpsc::Sender;

use crate::brain::Brain;
use crate::config::Behaviour;
use crate::metrics::{Counter, Event, Probe};
use crate::probes::Probes;
use crate::wire::{Connection, WireError};
use crate::world::{ItemCatalogue, World};

const LOGOUT_GRACE: Duration = Duration::from_secs(2);
const LOGOUT_RETRY: Duration = Duration::from_millis(500);
/// A server that accepts the connection and then never sends the login
/// burst must not block this bot (and vanish from the report) forever.
const LOGIN_BURST_TIMEOUT: Duration = Duration::from_secs(30);
/// One in `IDLE_NOISE_IN` ping firings also carries a `Look` or `Say` — the
/// low-rate traffic a real session generates between fights, not silence.
const IDLE_NOISE_IN: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("login refused")]
    LoginRefused,
}

fn jitter((low, high): (u64, u64)) -> Duration {
    let millis = if low >= high {
        low
    } else {
        rand::random_range(low..=high)
    };
    Duration::from_millis(millis)
}

pub struct Bot {
    addr: SocketAddr,
    token: String,
    brain: Brain,
    world: World,
    probes: Probes,
    behaviour: Behaviour,
    awaiting_backpack: bool,
    login_reported: bool,
    last_strip_mismatches: u64,
    logged_strip_mismatch: bool,
    spell_check: Option<tokio::sync::mpsc::Sender<Vec<String>>>,
}

impl Bot {
    pub fn new(
        addr: SocketAddr,
        token: String,
        brain: Brain,
        catalogue: ItemCatalogue,
        tx: Sender<Event>,
    ) -> Self {
        let behaviour = brain.behaviour().clone();
        Self {
            addr,
            token,
            brain,
            world: World::new(catalogue),
            probes: Probes::new(tx),
            behaviour,
            awaiting_backpack: false,
            login_reported: false,
            last_strip_mismatches: 0,
            logged_strip_mismatch: false,
            spell_check: None,
        }
    }

    /// Runs `brain.missing_spells` against this bot's own world the first time
    /// it has a `SpellList`, and reports the result once through `report`.
    /// `run` gives every bot a clone of the same sender and acts on whichever
    /// result arrives first, so a login failure on any one bot cannot leave
    /// the check unperformed.
    pub fn check_spells_once(mut self, report: tokio::sync::mpsc::Sender<Vec<String>>) -> Self {
        self.spell_check = Some(report);
        self
    }

    /// Drains every tally that must not be lost regardless of how the run
    /// ends: dry potions, the strip-mismatch delta, and inbound messages/bytes
    /// since the last flush. Called at the end of every exit path — a decision
    /// tick, a disconnect, a decode error, and logout — not only the first.
    async fn flush_tallies(&mut self, bytes_read: u64) {
        let dry = self.brain.take_dry_events().len() as u64;
        self.probes.count(Counter::PotionsDry, dry).await;

        let mismatches = self.world.strip_mismatches;
        if mismatches > self.last_strip_mismatches {
            let delta = mismatches - self.last_strip_mismatches;
            self.last_strip_mismatches = mismatches;
            self.probes.count(Counter::StripMismatch, delta).await;
            if !self.logged_strip_mismatch {
                self.logged_strip_mismatch = true;
                tracing::warn!(
                    bot = %self.world.name,
                    mismatch = ?self.world.first_strip_mismatch,
                    "walk strip mismatch: past x or y = 32767 the server's strip wraps"
                );
            }
        }

        self.probes.flush_received(bytes_read).await;
    }

    pub async fn run_until(mut self, deadline: Instant) -> Result<(), BotError> {
        self.probes.count(Counter::LoginAttempted, 1).await;

        let mut connection = match Connection::connect(self.addr).await {
            Ok(connection) => connection,
            Err(error) => {
                self.probes.count(Counter::ConnectFailed, 1).await;
                return Err(error.into());
            }
        };

        if let Err(error) = connection
            .send(ClientMessage::Login {
                auth_token: self.token.clone(),
            })
            .await
        {
            self.probes.count(Counter::LoginSendFailed, 1).await;
            self.flush_tallies(connection.bytes_read()).await;
            return Err(error.into());
        }

        match tokio::time::timeout(LOGIN_BURST_TIMEOUT, self.await_login_burst(&mut connection))
            .await
        {
            Ok(result) => result?,
            Err(_elapsed) => {
                self.probes.count(Counter::LoginFailed, 1).await;
                self.flush_tallies(connection.bytes_read()).await;
                return Err(BotError::LoginRefused);
            }
        }

        let mut next_decision = Instant::now();
        let mut next_ping = Instant::now() + jitter(self.behaviour.ping_interval_ms);

        loop {
            tokio::select! {
                frame = connection.next() => match frame {
                    Some(Ok(message)) => self.handle_incoming(message, &mut connection).await,
                    Some(Err(WireError::Io(_))) | None => {
                        self.probes.count(Counter::SessionDisconnected, 1).await;
                        self.flush_tallies(connection.bytes_read()).await;
                        return Ok(());
                    }
                    Some(Err(error)) => {
                        tracing::warn!(bot = %self.world.name, %error, "undecodable frame");
                        self.probes.count(Counter::DecodeError, 1).await;
                        self.flush_tallies(connection.bytes_read()).await;
                        return Ok(());
                    }
                },
                _ = tokio::time::sleep_until(next_decision.into()) => {
                    let scheduled = next_decision;
                    next_decision = self.decide(&mut connection, scheduled).await;
                }
                _ = tokio::time::sleep_until(next_ping.into()) => {
                    next_ping = self.tick_ping(&mut connection).await;
                }
            }

            if Instant::now() >= deadline {
                break;
            }
        }

        self.logout(&mut connection).await;
        Ok(())
    }

    /// Waits through however many `DescribeMap`/`SpawnAgent` frames precede
    /// the `DescribePlayer` that always opens the real burst
    /// (`session/view.rs::player_spawned`), feeding each through
    /// `handle_incoming` like any other frame. Opening the backpack is
    /// driven off `DescribePlayer` arriving, not off "the first frame after
    /// `Login`".
    async fn await_login_burst(&mut self, connection: &mut Connection) -> Result<(), BotError> {
        loop {
            match connection.next().await {
                Some(Ok(ServerMessage::LoginError)) => {
                    self.probes.count(Counter::LoginFailed, 1).await;
                    self.flush_tallies(connection.bytes_read()).await;
                    return Err(BotError::LoginRefused);
                }
                Some(Ok(message)) => {
                    self.handle_incoming(message, connection).await;
                    if self.login_reported {
                        return Ok(());
                    }
                }
                Some(Err(error @ WireError::Io(_))) => {
                    self.probes.count(Counter::BurstDisconnected, 1).await;
                    self.flush_tallies(connection.bytes_read()).await;
                    return Err(error.into());
                }
                None => {
                    self.probes.count(Counter::BurstDisconnected, 1).await;
                    self.flush_tallies(connection.bytes_read()).await;
                    return Err(BotError::LoginRefused);
                }
                Some(Err(error)) => {
                    self.probes.count(Counter::DecodeError, 1).await;
                    self.flush_tallies(connection.bytes_read()).await;
                    return Err(error.into());
                }
            }
        }
    }

    async fn open_backpack(&mut self, connection: &mut Connection) {
        let Some(&item_id) = self.world.equipment.get(&InventorySlot::Backpack) else {
            return;
        };
        let command = ClientMessage::UseItem {
            item: ClientItemRef {
                position: Position::new(
                    INVENTORY_COORD_FLAG,
                    InventorySlot::Backpack.as_id() as u16,
                    0,
                ),
                item_id,
                stack_index: 0,
            },
        };
        self.awaiting_backpack = true;
        self.probes.open(Probe::OpenContainer, Instant::now()).await;
        let _ = connection.send(command).await;
    }

    async fn handle_incoming(&mut self, message: ServerMessage, connection: &mut Connection) {
        self.world.apply(&message);
        self.probes.note_incoming();

        match &message {
            ServerMessage::DescribePlayer { .. } => {
                if !self.login_reported {
                    self.login_reported = true;
                    self.probes.count(Counter::LoginSucceeded, 1).await;
                    self.open_backpack(connection).await;
                }
            }
            ServerMessage::PlayerWalkAck { .. } => {
                self.probes.resolve(Probe::WalkAck).await;
            }
            ServerMessage::PlayerWalkDenied => {
                // The server sends exactly one denial per refused
                // `MovePlayer`; without cancelling, this entry never
                // resolves and every later ack is timed against a strictly
                // older send.
                self.probes.cancel(Probe::WalkAck);
                self.probes.count(Counter::WalkDenied, 1).await;
            }
            ServerMessage::Pong => {
                self.probes.resolve(Probe::Ping).await;
            }
            ServerMessage::OpenContainer { container_id, .. } => {
                self.probes.resolve(Probe::OpenContainer).await;
                if self.awaiting_backpack {
                    self.awaiting_backpack = false;
                    self.world.mark_carried(*container_id);
                } else {
                    self.brain.corpse_opened(*container_id);
                }
            }
            ServerMessage::TextMessage {
                message_type: TextMessageType::ActionDenied,
                ..
            } => {
                if self.awaiting_backpack {
                    // The backpack open was refused: without clearing this,
                    // the next `OpenContainer` — the first corpse the loot
                    // loop opens — would be handed to `world.mark_carried`
                    // instead, and loot would count as the bot's own stock.
                    self.awaiting_backpack = false;
                    self.probes.cancel(Probe::OpenContainer);
                }
                self.probes.count(Counter::ActionRefused, 1).await;
            }
            ServerMessage::SpellCast { spell, .. } => {
                let group = self
                    .probes
                    .resolve_cast(*spell)
                    .unwrap_or(SpellGroup::Attack);
                self.world.apply_at(&message, group, Instant::now());
            }
            ServerMessage::SpellList { .. } => {
                if let Some(report) = self.spell_check.take() {
                    let _ = report.try_send(self.brain.missing_spells(&self.world));
                }
            }
            _ => {}
        }
    }

    async fn decide(&mut self, connection: &mut Connection, scheduled: Instant) -> Instant {
        let now = Instant::now();
        self.probes
            .sample(Probe::DecisionLag, now.saturating_duration_since(scheduled))
            .await;

        if let Some(command) = self.brain.decide(&self.world, now) {
            match &command {
                ClientMessage::MovePlayer { .. } => self.probes.open(Probe::WalkAck, now).await,
                ClientMessage::UseItem { .. } => self.probes.open(Probe::OpenContainer, now).await,
                ClientMessage::CastSpell { spell_id, .. } => {
                    let group = self
                        .world
                        .spells
                        .get(spell_id)
                        .map(|s| s.group)
                        .unwrap_or(SpellGroup::Attack);
                    self.probes.open_cast(*spell_id, group);
                }
                _ => {}
            }
            let _ = connection.send(command).await;
        }

        self.flush_tallies(connection.bytes_read()).await;

        now + jitter(self.behaviour.decision_interval_ms)
    }

    async fn tick_ping(&mut self, connection: &mut Connection) -> Instant {
        let now = Instant::now();
        self.probes.open(Probe::Ping, now).await;
        let _ = connection.send(ClientMessage::Ping).await;

        if rand::random_ratio(1, IDLE_NOISE_IN) {
            let noise = if rand::random_bool(0.5) {
                ClientMessage::Look {
                    position: self.world.position.clone(),
                }
            } else {
                ClientMessage::Say {
                    message: ".".to_string(),
                    target: SayTarget::Channel(ChannelId(0)),
                }
            };
            let _ = connection.send(noise).await;
        }

        now + jitter(self.behaviour.ping_interval_ms)
    }

    async fn logout(&mut self, connection: &mut Connection) {
        let _ = connection.send(ClientMessage::Logout).await;
        let grace_deadline = Instant::now() + LOGOUT_GRACE;
        let mut next_retry = Instant::now() + LOGOUT_RETRY;

        loop {
            if Instant::now() >= grace_deadline {
                break;
            }

            tokio::select! {
                frame = connection.next() => match frame {
                    Some(Ok(message)) => self.handle_incoming(message, connection).await,
                    Some(Err(_)) | None => break,
                },
                _ = tokio::time::sleep_until(next_retry.min(grace_deadline).into()) => {
                    let _ = connection.send(ClientMessage::Logout).await;
                    next_retry = Instant::now() + LOGOUT_RETRY;
                }
            }
        }

        self.flush_tallies(connection.bytes_read()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Coords, Route};
    use crate::testing::*;
    use futures::{SinkExt, StreamExt};
    use rustibia_server::entities::agent::{AgentId, Facing, OutfitColors, OutfitId};
    use rustibia_server::entities::items::{ContainerId, ItemId};
    use rustibia_server::entities::position::Position;
    use rustibia_server::entities::spells::SpellId;
    use rustibia_server::messages::{GameMessageCodec as ServerCodec, SpellListEntry};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_util::codec::Framed;

    fn a_brain() -> Brain {
        a_brain_with(Behaviour::test_default())
    }

    fn a_brain_with(behaviour: Behaviour) -> Brain {
        Brain::new(
            Route {
                name: "test".to_string(),
                waypoints: vec![Coords {
                    x: 102,
                    y: 100,
                    z: 7,
                }],
            },
            behaviour,
            "Loadbot ".to_string(),
        )
    }

    /// A stand-in server: reads the `Login`, answers with the real login
    /// burst, then answers every later message with `Pong` so the bot's loop
    /// keeps running.
    async fn a_server_that_logs_in() -> std::net::SocketAddr {
        spawn_server(true).await
    }

    /// The same, except the login is refused the way the real server refuses one.
    async fn a_server_that_refuses_login() -> std::net::SocketAddr {
        spawn_server(false).await
    }

    async fn spawn_server(accept: bool) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});

            let _login = framed.next().await;

            if !accept {
                framed.send(ServerMessage::LoginError).await.unwrap();
                return;
            }

            for message in login_burst() {
                framed.send(message).await.unwrap();
            }

            while framed.next().await.is_some() {
                framed.send(ServerMessage::Pong).await.unwrap();
            }
        });

        addr
    }

    #[tokio::test]
    async fn a_bot_logs_in_and_reports_the_login() {
        let addr = a_server_that_logs_in().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let outcome = Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_millis(200))
            .await;

        assert!(outcome.is_ok());
        assert!(drain(&mut rx).contains(&Event::Counted(Counter::LoginSucceeded, 1)));
    }

    #[tokio::test]
    async fn a_login_error_retires_the_bot_without_retrying() {
        let addr = a_server_that_refuses_login().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let outcome = Bot::new(addr, "bad".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_secs(5))
            .await;

        assert!(outcome.is_err());
        assert!(drain(&mut rx).contains(&Event::Counted(Counter::LoginFailed, 1)));
    }

    #[tokio::test]
    async fn a_bot_pings_on_its_own_cadence_and_records_the_round_trip() {
        let addr = a_server_that_logs_in().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut behaviour = Behaviour::test_default();
        behaviour.ping_interval_ms = (10, 20);

        Bot::new(
            addr,
            "game-token".into(),
            a_brain_with(behaviour),
            catalogue(),
            tx,
        )
        .run_until(Instant::now() + Duration::from_millis(300))
        .await
        .unwrap();

        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Sampled(Probe::Ping, _))),
            "no ping round trip was recorded"
        );
    }

    /// Contract: login sends no container contents, and the server only ever
    /// answers a container's changes while it is open. The real burst also
    /// puts several other frames (`DescribeMap`, `SpawnAgent`) before
    /// `DescribePlayer` — this must still be the first *command*, i.e. the
    /// backpack open must be driven off `DescribePlayer` arriving, not off
    /// "the first frame after `Login`".
    #[tokio::test]
    async fn the_first_command_after_login_opens_the_equipped_backpack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (record_tx, mut commands) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;

            for message in login_burst() {
                framed.send(message).await.unwrap();
            }

            while let Some(Ok(message)) = framed.next().await {
                let _ = record_tx.send(message);
                framed.send(ServerMessage::Pong).await.unwrap();
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let _handle = tokio::spawn(
            Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
                .run_until(Instant::now() + Duration::from_millis(150)),
        );

        let first = tokio::time::timeout(Duration::from_secs(1), commands.recv())
            .await
            .unwrap()
            .unwrap();

        let ClientMessage::UseItem { item } = &first else {
            panic!("expected the backpack's UseItem first, got {first:?}");
        };
        assert_eq!(item.item_id, ItemId(2854));
        assert_eq!(
            item.position,
            Position::new(
                INVENTORY_COORD_FLAG,
                InventorySlot::Backpack.as_id() as u16,
                0
            ),
            "an equipped item is addressed the way the client does: x=INVENTORY_COORD_FLAG, y=the slot id"
        );
    }

    /// A refused backpack open must not leave `awaiting_backpack` set: the
    /// next `OpenContainer` is the first corpse the loot loop opens, and
    /// must go to `brain.corpse_opened`, not `world.mark_carried`.
    #[tokio::test]
    async fn a_refused_backpack_open_does_not_mark_the_next_container_carried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }

            // deny the backpack UseItem, then answer a later OpenContainer
            // (a corpse) as any real corpse open would be answered
            let _use_item = framed.next().await;
            framed
                .send(ServerMessage::TextMessage {
                    text: "Sorry, not possible.".to_string(),
                    message_type: TextMessageType::ActionDenied,
                })
                .await
                .unwrap();

            while let Some(Ok(_)) = framed.next().await {
                framed
                    .send(ServerMessage::OpenContainer {
                        container_id: ContainerId(9),
                        capacity: 20,
                        has_parent: false,
                        title: "a corpse".to_string(),
                        items: vec![Some((ItemId(3031), 1))].into_boxed_slice(),
                    })
                    .await
                    .unwrap();
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut bot = Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx);

        // Drive the handshake and the denial by hand so the test can inspect
        // `awaiting_backpack` directly rather than depend on the loot loop's
        // own timing to produce a corpse `OpenContainer`.
        let mut connection = Connection::connect(addr).await.unwrap();
        connection
            .send(ClientMessage::Login {
                auth_token: "game-token".into(),
            })
            .await
            .unwrap();
        bot.await_login_burst(&mut connection).await.unwrap();
        assert!(bot.awaiting_backpack, "the backpack open must be pending");

        // `login_reported` (and so `await_login_burst`) returns as soon as
        // `DescribePlayer` is seen, which can be before the rest of the
        // burst (`PlayerSkills`, `SpellList`, `PlayerStatus`) has arrived —
        // drain those like any other frame until the denial itself shows up.
        loop {
            let message = connection.next().await.unwrap().unwrap();
            let is_denial = matches!(
                message,
                ServerMessage::TextMessage {
                    message_type: TextMessageType::ActionDenied,
                    ..
                }
            );
            bot.handle_incoming(message, &mut connection).await;
            if is_denial {
                break;
            }
        }
        assert!(
            !bot.awaiting_backpack,
            "a denial must clear the pending backpack flag"
        );

        // The fake server answers any further inbound message with a
        // corpse's `OpenContainer`; nothing else here would ever prompt it,
        // since this test drives `handle_incoming` by hand rather than
        // through `decide`.
        connection.send(ClientMessage::Ping).await.unwrap();
        let opened = connection.next().await.unwrap().unwrap();
        bot.handle_incoming(opened, &mut connection).await;

        assert_eq!(
            bot.world.backpack(),
            None,
            "the corpse must not be mistaken for the never-opened backpack"
        );
    }

    /// A server that accepts the connection and then never sends the login
    /// burst must not block the bot forever; it must retire like any other
    /// login failure, not vanish from the report uncounted.
    #[tokio::test]
    async fn a_login_burst_that_never_arrives_times_out_as_a_login_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut bot = Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx);
        // A real run would wait `LOGIN_BURST_TIMEOUT`; exercise the timeout
        // mechanism directly against a short deadline instead of a 30 s test.
        let mut connection = Connection::connect(addr).await.unwrap();
        connection
            .send(ClientMessage::Login {
                auth_token: "game-token".into(),
            })
            .await
            .unwrap();

        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            bot.await_login_burst(&mut connection),
        )
        .await;

        assert!(
            outcome.is_err(),
            "the burst never arrives, so this is exercising the timeout wrapper's own deadline"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "no counter yet: run_until applies the timeout, not await_login_burst"
        );
    }

    /// A server reset (a closed socket, not a decoding failure) must never be
    /// mistaken for `DecodeError` — that would blame the protocol at exactly
    /// the moment the server fell over.
    #[tokio::test]
    async fn a_dropped_connection_counts_as_disconnected_not_a_decode_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            // drops the connection here without answering anything else
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_secs(5))
            .await;

        assert!(outcome.is_ok());
        let events = drain(&mut rx);
        assert!(events.contains(&Event::Counted(Counter::SessionDisconnected, 1)));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Counted(Counter::DecodeError, _)))
        );
    }

    #[tokio::test]
    async fn an_unknown_opcode_counts_as_a_decode_error_and_ends_the_bot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            let mut socket = framed.into_inner();
            // length-prefixed frame carrying an opcode this codec has no arm for
            socket.write_all(&[1, 0, 0xFF]).await.unwrap();
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_secs(5))
            .await;

        assert!(outcome.is_ok());
        assert!(drain(&mut rx).contains(&Event::Counted(Counter::DecodeError, 1)));
    }

    #[tokio::test]
    async fn every_decision_tick_samples_its_own_lag() {
        let addr = a_server_that_logs_in().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);

        Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_millis(200))
            .await
            .unwrap();

        assert!(
            drain(&mut rx)
                .iter()
                .any(|e| matches!(e, Event::Sampled(Probe::DecisionLag, _)))
        );
    }

    #[tokio::test]
    async fn a_dry_health_potion_is_counted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            framed
                .send(ServerMessage::AgentLifeChanged {
                    agent_id: AgentId(1),
                    current: 50,
                    max: 500,
                })
                .await
                .unwrap();

            while framed.next().await.is_some() {
                framed.send(ServerMessage::Pong).await.unwrap();
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_millis(150))
            .await
            .unwrap();

        assert!(
            drain(&mut rx)
                .iter()
                .any(|e| matches!(e, Event::Counted(Counter::PotionsDry, n) if *n >= 1))
        );
    }

    #[tokio::test]
    async fn a_strip_mismatch_is_counted_from_the_worlds_delta() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            // East expects a PLAYER_VIEWPORT_HEIGHT-long strip; 3 is a mismatch.
            framed
                .send(ServerMessage::PlayerWalkAck {
                    position: Position::new(101, 100, 7),
                    tiles: vec![(7u8, vec![Default::default(); 3].into_boxed_slice())],
                })
                .await
                .unwrap();

            while framed.next().await.is_some() {
                framed.send(ServerMessage::Pong).await.unwrap();
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_millis(150))
            .await
            .unwrap();

        assert!(
            drain(&mut rx)
                .iter()
                .any(|e| matches!(e, Event::Counted(Counter::StripMismatch, n) if *n >= 1))
        );
    }

    /// Feeds the `SpellCast` reply back through `world.apply_at` with the group
    /// of the spell this bot sent; if that wiring is missing, nothing suppresses
    /// the cast and the brain resends it on every decision tick.
    #[tokio::test]
    async fn a_spell_cast_replys_cooldown_suppresses_further_casts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (record_tx, mut commands) = mpsc::unbounded_channel();
        let spell_id = SpellId(9);

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;

            let spawn = ServerMessage::SpawnAgent {
                agent_id: AgentId(2),
                outfit: (OutfitId(1), OutfitColors::new(0, 0, 0, 0)),
                position: Position::new(101, 100, 7),
                facing: Facing::North,
                name: "Rat".to_string(),
                life: 100,
                speed: 100,
            };
            let spells = vec![SpellListEntry {
                id: spell_id,
                name: "Flame Strike".to_string(),
                words: "exori flam".to_string(),
                level: 1,
                icon: 0,
                aimable: true,
                group: SpellGroup::Attack,
            }];
            for message in login_burst_with(vec![spawn], spells) {
                framed.send(message).await.unwrap();
            }

            while let Some(Ok(message)) = framed.next().await {
                let _ = record_tx.send(message.clone());
                let reply = if matches!(message, ClientMessage::CastSpell { .. }) {
                    ServerMessage::SpellCast {
                        spell: spell_id,
                        spell_cooldown_ms: 2000,
                        group_cooldown_ms: 2000,
                    }
                } else {
                    ServerMessage::Pong
                };
                framed.send(reply).await.unwrap();
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut behaviour = Behaviour::test_default();
        // Wide enough that the cast's own round trip (loopback + tokio
        // scheduling, under whatever contention the full suite is running
        // under) always lands well inside one interval; a race here would
        // make the assertion below flaky rather than meaningful.
        behaviour.decision_interval_ms = (80, 120);

        Bot::new(
            addr,
            "game-token".into(),
            a_brain_with(behaviour),
            catalogue(),
            tx,
        )
        .run_until(Instant::now() + Duration::from_millis(400))
        .await
        .unwrap();

        let ticks = drain(&mut rx)
            .iter()
            .filter(|e| matches!(e, Event::Sampled(Probe::DecisionLag, _)))
            .count();
        assert!(
            ticks >= 3,
            "too few decision ticks ({ticks}) for this assertion to mean anything; \
             a single tick would pass with the cooldown wiring removed"
        );

        let mut casts = 0;
        while let Ok(message) = commands.try_recv() {
            if matches!(message, ClientMessage::CastSpell { .. }) {
                casts += 1;
            }
        }
        assert_eq!(
            casts, 1,
            "the cooldown from the server's own reply must suppress further casts"
        );
    }

    /// A `PlayerWalkAck` must resolve `Probe::WalkAck` and a `Pong` must
    /// resolve `Probe::Ping` — never each other's. The server answers each
    /// with a distinguishable, artificially different delay, so recording
    /// them into the wrong probe (or into a shared one) fails on the values,
    /// not merely on "some sample of some kind arrived".
    #[tokio::test]
    async fn a_walk_ack_and_a_pong_each_resolve_only_their_own_probe() {
        const PING_DELAY: Duration = Duration::from_millis(120);
        const WALK_DELAY: Duration = Duration::from_millis(10);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            framed
                .send(a_walkable_map(Position::new(100, 100, 7), 7))
                .await
                .unwrap();

            while let Some(Ok(message)) = framed.next().await {
                match message {
                    ClientMessage::MovePlayer { direction } => {
                        tokio::time::sleep(WALK_DELAY).await;
                        framed
                            .send(ServerMessage::PlayerWalkAck {
                                position: Position::new(100, 100, 7) + direction,
                                tiles: vec![],
                            })
                            .await
                            .unwrap();
                    }
                    ClientMessage::Ping => {
                        tokio::time::sleep(PING_DELAY).await;
                        framed.send(ServerMessage::Pong).await.unwrap();
                    }
                    _ => framed.send(ServerMessage::Pong).await.unwrap(),
                }
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut behaviour = Behaviour::test_default();
        behaviour.ping_interval_ms = (150, 160);
        behaviour.decision_interval_ms = (20, 30);

        Bot::new(
            addr,
            "game-token".into(),
            a_brain_with(behaviour),
            catalogue(),
            tx,
        )
        .run_until(Instant::now() + Duration::from_millis(400))
        .await
        .unwrap();

        let events = drain(&mut rx);
        let walk_sample = events.iter().find_map(|e| match e {
            Event::Sampled(Probe::WalkAck, d) => Some(*d),
            _ => None,
        });
        let ping_sample = events.iter().find_map(|e| match e {
            Event::Sampled(Probe::Ping, d) => Some(*d),
            _ => None,
        });

        let walk_sample = walk_sample.expect("no walk-ack probe was recorded");
        let ping_sample = ping_sample.expect("no ping probe was recorded");

        assert!(
            walk_sample < PING_DELAY,
            "the walk sample ({walk_sample:?}) looks like it resolved the ping's delayed reply"
        );
        assert!(
            ping_sample >= PING_DELAY,
            "the ping sample ({ping_sample:?}) looks like it resolved the walk's fast reply"
        );
    }

    /// A denied `MovePlayer` must not leave its probe pending: the server
    /// denies the first step, then acks the second, and the recorded sample
    /// must reflect only the second send.
    #[tokio::test]
    async fn a_walk_denial_does_not_leave_a_stale_probe_for_the_next_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }
            framed
                .send(a_walkable_map(Position::new(100, 100, 7), 7))
                .await
                .unwrap();

            let mut denied = false;
            while let Some(Ok(message)) = framed.next().await {
                let reply = match message {
                    ClientMessage::MovePlayer { .. } if !denied => {
                        denied = true;
                        ServerMessage::PlayerWalkDenied
                    }
                    ClientMessage::MovePlayer { direction } => ServerMessage::PlayerWalkAck {
                        position: Position::new(100, 100, 7) + direction,
                        tiles: vec![],
                    },
                    _ => ServerMessage::Pong,
                };
                framed.send(reply).await.unwrap();
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut behaviour = Behaviour::test_default();
        behaviour.decision_interval_ms = (10, 20);

        Bot::new(
            addr,
            "game-token".into(),
            a_brain_with(behaviour),
            catalogue(),
            tx,
        )
        // The brain won't retry a denied step until its own pacing window
        // elapses (~600ms at this speed/friction) — long enough to give a
        // stale first-send sample a lot of room to show up wrong.
        .run_until(Instant::now() + Duration::from_millis(2000))
        .await
        .unwrap();

        let sample = drain(&mut rx)
            .into_iter()
            .find_map(|e| match e {
                Event::Sampled(Probe::WalkAck, d) => Some(d),
                _ => None,
            })
            .expect("expected a walk-ack sample");

        assert!(
            sample < Duration::from_millis(300),
            "the sample ({sample:?}) must be bounded by the second send, not the denied first one"
        );
    }

    /// A bot sends `Logout` when its deadline arrives.
    #[tokio::test]
    async fn a_bot_sends_logout_at_its_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (record_tx, mut commands) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            for message in login_burst() {
                framed.send(message).await.unwrap();
            }

            while let Some(Ok(message)) = framed.next().await {
                let _ = record_tx.send(message.clone());
                framed.send(ServerMessage::Pong).await.unwrap();
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        Bot::new(addr, "game-token".into(), a_brain(), catalogue(), tx)
            .run_until(Instant::now() + Duration::from_millis(100))
            .await
            .unwrap();

        let mut sent_logout = false;
        while let Ok(message) = commands.try_recv() {
            if matches!(message, ClientMessage::Logout) {
                sent_logout = true;
            }
        }
        assert!(sent_logout, "expected a Logout at the deadline");
    }
}

//! The per-player session actor: its handle, its state, and the loop that owns
//! both. The two dispatch tables live here; each arm's handler lives in the
//! submodule for its topic.

mod chat;
mod combat;
mod delta;
mod items;
mod movement;
mod view;

use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use thiserror::Error;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;

use crate::actors::SharedContext;
use crate::actors::chat::ChatActorHandle;
use crate::actors::connection::ConnectionActorHandle;
use crate::actors::persistence::PersistenceActorHandle;
use crate::actors::world::WorldActorHandle;
use crate::actors::world::WorldCommand;
use crate::config::CONFIG;
use crate::entities::agent::Agent;
use crate::entities::agent::AgentId;
use crate::entities::agent::AgentKey;
use crate::entities::chat::ChannelId;
use crate::entities::effects::AreaEffect;
use crate::entities::items::ContainerId;
use crate::entities::items::ItemGuid;
use crate::entities::map::GameMap;
use crate::entities::position::Direction;
use crate::entities::world_delta::WorldDelta;
use crate::game::events::BroadcastMessage;
use crate::game::{Tick, TickDelta};
use crate::local_id::LocalIdMap;
use crate::messages::TextMessageType;
use crate::messages::{ClientMessage, ServerMessage};
use crate::online_registry::RegistryGuard;

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session failed to initialize")]
    FailedToInitialize,
    #[error("Message type unknown or out of order")]
    WrongMessageType,
    #[error("Invalid State")]
    InvalidState,
    #[error("Connection is closed")]
    ConnectionClosed,
    #[error("Player logged out")]
    Logout,
    #[error("World actor stopped")]
    WorldStopped,
}

#[derive(Clone, Debug)]
pub enum SessionCommand {
    PlayerMessage(ClientMessage),
    Broadcast(BroadcastMessage),
    WorldDelta(Arc<WorldDelta>),
    ChatPrivate {
        author: AgentKey,
        message: String,
    },
    ChatChannel {
        author: AgentKey,
        channel: ChannelId,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct SessionActorHandle {
    tx: mpsc::Sender<SessionCommand>,
    token: CancellationToken,
}

impl SessionActorHandle {
    pub fn close(&self) {
        self.token.cancel();
    }

    pub async fn receive_message(
        &self,
        msg: ClientMessage,
    ) -> Result<(), mpsc::error::SendError<SessionCommand>> {
        self.tx.send(SessionCommand::PlayerMessage(msg)).await?;
        Ok(())
    }

    pub fn receive_broadcast(
        &self,
        msg: BroadcastMessage,
    ) -> Result<(), mpsc::error::TrySendError<SessionCommand>> {
        self.tx.try_send(SessionCommand::Broadcast(msg))?;
        Ok(())
    }

    pub fn receive_delta(
        &self,
        delta: Arc<WorldDelta>,
    ) -> Result<(), mpsc::error::TrySendError<SessionCommand>> {
        self.tx.try_send(SessionCommand::WorldDelta(delta))?;
        Ok(())
    }

    pub fn receive_chat_private(
        &self,
        author: AgentKey,
        message: String,
    ) -> Result<(), mpsc::error::TrySendError<SessionCommand>> {
        self.tx
            .try_send(SessionCommand::ChatPrivate { author, message })?;
        Ok(())
    }

    pub fn receive_chat_channel(
        &self,
        author: AgentKey,
        channel: ChannelId,
        message: String,
    ) -> Result<(), mpsc::error::TrySendError<SessionCommand>> {
        self.tx.try_send(SessionCommand::ChatChannel {
            author,
            channel,
            message,
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<SessionCommand>) {
        let (tx, rx) = mpsc::channel(64);
        (
            Self {
                tx,
                token: CancellationToken::new(),
            },
            rx,
        )
    }
}

pub struct SessionActor {
    session_id: String,
    rx: mpsc::Receiver<SessionCommand>,
    token: CancellationToken,
    connection: ConnectionActorHandle,
    world: WorldActorHandle,
    player_key: AgentKey,
    shared_map: Arc<ArcSwap<GameMap>>,
    containers: LocalIdMap<ItemGuid, ContainerId>,
    agents: LocalIdMap<AgentKey, AgentId>,
    persistence: PersistenceActorHandle,
    chat: ChatActorHandle,
    tick_rx: watch::Receiver<Tick>,
    next_chat_tick: Tick,
    queued_walk: Option<Direction>,
    logout_pending: bool,
    registry_guard: Option<RegistryGuard>,
}

#[cfg(test)]
type TestSession = (
    SessionActor,
    mpsc::Receiver<crate::actors::connection::ConnectionCommand>,
    mpsc::Receiver<(WorldCommand, Option<TickDelta>)>,
    watch::Sender<Tick>,
);

impl SessionActor {
    pub fn start(
        session_id: String,
        connection: ConnectionActorHandle,
        context: SharedContext,
        agent: Agent,
        registry_guard: RegistryGuard,
    ) -> SessionActorHandle {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);
        let token = CancellationToken::new();
        let self_handle = SessionActorHandle {
            tx,
            token: token.clone(),
        };

        let self_handle_clone = self_handle.clone();
        tokio::spawn(async move {
            let spawn_result = context
                .world
                .spawn_player(agent, self_handle_clone.clone())
                .await;
            match spawn_result {
                Ok((agent_key, message_router_guard)) => {
                    let _router_guard = message_router_guard;
                    let actor = Self {
                        session_id,
                        rx,
                        token,
                        connection,
                        chat: context.chat.clone(),
                        world: context.world.clone(),
                        player_key: agent_key,
                        shared_map: context.shared_map.clone(),
                        containers: LocalIdMap::new(),
                        agents: LocalIdMap::new(),
                        persistence: context.persistence.clone(),
                        tick_rx: context.tick_rx.clone(),
                        next_chat_tick: Tick(0),
                        queued_walk: None,
                        logout_pending: false,
                        registry_guard: Some(registry_guard),
                    };
                    actor.run().await;
                }
                Err(e) => {
                    error!(session = session_id, "Failed to spawn player: {e}");
                    let _ = connection.close().await;
                }
            }
        });

        self_handle
    }

    async fn save_player(&self) {
        let map = self.shared_map.load();
        let Some(agent) = map.get_agent(self.player_key) else {
            return;
        };
        let Some(position) = map.agent_position(self.player_key) else {
            return;
        };
        let Some(snapshot) = agent.to_snapshot(position.clone()) else {
            return;
        };
        if let Err(e) = self.persistence.save_player(snapshot).await {
            error!(
                session = self.session_id,
                "Failed to queue player save: {e}"
            );
        }
    }

    async fn run(mut self) {
        info!(session = self.session_id, "Session actor started");

        let mut save_timer = tokio::time::interval(CONFIG.save_interval);
        save_timer.tick().await; // skip the immediate first tick

        loop {
            let result = select! { biased;
                _ = self.token.cancelled() => {
                    self.close_connection().await;
                    break;
                }
                changed = self.tick_rx.changed() => {
                    if changed.is_err() {
                        // The world's tick sender was dropped. Without this the
                        // branch returns Ready forever and the session spins.
                        Err(SessionError::WorldStopped.into())
                    } else {
                        self.tick_schedules().await
                    }
                }
                cmd = self.rx.recv() =>
                    if let Some(cmd) = cmd {
                        self.route_command(cmd).await
                    } else {
                        Err(SessionError::ConnectionClosed.into())
                    },
                _ = save_timer.tick() => {
                    self.save_player().await;
                    Ok(())
                }
            };
            if let Err(e) = result {
                if e.downcast_ref::<SessionError>()
                    .is_some_and(|e| matches!(e, SessionError::Logout))
                {
                    info!(session = self.session_id, "Player logged out cleanly");
                } else {
                    error!(session = self.session_id, "Error on session command: {e}");
                }
                break;
            }
        }

        self.save_player().await;
        let _ = self.connection.close().await;

        let delay_ticks =
            (CONFIG.player_despawn_delay.as_millis() / CONFIG.tick_duration.as_millis()) as u64;
        let _ = self
            .world
            .send_delayed(
                WorldCommand::DespawnPlayer {
                    agent_key: self.player_key,
                    registry: self.registry_guard.take(),
                    give_up_at: None,
                },
                TickDelta(delay_ticks),
            )
            .await;
    }

    async fn close_connection(&self) {
        let _ = self.connection.close().await;
    }

    async fn route_command(&mut self, cmd: SessionCommand) -> Result<()> {
        match cmd {
            SessionCommand::PlayerMessage(msg) => self.handle_client_message(msg).await,
            SessionCommand::Broadcast(msg) => self.route_broadcast(msg).await,
            SessionCommand::WorldDelta(delta) => self.apply_delta(&delta).await,
            SessionCommand::ChatPrivate { author, message } => {
                self.receive_private_message(author, message).await
            }
            SessionCommand::ChatChannel {
                author,
                channel,
                message,
            } => self.receive_channel_message(author, channel, message).await,
        }
    }

    async fn handle_client_message(&mut self, command: ClientMessage) -> Result<()> {
        match command {
            ClientMessage::Ping => self.pong().await,
            ClientMessage::Login { .. } => Err(SessionError::WrongMessageType.into()),
            ClientMessage::MovePlayer { direction } => self.handle_move_player(direction).await,
            ClientMessage::GetPlayerPosition => self.handle_get_position().await,
            ClientMessage::MoveItem { item, amount, to } => {
                self.handle_move_item(item, amount, to).await
            }
            ClientMessage::UseItem { item } => self.handle_use_item(item).await,
            ClientMessage::CloseContainer { container_id } => {
                self.handle_close_container(container_id)
            }
            ClientMessage::OpenParentContainer { container_id } => {
                self.handle_open_parent_container(container_id).await
            }
            ClientMessage::ChangeDirection { direction } => {
                self.handle_change_direction(direction).await
            }
            ClientMessage::Logout => self.handle_logout().await,
            ClientMessage::UseItemWith {
                source,

                target,
                target_agent,
            } => {
                self.handle_use_item_with(source, target, target_agent)
                    .await
            }
            ClientMessage::Look { position } => self.handle_look(position).await,
            ClientMessage::Say { message, target } => self.handle_say(message, target).await,
            ClientMessage::RequestChannels => self.handle_request_channels().await,
            ClientMessage::OpenChannel { channel } => self.handle_open_channel(channel).await,
            ClientMessage::CloseChannel { channel } => self.handle_close_channel(channel).await,
            ClientMessage::OpenPmChat { name } => self.handle_open_pm_chat(name).await,
            ClientMessage::SetTarget { agent_id, seq } => {
                self.handle_set_target(agent_id, seq).await
            }
            ClientMessage::CastSpell {
                spell_id,
                target,
                param,
            } => self.handle_cast_spell(spell_id, target, param).await,
        }
    }

    async fn route_broadcast(&mut self, msg: BroadcastMessage) -> Result<()> {
        match msg {
            BroadcastMessage::AgentMoved {
                agent_key,
                direction,
                to_position,
                ..
            } => self.agent_moved(agent_key, direction, to_position).await,
            BroadcastMessage::PlayerSpawned {
                agent_key,
                position,
            } => self.player_spawned(agent_key, position).await,
            BroadcastMessage::MoveItemDenied { message, .. } => self.deny(&message).await,
            BroadcastMessage::UseItemDenied { message, .. } => self.deny(&message).await,
            BroadcastMessage::OpenContainer { item, .. } => self.open_container(item).await,
            BroadcastMessage::AgentWalkDenied { .. } => self.walk_denied().await,
            BroadcastMessage::AgentDespawned {
                agent_key,
                snapshot,
                ..
            } => self.agent_despawned(agent_key, snapshot).await,
            BroadcastMessage::AgentTeleported {
                agent_key,
                to_position,
                ..
            } => self.agent_teleported(agent_key, to_position).await,
            BroadcastMessage::LogoutDenied { .. } => self.logout_denied().await,
            BroadcastMessage::AgentSaid {
                agent_key,
                position,
                message,
            } => self.agent_said(agent_key, position, message).await,
            BroadcastMessage::AgentLostTarget { seq, .. } => self.target_lost(seq).await,
            BroadcastMessage::DamageTaken {
                source,
                target,
                position,
                blood_type,
                damage,
            } => {
                self.agent_took_damage(source, target, position, blood_type, damage)
                    .await
            }
            BroadcastMessage::MissileLaunched { missile } => {
                self.missile_launched(missile.from, missile.to, missile.missile_id)
                    .await
            }
            BroadcastMessage::AttackMissed { position } => self.attack_missed(position).await,
            BroadcastMessage::ExperienceGained { amount, .. } => {
                self.experience_gained(amount).await
            }
            BroadcastMessage::SkillUpgraded {
                skill_type, gained, ..
            } => self.skill_upgraded(skill_type, gained).await,
            BroadcastMessage::PotionDrunk { target, position } => {
                self.potion_drunk(target, position).await
            }
            BroadcastMessage::SpellCast {
                agent_key,
                spell_id,
                ..
            } => self.spell_cast(agent_key, spell_id).await,
            BroadcastMessage::SpellDenied {
                agent_key,
                position,
                reason,
                delivery,
            } => {
                self.spell_denied(agent_key, position, reason, delivery)
                    .await
            }
            BroadcastMessage::AgentHealed {
                position,
                amount,
                restore_type,
                ..
            } => self.agent_healed(position, amount, restore_type).await,
            BroadcastMessage::AreaEffectAppeared { area_effect } => {
                self.send_effect(area_effect).await
            }
            BroadcastMessage::AgentActionMessage { position, message } => {
                self.action_message(position, message).await
            }
        }
    }

    async fn pong(&self) -> Result<()> {
        self.connection.send_message(ServerMessage::Pong).await?;
        Ok(())
    }

    async fn send_effect(&self, effect: AreaEffect) -> Result<()> {
        self.connection
            .send_message(ServerMessage::ShowEffect {
                effect_id: effect.effect_id,
                position: effect.origin,
                delta: effect.delta,
            })
            .await?;
        Ok(())
    }

    /// The one place a refusal reaches the player.
    async fn deny(&self, text: &str) -> Result<()> {
        self.connection
            .send_message(ServerMessage::TextMessage {
                text: text.to_owned(),
                message_type: TextMessageType::ActionDenied,
            })
            .await?;
        Ok(())
    }

    async fn tick_schedules(&mut self) -> Result<()> {
        self.check_walk_queue().await?;
        self.remove_agents_not_in_reach().await?;
        Ok(())
    }

    async fn handle_logout(&mut self) -> Result<()> {
        if !self.logout_pending {
            self.logout_pending = true;
            self.world
                .send(WorldCommand::RequestLogout {
                    agent_key: self.player_key,
                })
                .await;
        }
        Ok(())
    }

    async fn logout_denied(&self) -> Result<()> {
        self.deny("You may not logout during an action.").await
    }

    #[cfg(test)]
    fn for_test(player_key: AgentKey, map: GameMap) -> TestSession {
        use crate::actors::chat::ChatActorHandle;
        use crate::actors::world::WorldActorHandle;

        let (_tx, rx) = mpsc::channel(64);
        let (connection, connection_rx) = ConnectionActorHandle::for_test();
        let (world, world_rx) = WorldActorHandle::for_test();
        let (chat, _chat_rx) = ChatActorHandle::for_test();
        let (persistence, _persistence_rx) = PersistenceActorHandle::for_test(16);
        let (tick_tx, tick_rx) = watch::channel(Tick(0));

        (
            Self {
                session_id: "test".to_owned(),
                rx,
                token: CancellationToken::new(),
                connection,
                world,
                chat,
                player_key,
                shared_map: Arc::new(ArcSwap::from_pointee(map)),
                containers: LocalIdMap::new(),
                agents: LocalIdMap::new(),
                persistence,
                tick_rx,
                next_chat_tick: Tick(0),
                queued_walk: None,
                logout_pending: false,
                registry_guard: None,
            },
            connection_rx,
            world_rx,
            tick_tx,
        )
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::entities::map::MapTile;
    use crate::entities::position::Position;
    use crate::persistence::test_fixtures::a_test_snapshot;

    pub fn seat_player(map: &mut GameMap, at: &Position, id: u32) -> AgentKey {
        map.insert_tile(at.clone(), MapTile::new());
        map.insert_agent(Agent::from_player(a_test_snapshot(id, 1)), at)
            .unwrap()
    }
}

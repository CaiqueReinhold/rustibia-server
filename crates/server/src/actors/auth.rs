use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use super::{SharedContext, connection::ConnectionError, session::SessionActor};
use crate::actors::connection::ConnectionActorHandle;
use crate::telemetry;
use crate::{
    config::CONFIG,
    entities::agent::Agent,
    messages::{ClientMessage, ServerMessage},
    persistence::login::{LoginError, LoginRepository},
};

#[derive(Clone, Debug)]
pub enum AuthCommand {
    ReceivePlayerMessage(ClientMessage),
}

#[derive(Clone, Debug)]
pub struct AuthActorHandle {
    tx: mpsc::Sender<AuthCommand>,
}

impl AuthActorHandle {
    pub async fn receive_message(
        &self,
        msg: ClientMessage,
    ) -> Result<(), mpsc::error::SendError<AuthCommand>> {
        self.tx.send(AuthCommand::ReceivePlayerMessage(msg)).await?;
        Ok(())
    }
}

pub struct AuthActor<L: LoginRepository> {
    session_id: String,
    rx: mpsc::Receiver<AuthCommand>,
    world_ctx: SharedContext,
    login_repo: Arc<L>,
}

impl<L: LoginRepository + 'static> AuthActor<L> {
    pub fn start(
        session_id: String,
        conn_rx: oneshot::Receiver<ConnectionActorHandle>,
        login_repo: Arc<L>,
        world_ctx: SharedContext,
    ) -> AuthActorHandle {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);

        tokio::spawn(async move {
            let actor = Self {
                session_id,
                rx,
                world_ctx,
                login_repo,
            };
            actor.run(conn_rx).await;
        });

        AuthActorHandle { tx }
    }

    async fn run(mut self, conn_rx: oneshot::Receiver<ConnectionActorHandle>) {
        info!(session = self.session_id, "Auth actor started");

        let connection = match conn_rx.await {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Err(e) = self.authenticate(&connection).await {
            info!(session = self.session_id, "Auth failed: {e}");
            let _ = connection.close().await;
        }
    }

    async fn authenticate(&mut self, connection: &ConnectionActorHandle) -> Result<()> {
        let msg = match self.rx.recv().await {
            Some(AuthCommand::ReceivePlayerMessage(msg)) => msg,
            None => return Err(ConnectionError::ConnectionClosed.into()),
        };

        let auth_token = match msg {
            ClientMessage::Login { auth_token } => auth_token,
            msg => {
                info!("{:?}", msg);
                let _ = connection.send_message(ServerMessage::LoginError).await;
                return Err(ConnectionError::WrongMessageType.into());
            }
        };

        let redeem_start = Instant::now();
        let player = match self.login_repo.redeem(&auth_token).await {
            Ok(p) => p,
            Err(e) => {
                let outcome = match &e {
                    LoginError::Rejected => {
                        info!(session = self.session_id, "Login rejected: {e}");
                        "rejected"
                    }
                    LoginError::Unavailable(detail) => {
                        error!(
                            session = self.session_id,
                            "Login service unavailable, refusing the login: {detail}"
                        );
                        "unavailable"
                    }
                };
                telemetry::metrics().record_login(outcome, redeem_start.elapsed());
                let _ = connection.send_message(ServerMessage::LoginError).await;
                return Err(e.into());
            }
        };

        let registry_guard = match self.world_ctx.online_registry.try_register(player.id) {
            Some(guard) => guard,
            None => {
                telemetry::metrics().record_login("already_online", redeem_start.elapsed());
                info!(
                    session = self.session_id,
                    "Character {} is already online.", player.name
                );
                let _ = connection.send_message(ServerMessage::LoginError).await;
                return Err(anyhow::anyhow!("Character {} is already online", player.id));
            }
        };
        telemetry::metrics().record_login("ok", redeem_start.elapsed());

        let session = SessionActor::start(
            self.session_id.clone(),
            connection.clone(),
            self.world_ctx.clone(),
            Agent::from_player(*player),
            registry_guard,
        );

        connection.set_session(session).await?;

        info!(
            session = self.session_id,
            "Auth complete, session handed off"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::chat::ChatActorHandle;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::persistence::PersistenceActorHandle;
    use crate::actors::world::WorldActorHandle;
    use crate::entities::map::GameMap;
    use crate::entities::player::PlayerId;
    use crate::game::{Tick, TickDelta};
    use crate::online_registry::OnlineRegistry;
    use crate::persistence::player::PlayerSnapshot;
    use crate::persistence::test_fixtures::a_test_snapshot;
    use arc_swap::ArcSwap;
    use std::time::Duration;
    use tokio::sync::mpsc::Receiver;

    /// A `LoginRepository` that answers from a script. No database, no network — the
    /// point of the seam is that this actor can be tested without either.
    struct FakeLogin {
        answer: Result<Box<PlayerSnapshot>, LoginError>,
    }

    impl FakeLogin {
        fn accepting(character_id: u32) -> Self {
            Self {
                answer: Ok(Box::new(a_test_snapshot(character_id, 1))),
            }
        }

        fn failing(error: LoginError) -> Self {
            Self { answer: Err(error) }
        }
    }

    impl LoginRepository for FakeLogin {
        async fn redeem(&self, _auth_token: &str) -> Result<Box<PlayerSnapshot>, LoginError> {
            match &self.answer {
                Ok(snapshot) => Ok(snapshot.clone()),
                Err(LoginError::Rejected) => Err(LoginError::Rejected),
                Err(LoginError::Unavailable(d)) => Err(LoginError::Unavailable(d.clone())),
            }
        }
    }

    /// Everything `AuthActor` needs besides the login repository. The receivers come back
    /// so the caller can keep them alive; a dropped receiver turns every send into a
    /// silent no-op and would make these tests pass regardless of behaviour.
    #[allow(clippy::type_complexity)]
    fn a_context() -> (
        SharedContext,
        Receiver<(crate::actors::world::WorldCommand, Option<TickDelta>)>,
        Receiver<crate::actors::persistence::PersistenceCommand>,
    ) {
        let (world, world_rx) = WorldActorHandle::for_test();
        let (persistence, persistence_rx) = PersistenceActorHandle::for_test(16);
        let (chat, _chat_rx) = ChatActorHandle::for_test();
        let (_tick_tx, tick_rx) = tokio::sync::watch::channel(Tick(0));

        (
            SharedContext {
                world,
                shared_map: Arc::new(ArcSwap::from_pointee(GameMap::new())),
                persistence: persistence.clone(),
                online_registry: OnlineRegistry::new(persistence),
                chat,
                tick_rx,
            },
            world_rx,
            persistence_rx,
        )
    }

    /// Runs the handshake with `first_message` and returns what the connection was told.
    async fn authenticate_with<L: LoginRepository + 'static>(
        login: L,
        ctx: SharedContext,
        first_message: ClientMessage,
    ) -> Vec<ConnectionCommand> {
        let (connection, mut connection_rx) = ConnectionActorHandle::for_test();
        let (conn_tx, conn_rx) = oneshot::channel();

        let handle = AuthActor::start("test-session".to_string(), conn_rx, Arc::new(login), ctx);
        conn_tx.send(connection).ok().unwrap();
        handle.receive_message(first_message).await.unwrap();

        // The actor sends, then exits; a short drain is enough and bounded so a
        // regression that sends nothing fails rather than hangs.
        let mut commands = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), connection_rx.recv()).await {
                Ok(Some(command)) => commands.push(command),
                Ok(None) => break,
                Err(_) if !commands.is_empty() => break,
                Err(_) => continue,
            }
        }
        commands
    }

    fn a_login() -> ClientMessage {
        ClientMessage::Login {
            auth_token: "a-token".to_string(),
        }
    }

    #[tokio::test]
    async fn a_successful_login_hands_the_connection_a_session() {
        let (ctx, _world_rx, _persistence_rx) = a_context();

        let commands = authenticate_with(FakeLogin::accepting(7), ctx, a_login()).await;

        assert!(
            commands
                .iter()
                .any(|c| matches!(c, ConnectionCommand::SetSession(_))),
            "the connection must be switched to the session actor, got {commands:?}"
        );
        assert!(
            !commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "a successful login must not also send LoginError, got {commands:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_login_sends_login_error_and_no_session() {
        let (ctx, _world_rx, _persistence_rx) = a_context();

        let commands =
            authenticate_with(FakeLogin::failing(LoginError::Rejected), ctx, a_login()).await;

        assert!(
            commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "got {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, ConnectionCommand::SetSession(_))),
            "a rejected login must not reach the world, got {commands:?}"
        );
    }

    /// Fail closed: the site being down must refuse the login rather than let the player
    /// in unauthenticated. The client is told the same thing as for a bad token, because
    /// it can do nothing with the difference.
    #[tokio::test]
    async fn an_unavailable_login_service_also_refuses_the_login() {
        let (ctx, _world_rx, _persistence_rx) = a_context();

        let commands = authenticate_with(
            FakeLogin::failing(LoginError::Unavailable("connection refused".into())),
            ctx,
            a_login(),
        )
        .await;

        assert!(
            commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "got {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, ConnectionCommand::SetSession(_))),
            "there is no SQL fallback, so this must not produce a session: {commands:?}"
        );
    }

    #[tokio::test]
    async fn a_first_message_that_is_not_login_is_refused() {
        let (ctx, _world_rx, _persistence_rx) = a_context();

        let commands = authenticate_with(FakeLogin::accepting(7), ctx, ClientMessage::Ping).await;

        assert!(
            commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "the protocol requires Login first, got {commands:?}"
        );
    }

    /// The duplicate-login guard sits after the redeem call, so a valid token for a
    /// character already in the world must still be refused.
    #[tokio::test]
    async fn a_character_already_online_is_refused() {
        let (ctx, _world_rx, _persistence_rx) = a_context();
        let _guard = ctx
            .online_registry
            .try_register(PlayerId(7))
            .expect("the first registration must succeed");

        let commands = authenticate_with(FakeLogin::accepting(7), ctx.clone(), a_login()).await;

        assert!(
            commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "got {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, ConnectionCommand::SetSession(_))),
            "a character cannot be online twice, got {commands:?}"
        );
    }

    /// The registry key comes from the loaded character, not from anything the client
    /// said. Before the token carried a character these were forced equal by the site's
    /// ownership check; now the client has no say at all, and this pins that.
    #[tokio::test]
    async fn the_session_registers_the_character_the_repository_returned() {
        let (ctx, _world_rx, _persistence_rx) = a_context();

        // Occupy the slot the repository's character will want.
        let _held = ctx
            .online_registry
            .try_register(PlayerId(7))
            .expect("the slot must start free");

        let commands = authenticate_with(FakeLogin::accepting(7), ctx, a_login()).await;

        assert!(
            commands.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::LoginError)
            )),
            "a second login for the repository's character must be refused, got {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, ConnectionCommand::SetSession(_))),
            "got {commands:?}"
        );
    }
}

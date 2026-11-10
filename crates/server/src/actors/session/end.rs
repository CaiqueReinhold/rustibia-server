use strum::IntoStaticStr;
use tracing::Level;

use super::SessionError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SessionEnd {
    Logout,
    ConnectionLost,
    Shutdown,
    Evicted,
    Protocol,
    Fault,
}

impl SessionEnd {
    pub fn reason(self) -> &'static str {
        self.into()
    }

    pub fn level(self) -> Level {
        match self {
            Self::Logout | Self::ConnectionLost | Self::Shutdown => Level::INFO,
            Self::Evicted | Self::Protocol => Level::WARN,
            Self::Fault => Level::ERROR,
        }
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        match error.downcast_ref::<SessionError>() {
            Some(SessionError::Logout) => Self::Logout,
            Some(SessionError::ConnectionClosed) => Self::ConnectionLost,
            Some(SessionError::WorldStopped) => Self::Shutdown,
            Some(SessionError::WrongMessageType | SessionError::InvalidState) => Self::Protocol,
            Some(SessionError::FailedToInitialize) | None => Self::Fault,
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use tracing::Level;

    use super::*;
    use crate::actors::session::{SessionActor, SessionActorHandle, SessionError};
    use crate::actors::world::WorldCommand;
    use crate::entities::agent::AgentKey;
    use crate::entities::map::GameMap;
    use crate::game::TickDelta;
    use crate::telemetry::testing::{CapturedEvent, capture_events};

    fn end_of(error: SessionError) -> SessionEnd {
        SessionEnd::from_error(&error.into())
    }

    #[test]
    fn a_logout_ends_quietly() {
        assert_eq!(end_of(SessionError::Logout), SessionEnd::Logout);
        assert_eq!(SessionEnd::Logout.level(), Level::INFO);
        assert_eq!(SessionEnd::Logout.reason(), "logout");
    }

    #[test]
    fn a_closed_connection_ends_quietly() {
        assert_eq!(
            end_of(SessionError::ConnectionClosed),
            SessionEnd::ConnectionLost
        );
        assert_eq!(SessionEnd::ConnectionLost.level(), Level::INFO);
        assert_eq!(SessionEnd::ConnectionLost.reason(), "connection_lost");
    }

    #[test]
    fn a_stopped_world_is_a_shutdown() {
        assert_eq!(end_of(SessionError::WorldStopped), SessionEnd::Shutdown);
        assert_eq!(SessionEnd::Shutdown.level(), Level::INFO);
        assert_eq!(SessionEnd::Shutdown.reason(), "shutdown");
    }

    #[test]
    fn an_eviction_warns() {
        assert_eq!(SessionEnd::Evicted.level(), Level::WARN);
        assert_eq!(SessionEnd::Evicted.reason(), "evicted");
    }

    #[test]
    fn a_client_out_of_order_warns() {
        assert_eq!(end_of(SessionError::WrongMessageType), SessionEnd::Protocol);
        assert_eq!(end_of(SessionError::InvalidState), SessionEnd::Protocol);
        assert_eq!(SessionEnd::Protocol.level(), Level::WARN);
        assert_eq!(SessionEnd::Protocol.reason(), "protocol");
    }

    #[test]
    fn anything_else_is_a_fault() {
        assert_eq!(end_of(SessionError::FailedToInitialize), SessionEnd::Fault);
        assert_eq!(
            SessionEnd::from_error(&anyhow!("a channel closed")),
            SessionEnd::Fault
        );
        assert_eq!(SessionEnd::Fault.level(), Level::ERROR);
        assert_eq!(SessionEnd::Fault.reason(), "fault");
    }

    async fn run_until_ended(
        session: SessionActor,
        world_rx: &mut tokio::sync::mpsc::Receiver<(WorldCommand, Option<TickDelta>)>,
    ) {
        tokio::join!(session.run(), async {
            world_rx.recv().await;
        });
    }

    fn session_ended(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
        events
            .iter()
            .filter(|event| event.message == "Session ended")
            .collect()
    }

    #[tokio::test]
    async fn a_dropped_connection_logs_one_info_line_and_no_error() {
        let events = capture_events();
        let (session, _connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(AgentKey::default(), GameMap::new());
        session.end.set(SessionEnd::ConnectionLost).unwrap();
        session.token.cancel();

        run_until_ended(session, &mut world_rx).await;

        let all = events.all();
        let ended = session_ended(&all);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].level, Level::INFO);
        assert_eq!(ended[0].fields["reason"], "connection_lost");
        assert_eq!(ended[0].fields["character"], "1");
        assert_eq!(ended[0].fields["name"], "test");
        assert!(all.iter().all(|event| event.level != Level::ERROR));
    }

    #[tokio::test]
    async fn an_eviction_logs_a_warning() {
        let events = capture_events();
        let (session, _connection_rx, mut world_rx, _tick_tx) =
            SessionActor::for_test(AgentKey::default(), GameMap::new());
        session.end.set(SessionEnd::Evicted).unwrap();
        session.token.cancel();

        run_until_ended(session, &mut world_rx).await;

        let all = events.all();
        let ended = session_ended(&all);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].level, Level::WARN);
        assert_eq!(ended[0].fields["reason"], "evicted");
    }

    #[test]
    fn the_first_close_reason_wins() {
        let (handle, _rx) = SessionActorHandle::for_test();

        handle.close(SessionEnd::Evicted);
        handle.close(SessionEnd::ConnectionLost);

        assert_eq!(handle.end.get(), Some(&SessionEnd::Evicted));
    }
}

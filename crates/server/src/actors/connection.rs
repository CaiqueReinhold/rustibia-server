//! The connection actor is responsible for framing and message
//! serialization/deserialization. It owns the raw [`TcpStream`] and translates
//! between bytes on the wire and typed commands.
//!
//! Incoming client messages are routed to whichever upstream actor is currently
//! active. On connection start the upstream is [`AuthActor`](super::auth::AuthActor);
//! once authentication completes a [`ConnectionCommand::SetSession`] switches it
//! to the [`SessionActor`](super::session::SessionActor) for the remainder of the
//! connection's lifetime.

use anyhow::Result;
use futures::sink::SinkExt;
use thiserror::Error;
use tokio::io::AsyncWrite;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::select;
use tokio::{net::TcpStream, sync::mpsc};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info};

use crate::actors::auth::AuthActorHandle;
use crate::actors::session::SessionActorHandle;
use crate::config::CONFIG;
use crate::messages::{
    ClientMessage, GameMessageCodec, MessageDecodeError, MessageEncodeError, ServerMessage,
};

#[derive(Clone, Debug)]
pub enum ConnectionCommand {
    Close,
    SendPlayerMessage(ServerMessage),
    SetSession(SessionActorHandle),
}

#[derive(Error, Debug)]
pub enum ConnectionError {
    #[error("Read error")]
    ReadError(#[from] MessageDecodeError),
    #[error("Server error")]
    ServerError,
    #[error("Connnection is closed")]
    ConnectionClosed,
    #[error("Wrong message type")]
    WrongMessageType,
}

enum Upstream {
    Auth(AuthActorHandle),
    Session(SessionActorHandle),
}

#[derive(Clone, Debug)]
pub struct ConnectionActorHandle {
    tx: mpsc::Sender<ConnectionCommand>,
}

impl ConnectionActorHandle {
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<ConnectionCommand>) {
        let (tx, rx) = mpsc::channel(16);
        (Self { tx }, rx)
    }

    pub async fn close(&self) -> Result<(), mpsc::error::SendError<ConnectionCommand>> {
        self.tx.send(ConnectionCommand::Close).await?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        msg: ServerMessage,
    ) -> Result<(), mpsc::error::SendError<ConnectionCommand>> {
        self.tx
            .send(ConnectionCommand::SendPlayerMessage(msg))
            .await?;
        Ok(())
    }

    pub async fn set_session(
        &self,
        session: SessionActorHandle,
    ) -> Result<(), mpsc::error::SendError<ConnectionCommand>> {
        self.tx.send(ConnectionCommand::SetSession(session)).await?;
        Ok(())
    }
}

pub struct ConnectionActor {
    session_id: String,
    rx: mpsc::Receiver<ConnectionCommand>,
    reader: FramedRead<OwnedReadHalf, GameMessageCodec>,
    writer: FramedWrite<OwnedWriteHalf, GameMessageCodec>,
    upstream: Upstream,
}

impl ConnectionActor {
    pub fn start(
        session_id: String,
        stream: TcpStream,
        auth: AuthActorHandle,
    ) -> ConnectionActorHandle {
        let (tx, rx) = mpsc::channel(CONFIG.max_buffered_messages);
        let (read, write) = stream.into_split();

        let actor = Self {
            session_id,
            rx,
            reader: FramedRead::new(read, GameMessageCodec {}),
            writer: FramedWrite::new(write, GameMessageCodec {}),
            upstream: Upstream::Auth(auth),
        };

        tokio::spawn(actor.run());

        ConnectionActorHandle { tx }
    }

    async fn run(mut self) {
        info!(session = self.session_id, "Connection started");
        // Reused across iterations so a busy connection is not reallocating a
        // batch buffer every wake-up. `drain` keeps the capacity.
        let mut commands = Vec::with_capacity(CONFIG.max_buffered_messages);
        loop {
            let result = select! {
                msg = self.reader.next() => self.handle_client_message(msg).await,
                // `recv_many` is cancel-safe the same way `recv` is: if the reader
                // branch wins, nothing was taken off the channel.
                _ = self.rx.recv_many(&mut commands, CONFIG.max_buffered_messages) =>
                    self.handle_connection_commands(&mut commands).await
            };
            if let Err(e) = result {
                error!(session = self.session_id, "Connection error: {e}");
                break;
            }
        }
        if let Upstream::Session(session) = self.upstream {
            session.close();
        }
        info!(session = self.session_id, "Connection finished");
    }

    async fn handle_client_message(
        &self,
        message: Option<Result<ClientMessage, MessageDecodeError>>,
    ) -> Result<(), ConnectionError> {
        let msg = message.ok_or(ConnectionError::ConnectionClosed)??;
        let send_result = match &self.upstream {
            Upstream::Auth(auth) => auth.receive_message(msg).await.is_err(),
            Upstream::Session(session) => session.receive_message(msg).await.is_err(),
        };
        if send_result {
            return Err(ConnectionError::ServerError);
        }
        Ok(())
    }

    /// Handles everything that was already queued, in order, and writes the
    /// player messages among them with a single flush.
    async fn handle_connection_commands(
        &mut self,
        commands: &mut Vec<ConnectionCommand>,
    ) -> Result<(), ConnectionError> {
        if commands.is_empty() {
            return Err(ConnectionError::ConnectionClosed);
        }

        let mut messages = Vec::with_capacity(commands.len());
        let mut closing = false;
        for cmd in commands.drain(..) {
            match cmd {
                ConnectionCommand::Close => {
                    closing = true;
                    // Anything queued behind a close is not ours to deliver, but
                    // what came before it is: those frames were accepted already.
                    break;
                }
                ConnectionCommand::SetSession(session) => {
                    info!(session = self.session_id, "Upstream switched to session");
                    self.upstream = Upstream::Session(session);
                }
                ConnectionCommand::SendPlayerMessage(msg) => {
                    messages.push(msg);
                }
            }
        }
        commands.clear();

        let flushed = write_batch(&mut self.writer, messages).await;

        if closing {
            return Err(ConnectionError::ConnectionClosed);
        }
        flushed.map_err(|_| ConnectionError::ServerError)
    }
}

/// Feeds every message into the codec's buffer and flushes once.
///
/// [`SinkExt::feed`] is `send` without the flush. `FramedWrite` still flushes on
/// its own once the buffer passes its backpressure boundary, so a large batch
/// cannot grow without bound.
///
/// Generic over the sink so the batching can be tested against a writer that
/// counts its writes; the actor itself owns a `TcpStream` half that cannot be
/// stood up in a unit test.
async fn write_batch<W>(
    writer: &mut FramedWrite<W, GameMessageCodec>,
    messages: impl IntoIterator<Item = ServerMessage>,
) -> Result<(), MessageEncodeError>
where
    W: AsyncWrite + Unpin,
{
    let mut wrote = false;
    for msg in messages {
        writer.feed(msg).await?;
        wrote = true;
    }
    // Flushing an empty buffer is harmless but pointless; skipping it keeps a
    // wake-up that carried only a `SetSession` off the socket entirely.
    if wrote {
        writer.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    /// An `AsyncWrite` that records how many times it was written to.
    ///
    /// This is the only way to see the property under test: how many `write`
    /// calls a batch turns into. With `TCP_NODELAY` on the listener each of those
    /// is its own segment, so the count is the thing that matters, not the bytes.
    #[derive(Default)]
    struct CountingWriter {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn framed() -> FramedWrite<CountingWriter, GameMessageCodec> {
        FramedWrite::new(CountingWriter::default(), GameMessageCodec {})
    }

    /// The point of the change: a whole batch costs one write, not one each.
    #[tokio::test]
    async fn a_batch_is_written_once() {
        let mut writer = framed();

        write_batch(&mut writer, vec![ServerMessage::Pong; 9])
            .await
            .unwrap();

        assert_eq!(
            writer.get_ref().writes,
            1,
            "nine messages must reach the socket in one write"
        );
    }

    /// The behaviour being replaced, pinned so the difference is not theoretical:
    /// `SinkExt::send` is feed + flush, so it pays a write per message.
    #[tokio::test]
    async fn sending_one_at_a_time_writes_once_per_message() {
        let mut writer = framed();

        for _ in 0..9 {
            writer.send(ServerMessage::Pong).await.unwrap();
        }

        assert_eq!(writer.get_ref().writes, 9);
    }

    /// Batching is a change to how bytes leave, never to which bytes leave.
    #[tokio::test]
    async fn batching_does_not_change_the_bytes() {
        let mut batched = framed();
        let mut one_at_a_time = framed();

        write_batch(&mut batched, vec![ServerMessage::Pong; 4])
            .await
            .unwrap();
        for _ in 0..4 {
            one_at_a_time.send(ServerMessage::Pong).await.unwrap();
        }

        assert_eq!(batched.get_ref().bytes, one_at_a_time.get_ref().bytes);
    }

    /// A wake-up carrying only a `SetSession` has nothing to send, and must not
    /// touch the socket at all.
    #[tokio::test]
    async fn an_empty_batch_does_not_write() {
        let mut writer = framed();

        write_batch(&mut writer, Vec::new()).await.unwrap();

        assert_eq!(writer.get_ref().writes, 0);
    }
}

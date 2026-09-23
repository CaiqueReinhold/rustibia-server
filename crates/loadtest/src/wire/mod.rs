mod codec;

pub use codec::WireError;

use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use rustibia_server::messages::{ClientMessage, ServerMessage};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use codec::LoadtestCodec;

pub struct Connection {
    framed: Framed<TcpStream, LoadtestCodec>,
}

impl Connection {
    pub async fn connect(addr: SocketAddr) -> Result<Self, WireError> {
        let socket = TcpStream::connect(addr).await?;
        socket.set_nodelay(true)?;
        Ok(Self {
            framed: Framed::new(socket, LoadtestCodec::default()),
        })
    }

    pub async fn send(&mut self, message: ClientMessage) -> Result<(), WireError> {
        self.framed.send(message).await
    }

    pub async fn next(&mut self) -> Option<Result<ServerMessage, WireError>> {
        self.framed.next().await
    }

    /// The wire length of every frame read so far, malformed or not — counted
    /// in `decode` before the payload is interpreted.
    pub fn bytes_read(&self) -> u64 {
        self.framed.codec().bytes_read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustibia_server::messages::GameMessageCodec as ServerCodec;
    use tokio::net::TcpListener;

    async fn pong_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            framed.send(ServerMessage::Pong).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn a_connection_round_trips_a_message() {
        let addr = pong_server().await;
        let mut conn = Connection::connect(addr).await.unwrap();

        conn.send(ClientMessage::Ping).await.unwrap();

        assert_eq!(conn.next().await.unwrap().unwrap(), ServerMessage::Pong);
    }
}

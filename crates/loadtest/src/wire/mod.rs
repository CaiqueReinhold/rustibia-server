mod codec;

pub use codec::WireError;

use std::sync::Arc;

use anyhow::Context as _;
use futures::{SinkExt, StreamExt};
use rustibia_server::messages::{ClientMessage, ServerMessage};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName, version::TLS13};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::codec::Framed;

use codec::LoadtestCodec;

/// Where the game server is, and what to trust it by: the public roots, plus `extra_ca`
/// when given.
#[derive(Clone)]
pub struct Dialer {
    address: String,
    name: ServerName<'static>,
    connector: TlsConnector,
}

impl Dialer {
    /// `server` is `host:port`; the certificate is checked against `host`.
    pub fn new(server: &str, extra_ca: Option<&str>) -> anyhow::Result<Self> {
        let (host, _port) = server
            .rsplit_once(':')
            .with_context(|| format!("{server} is not host:port"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let name = ServerName::try_from(host.to_string())
            .with_context(|| format!("{host} is not a valid server name"))?;

        let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = extra_ca {
            let pem = std::fs::read(path).with_context(|| format!("reading {path}"))?;
            let certs = rustls_pemfile::certs(&mut pem.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("parsing {path}"))?;
            anyhow::ensure!(!certs.is_empty(), "{path} contains no certificates");
            for cert in certs {
                roots
                    .add(cert)
                    .with_context(|| format!("{path} is not a usable CA"))?;
            }
        }

        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&TLS13])?
                .with_root_certificates(roots)
                .with_no_client_auth();

        Ok(Self {
            address: server.to_string(),
            name,
            connector: TlsConnector::from(Arc::new(config)),
        })
    }
}

pub struct Connection {
    framed: Framed<TlsStream<TcpStream>, LoadtestCodec>,
}

impl Connection {
    pub async fn connect(dialer: &Dialer) -> Result<Self, WireError> {
        let socket = TcpStream::connect(&dialer.address).await?;
        socket.set_nodelay(true)?;
        let socket = dialer
            .connector
            .connect(dialer.name.clone(), socket)
            .await?;
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
    use crate::testing::TestListener;
    use rustibia_server::messages::GameMessageCodec as ServerCodec;

    async fn pong_server() -> Dialer {
        let listener = TestListener::bind().await;
        let dialer = listener.dialer();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(socket, ServerCodec {});
            let _ = framed.next().await;
            framed.send(ServerMessage::Pong).await.unwrap();
        });
        dialer
    }

    #[tokio::test]
    async fn a_connection_round_trips_a_message() {
        let dialer = pong_server().await;
        let mut conn = Connection::connect(&dialer).await.unwrap();

        conn.send(ClientMessage::Ping).await.unwrap();

        assert_eq!(conn.next().await.unwrap().unwrap(), ServerMessage::Pong);
    }

    #[tokio::test]
    async fn a_server_outside_the_trusted_roots_is_refused() {
        let listener = TestListener::bind().await;
        let untrusting = Dialer::new(&listener.address(), None).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        assert!(Connection::connect(&untrusting).await.is_err());
    }

    #[test]
    fn an_address_without_a_port_is_refused() {
        assert!(Dialer::new("localhost", None).is_err());
    }
}

use std::{
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

use anyhow::Result;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use uuid::{NoContext, Timestamp};

use crate::{
    actors::{SharedContext, auth::AuthActor, connection::ConnectionActor},
    persistence::login::Login,
    telemetry,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// See `persistence::login`.
pub struct Context {
    pub login: Arc<Login>,
    pub shared_ctx: SharedContext,
}

pub struct Listener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl Listener {
    pub async fn bind(addr: SocketAddr, acceptor: TlsAcceptor) -> Result<Self> {
        let inner = TcpListener::bind(addr).await?;
        Ok(Self { inner, acceptor })
    }

    pub async fn listen(&self, context: Context) {
        let context = Arc::new(context);
        accept_loop(
            &self.inner,
            &self.acceptor,
            HANDSHAKE_TIMEOUT,
            move |stream| {
                let context = Arc::clone(&context);
                async move {
                    if let Err(e) = accept_connection(stream, &context).await {
                        error!("accept_connection failed: {e}")
                    }
                }
            },
        )
        .await
    }
}

/// Accepts for ever. Each socket's handshake runs on its own task, so `serve` sees only
/// completed handshakes and a client that stalls one holds up nobody else.
async fn accept_loop<F, Fut>(
    listener: &TcpListener,
    acceptor: &TlsAcceptor,
    handshake_timeout: Duration,
    serve: F,
) where
    F: Fn(TlsStream<TcpStream>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!("Failed to accept connection: {e}");
                continue;
            }
        };
        info!("new connection from {:?}", addr);
        // Movement frames are a few bytes each. Nagle would hold them
        // until the previous segment is acknowledged, adding up to a
        // round trip of variable delay to every walk.
        if let Err(e) = stream.set_nodelay(true) {
            warn!("could not disable Nagle for {addr:?}: {e}");
        }

        let acceptor = acceptor.clone();
        let serve = serve.clone();
        tokio::spawn(async move {
            if let Some(stream) = handshake(&acceptor, stream, addr, handshake_timeout).await {
                serve(stream).await;
            }
        });
    }
}

async fn handshake(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
    addr: SocketAddr,
    limit: Duration,
) -> Option<TlsStream<TcpStream>> {
    let start = Instant::now();
    let (outcome, stream) = match tokio::time::timeout(limit, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => ("ok", Some(stream)),
        Ok(Err(e)) => {
            warn!("TLS handshake with {addr:?} failed: {e}");
            ("error", None)
        }
        Err(_) => {
            warn!("TLS handshake with {addr:?} timed out");
            ("timeout", None)
        }
    };
    telemetry::metrics().record_handshake(outcome, start.elapsed());
    stream
}

async fn accept_connection(stream: TlsStream<TcpStream>, context: &Context) -> Result<()> {
    let session_id = uuid::Uuid::new_v7(Timestamp::now(NoContext)).to_string();
    let (conn_tx, conn_rx) = oneshot::channel();

    let auth = AuthActor::start(
        session_id.clone(),
        conn_rx,
        context.login.clone(),
        context.shared_ctx.clone(),
    );
    let connection = ConnectionActor::start(session_id, stream, auth);

    if let Err(conn) = conn_tx.send(connection) {
        info!("failed to open connection");
        conn.close().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_tls::{
        self, GameCertResolver,
        testing::{TestCerts, dial},
    };
    use rustls::version::{TLS12, TLS13};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    async fn echo_server(
        certs: &TestCerts,
        handshake_timeout: Duration,
    ) -> (SocketAddr, Arc<GameCertResolver>) {
        let resolver = certs.resolver();
        let acceptor = game_tls::acceptor(Arc::clone(&resolver)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            accept_loop(
                &listener,
                &acceptor,
                handshake_timeout,
                |mut stream| async move {
                    let mut buf = [0; 64];
                    while let Ok(n @ 1..) = stream.read(&mut buf).await {
                        if stream.write_all(&buf[..n]).await.is_err()
                            || stream.flush().await.is_err()
                        {
                            break;
                        }
                    }
                },
            )
            .await
        });
        (addr, resolver)
    }

    async fn assert_echoes(stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) {
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");
    }

    #[tokio::test]
    async fn a_trusted_client_round_trips_over_tls_13() {
        let certs = TestCerts::generate();
        let (addr, _) = echo_server(&certs, HANDSHAKE_TIMEOUT).await;

        let mut stream = dial(addr, &certs.connector(&TLS13)).await.unwrap();

        assert_eq!(
            stream.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        assert_echoes(&mut stream).await;
    }

    #[tokio::test]
    async fn a_tls_12_client_is_refused() {
        let certs = TestCerts::generate();
        let (addr, _) = echo_server(&certs, HANDSHAKE_TIMEOUT).await;

        assert!(dial(addr, &certs.connector(&TLS12)).await.is_err());
    }

    #[tokio::test]
    async fn a_plaintext_client_is_dropped_and_the_next_is_served() {
        let certs = TestCerts::generate();
        let (addr, _) = echo_server(&certs, HANDSHAKE_TIMEOUT).await;

        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut rest = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(5), plain.read_to_end(&mut rest)).await;

        assert!(
            closed.is_ok(),
            "a connection that is not TLS must be closed"
        );
        assert_echoes(&mut dial(addr, &certs.connector(&TLS13)).await.unwrap()).await;
    }

    #[tokio::test]
    async fn a_silent_client_does_not_hold_up_the_next() {
        let certs = TestCerts::generate();
        let (addr, _) = echo_server(&certs, Duration::from_secs(60)).await;
        let _silent = TcpStream::connect(addr).await.unwrap();

        let served = tokio::time::timeout(Duration::from_secs(5), async {
            assert_echoes(&mut dial(addr, &certs.connector(&TLS13)).await.unwrap()).await
        })
        .await;

        assert!(
            served.is_ok(),
            "a pending handshake must not block the accept loop"
        );
    }

    #[tokio::test]
    async fn a_silent_client_is_dropped_at_the_handshake_timeout() {
        let certs = TestCerts::generate();
        let (addr, _) = echo_server(&certs, Duration::from_millis(200)).await;
        let mut silent = TcpStream::connect(addr).await.unwrap();

        let mut rest = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut rest)).await;

        assert!(
            closed.is_ok(),
            "the handshake timeout must close the socket"
        );
    }

    #[tokio::test]
    async fn a_reload_changes_new_connections_and_spares_open_ones() {
        let current = TestCerts::generate();
        let renewed = TestCerts::generate();
        let (addr, resolver) = echo_server(&current, HANDSHAKE_TIMEOUT).await;
        let mut open = dial(addr, &current.connector(&TLS13)).await.unwrap();
        assert_echoes(&mut open).await;

        current.renew_from(&renewed);
        resolver.reload().unwrap();

        assert!(
            dial(addr, &current.connector(&TLS13)).await.is_err(),
            "the replaced certificate must no longer be presented"
        );
        assert_echoes(&mut dial(addr, &renewed.connector(&TLS13)).await.unwrap()).await;
        assert_echoes(&mut open).await;
    }
}

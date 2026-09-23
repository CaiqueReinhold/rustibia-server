use std::net::SocketAddr;
use tracing::{error, info, warn};

use anyhow::Result;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use uuid::{NoContext, Timestamp};

use std::sync::Arc;

use crate::{
    actors::{SharedContext, auth::AuthActor, connection::ConnectionActor},
    persistence::login::LoginRepository,
};

/// See `persistence::login`.
pub struct Context<L: LoginRepository> {
    pub login_repo: Arc<L>,
    pub shared_ctx: SharedContext,
}

pub struct Listener {
    inner: tokio::net::TcpListener,
}

impl Listener {
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        let inner = TcpListener::bind(addr).await?;
        Ok(Self { inner })
    }

    pub async fn listen<L: LoginRepository + 'static>(&self, context: Context<L>) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    info!("new connection from {:?}", addr);
                    // Movement frames are a few bytes each. Nagle would hold them
                    // until the previous segment is acknowledged, adding up to a
                    // round trip of variable delay to every walk.
                    if let Err(e) = stream.set_nodelay(true) {
                        warn!("could not disable Nagle for {addr:?}: {e}");
                    }
                    if let Err(e) = Self::accept_connection(stream, &context).await {
                        error!("accept_connection failed: {e}")
                    }
                }
                Err(e) => {
                    warn!("Failed to accept connection: {e}");
                }
            }
        }
    }

    async fn accept_connection<L: LoginRepository + 'static>(
        stream: TcpStream,
        context: &Context<L>,
    ) -> Result<()> {
        let session_id = uuid::Uuid::new_v7(Timestamp::now(NoContext)).to_string();
        let (conn_tx, conn_rx) = oneshot::channel();

        let auth = AuthActor::start(
            session_id.clone(),
            conn_rx,
            context.login_repo.clone(),
            context.shared_ctx.clone(),
        );
        let connection = ConnectionActor::start(session_id, stream, auth);

        if let Err(conn) = conn_tx.send(connection) {
            info!("failed to open connection");
            conn.close().await?;
        }
        Ok(())
    }
}

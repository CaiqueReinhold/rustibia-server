//! The game socket's certificate, and swapping it while players are connected.

use std::{fs::File, io::BufReader, sync::Arc};

use arc_swap::ArcSwap;
use rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    version::TLS13,
};
use thiserror::Error;
use tokio::signal::unix::Signal;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info};

#[derive(Debug, Error)]
pub enum GameTlsError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("{0} contains no certificates")]
    NoCertificates(String),
    #[error("{0} contains no private key")]
    NoPrivateKey(String),
    #[error("{cert} and {key} are not a usable pair: {source}")]
    Pair {
        cert: String,
        key: String,
        source: rustls::Error,
    },
}

/// Presents whichever certificate was loaded last. A handshake reads it once, so a reload
/// changes only connections that have not handshaken yet.
#[derive(Debug)]
pub struct GameCertResolver {
    cert: String,
    key: String,
    current: ArcSwap<CertifiedKey>,
}

impl GameCertResolver {
    pub fn load(cert: &str, key: &str) -> Result<Self, GameTlsError> {
        Ok(Self {
            current: ArcSwap::from_pointee(certified_key(cert, key)?),
            cert: cert.to_string(),
            key: key.to_string(),
        })
    }

    /// Re-reads both files. On failure the certificate already in use stays.
    pub fn reload(&self) -> Result<(), GameTlsError> {
        self.current
            .store(Arc::new(certified_key(&self.cert, &self.key)?));
        Ok(())
    }
}

impl ResolvesServerCert for GameCertResolver {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

pub fn acceptor(resolver: Arc<GameCertResolver>) -> Result<TlsAcceptor, rustls::Error> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub fn reload_on(mut hangup: Signal, resolver: Arc<GameCertResolver>) {
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            match resolver.reload() {
                Ok(()) => info!("Reloaded the game certificate"),
                Err(e) => error!("Keeping the previous game certificate: {e}"),
            }
        }
    });
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certified_key(cert: &str, key: &str) -> Result<CertifiedKey, GameTlsError> {
    CertifiedKey::from_der(load_certs(cert)?, load_key(key)?, &provider()).map_err(|source| {
        GameTlsError::Pair {
            cert: cert.to_string(),
            key: key.to_string(),
            source,
        }
    })
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, GameTlsError> {
    let mut reader = BufReader::new(open(path)?);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|e| GameTlsError::Read(path.to_string(), e))?;

    if certs.is_empty() {
        return Err(GameTlsError::NoCertificates(path.to_string()));
    }
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, GameTlsError> {
    let mut reader = BufReader::new(open(path)?);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| GameTlsError::Read(path.to_string(), e))?
        .ok_or_else(|| GameTlsError::NoPrivateKey(path.to_string()))
}

fn open(path: &str) -> Result<File, GameTlsError> {
    File::open(path).map_err(|e| GameTlsError::Read(path.to_string(), e))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::{net::SocketAddr, sync::Arc};

    use rustibia_certgen::{CA_CERT, GAME_CERT, GAME_KEY};
    use rustls::{ClientConfig, RootCertStore, SupportedProtocolVersion, pki_types::ServerName};
    use tokio::net::TcpStream;
    use tokio_rustls::{TlsConnector, client::TlsStream};

    use super::{GameCertResolver, load_certs, provider};

    pub struct TestCerts {
        dir: tempfile::TempDir,
    }

    impl TestCerts {
        pub fn generate() -> Self {
            let dir = tempfile::tempdir().unwrap();
            rustibia_certgen::generate_bundle(dir.path()).unwrap();
            Self { dir }
        }

        pub fn path(&self, name: &str) -> String {
            self.dir.path().join(name).display().to_string()
        }

        pub fn resolver(&self) -> Arc<GameCertResolver> {
            Arc::new(GameCertResolver::load(&self.path(GAME_CERT), &self.path(GAME_KEY)).unwrap())
        }

        /// A client that trusts this bundle's CA and nothing else.
        pub fn connector(&self, version: &'static SupportedProtocolVersion) -> TlsConnector {
            let mut roots = RootCertStore::empty();
            for ca in load_certs(&self.path(CA_CERT)).unwrap() {
                roots.add(ca).unwrap();
            }
            let config = ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(&[version])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            TlsConnector::from(Arc::new(config))
        }

        /// Overwrites this bundle's game certificate and key with `other`'s, as a renewal does.
        pub fn renew_from(&self, other: &TestCerts) {
            for name in [GAME_CERT, GAME_KEY] {
                std::fs::copy(other.path(name), self.path(name)).unwrap();
            }
        }
    }

    pub async fn dial(
        addr: SocketAddr,
        connector: &TlsConnector,
    ) -> std::io::Result<TlsStream<TcpStream>> {
        let stream = TcpStream::connect(addr).await?;
        connector
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustibia_certgen::{GAME_CERT, GAME_KEY};
    use testing::TestCerts;

    #[test]
    fn a_generated_pair_loads() {
        let certs = TestCerts::generate();

        assert!(GameCertResolver::load(&certs.path(GAME_CERT), &certs.path(GAME_KEY)).is_ok());
    }

    #[test]
    fn a_missing_certificate_is_refused() {
        let certs = TestCerts::generate();

        let err = GameCertResolver::load(&certs.path("nope.crt"), &certs.path(GAME_KEY))
            .expect_err("a server without its certificate must not start");

        assert!(matches!(err, GameTlsError::Read(_, _)), "got {err:?}");
    }

    #[test]
    fn a_key_from_another_certificate_is_refused() {
        let ours = TestCerts::generate();
        let theirs = TestCerts::generate();

        let err = GameCertResolver::load(&ours.path(GAME_CERT), &theirs.path(GAME_KEY))
            .expect_err("a mismatched pair fails every handshake, so it must fail the load");

        assert!(matches!(err, GameTlsError::Pair { .. }), "got {err:?}");
    }

    #[test]
    fn a_failed_reload_keeps_the_current_certificate() {
        let certs = TestCerts::generate();
        let resolver = certs.resolver();
        let before = resolver.current.load_full();
        std::fs::write(certs.path(GAME_CERT), b"not a certificate").unwrap();

        assert!(resolver.reload().is_err());
        assert!(Arc::ptr_eq(&before, &resolver.current.load_full()));
    }
}

//! Every call the game server makes to the site's internal API.

use std::time::Duration;

use rustibia_contract::{CharacterRecord, RedeemRequest, SaveBatch, SaveResults};
use thiserror::Error;

/// How long to wait on the site before treating it as unavailable.
///
/// Short on purpose: a player may be sitting on a connecting screen, and a login that is
/// going to fail should fail while they are still watching rather than after they have
/// given up and retried.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum SiteError {
    #[error("not found")]
    NotFound,
    #[error("site unavailable: {0}")]
    Unavailable(String),
}

pub struct SiteClient {
    client: reqwest::Client,
    base_url: String,
}

impl SiteClient {
    /// `base_url` is the site's internal origin, e.g. `https://localhost:8443`.
    pub fn new(base_url: &str, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Builds the mutual-TLS client, or fails.
    ///
    /// `add_root_certificate` with our CA and nothing else is deliberate — this client
    /// talks to exactly one host, and trusting the public root store would mean any CA on
    /// earth could impersonate the site. The identity is the other half: without it the
    /// site's verifier closes the connection.
    pub fn build_client(cert: &str, key: &str, ca: &str) -> Result<reqwest::Client, ClientError> {
        let mut identity = read(cert)?;
        identity.extend_from_slice(b"\n");
        identity.extend_from_slice(&read(key)?);

        let identity = reqwest::Identity::from_pem(&identity)
            .map_err(|e| ClientError::Identity(format!("{cert} + {key}"), e))?;
        let ca_cert = reqwest::Certificate::from_pem(&read(ca)?)
            .map_err(|e| ClientError::Certificate(ca.to_string(), e))?;

        reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca_cert)
            .identity(identity)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ClientError::Build)
    }

    pub async fn redeem(&self, auth_token: &str) -> Result<CharacterRecord, SiteError> {
        let request = RedeemRequest {
            auth_token: auth_token.to_string(),
        };
        let response = self
            .client
            .post(self.url("/internal/game-tokens/redeem"))
            .json(&request)
            .send()
            .await;
        parse(accept(response).await?).await
    }

    pub async fn save(&self, batch: &SaveBatch) -> Result<SaveResults, SiteError> {
        let response = self
            .client
            .post(self.url("/internal/saves"))
            .json(batch)
            .send()
            .await;
        parse(accept(response).await?).await
    }

    pub async fn mark_online(&self, character_id: i32) -> Result<(), SiteError> {
        let response = self
            .client
            .post(self.url(&format!("/internal/online/{character_id}")))
            .send()
            .await;
        accept(response).await.map(drop)
    }

    pub async fn mark_offline(&self, character_id: i32) -> Result<(), SiteError> {
        let response = self
            .client
            .delete(self.url(&format!("/internal/online/{character_id}")))
            .send()
            .await;
        accept(response).await.map(drop)
    }

    pub async fn reset_online(&self) -> Result<(), SiteError> {
        let response = self
            .client
            .post(self.url("/internal/online/reset"))
            .send()
            .await;
        accept(response).await.map(drop)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

async fn accept(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<reqwest::Response, SiteError> {
    let response = response.map_err(|e| SiteError::Unavailable(e.to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(SiteError::NotFound);
    }
    let body = response.text().await.unwrap_or_default();
    Err(SiteError::Unavailable(format!("the site answered {status}: {body}")))
}

async fn parse<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T, SiteError> {
    response
        .json()
        .await
        .map_err(|e| SiteError::Unavailable(format!("unparseable response: {e}")))
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("{0} is not a usable client identity: {1}")]
    Identity(String, reqwest::Error),
    #[error("{0} is not a usable certificate authority: {1}")]
    Certificate(String, reqwest::Error),
    #[error("building the internal HTTP client failed: {0}")]
    Build(reqwest::Error),
}

fn read(path: &str) -> Result<Vec<u8>, ClientError> {
    std::fs::read(path).map_err(|e| ClientError::Read(path.to_string(), e))
}

#[cfg(test)]
mod tests {
    use rustibia_contract::{SaveOutcome, SaveResult};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn a_site(server: &MockServer) -> SiteClient {
        SiteClient::new(&server.uri(), reqwest::Client::new())
    }

    #[test]
    fn urls_are_built_without_a_double_slash() {
        let site = SiteClient::new("https://site:8443/", reqwest::Client::new());

        assert_eq!(site.url("/internal/saves"), "https://site:8443/internal/saves");
    }

    #[tokio::test]
    async fn a_save_batch_returns_the_sites_outcomes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/saves"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "id": 7, "outcome": "stale" }]
            })))
            .mount(&server)
            .await;

        let results = a_site(&server)
            .save(&SaveBatch { characters: Vec::new() })
            .await
            .unwrap();

        assert_eq!(
            results.results,
            vec![SaveResult { id: 7, outcome: SaveOutcome::Stale }]
        );
    }

    #[tokio::test]
    async fn a_failed_save_is_unavailable_and_carries_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/saves"))
            .respond_with(ResponseTemplate::new(422).set_body_string("bad batch"))
            .mount(&server)
            .await;

        let error = a_site(&server)
            .save(&SaveBatch { characters: Vec::new() })
            .await
            .unwrap_err();

        assert!(
            matches!(&error, SiteError::Unavailable(detail) if detail.contains("422") && detail.contains("bad batch"))
        );
    }

    #[tokio::test]
    async fn online_events_hit_their_routes() {
        let server = MockServer::start().await;
        for (verb, route) in [
            ("POST", "/internal/online/7"),
            ("DELETE", "/internal/online/7"),
            ("POST", "/internal/online/reset"),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
        }
        let site = a_site(&server);

        site.mark_online(7).await.unwrap();
        site.mark_offline(7).await.unwrap();
        site.reset_online().await.unwrap();
    }

    #[tokio::test]
    async fn a_redeem_404_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/game-tokens/redeem"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(matches!(a_site(&server).redeem("t").await, Err(SiteError::NotFound)));
    }


    struct Certs {
        dir: std::path::PathBuf,
    }

    impl Certs {
        fn generate(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rustibia-server-tls-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            rustibia_certgen::generate_bundle(&dir).unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> String {
            self.dir.join(name).display().to_string()
        }
    }

    impl Drop for Certs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn build_client_accepts_a_generated_bundle() {
        let certs = Certs::generate("ok");

        assert!(
            SiteClient::build_client(
                &certs.path("server.crt"),
                &certs.path("server.key"),
                &certs.path("ca.crt"),
            )
            .is_ok()
        );
    }

    #[test]
    fn build_client_fails_on_a_missing_identity() {
        let certs = Certs::generate("missing-identity");

        let err = SiteClient::build_client(
            &certs.path("nope.crt"),
            &certs.path("server.key"),
            &certs.path("ca.crt"),
        )
        .expect_err("a missing client certificate must stop the process at boot");

        assert!(matches!(err, ClientError::Read(_, _)), "got {err:?}");
    }

    #[test]
    fn build_client_fails_on_a_missing_ca() {
        let certs = Certs::generate("missing-ca");

        assert!(
            SiteClient::build_client(
                &certs.path("server.crt"),
                &certs.path("server.key"),
                &certs.path("nope.crt"),
            )
            .is_err(),
            "without the CA this client would have to trust anything claiming to be the site"
        );
    }

    #[test]
    fn build_client_fails_on_a_key_that_is_not_pem() {
        let certs = Certs::generate("garbage-key");
        let garbage = certs.path("garbage.key");
        std::fs::write(&garbage, b"not a key").unwrap();

        assert!(
            SiteClient::build_client(
                &certs.path("server.crt"),
                &garbage,
                &certs.path("ca.crt"),
            )
            .is_err()
        );
    }
}

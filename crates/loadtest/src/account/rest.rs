use anyhow::{Context as _, Result};
use rustibia_server::entities::vocation::Vocation;
use serde::Deserialize;

#[derive(Clone)]
pub struct Session {
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct CharacterId(pub i32);

#[derive(Debug, Deserialize)]
pub struct Character {
    pub id: CharacterId,
    pub name: String,
}

#[derive(Deserialize)]
struct AuthResponse {
    session_token: String,
}

#[derive(Deserialize)]
struct GameTokenResponse {
    auth_token: String,
}

#[derive(Clone)]
pub struct SiteClient {
    base: String,
    http: reqwest::Client,
}

/// Without this, a stalled site leaves a bot task blocked on `game_token`
/// forever — never counted, never joined, and holding the metrics channel
/// open so `run` never sees it close.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl SiteClient {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()?,
        })
    }

    pub async fn authenticate(&self, email: &str, password: &str) -> Result<Session> {
        let response: AuthResponse = self
            .http
            .post(format!("{}/api/auth", self.base))
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()
            .await?
            .error_for_status()
            .context("authenticating against the site")?
            .json()
            .await?;

        Ok(Session {
            token: response.session_token,
        })
    }

    pub async fn characters(&self, session: &Session) -> Result<Vec<Character>> {
        Ok(self
            .http
            .get(format!("{}/api/characters", self.base))
            .bearer_auth(&session.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn game_token(&self, session: &Session, character_id: CharacterId) -> Result<String> {
        let character_id = character_id.0;
        let response: GameTokenResponse = self
            .http
            .post(format!("{}/api/characters/{character_id}/token", self.base))
            .bearer_auth(&session.token)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("issuing a game token for character {character_id}"))?
            .json()
            .await?;

        Ok(response.auth_token)
    }

    /// Posts the account page's own form. The session extractor accepts a bearer
    /// token on the web routes as well as the API ones, so no cookie jar is needed.
    pub async fn create_character(
        &self,
        session: &Session,
        name: &str,
        vocation: Vocation,
    ) -> Result<()> {
        let vocation = (vocation as i16).to_string();
        let response = self
            .http
            .post(format!("{}/account/characters/new", self.base))
            .bearer_auth(&session.token)
            .form(&[
                ("name", name),
                ("vocation", vocation.as_str()),
                ("sex", "1"),
            ])
            .send()
            .await?;

        if response.status().is_success() || response.status().is_redirection() {
            return Ok(());
        }

        anyhow::bail!("character creation refused: {}", response.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn authenticating_returns_a_session_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_token": "sess-1",
                "expires_at": "2026-09-23T00:00:00Z"
            })))
            .mount(&server)
            .await;

        let site = SiteClient::new(&server.uri()).unwrap();
        let session = site.authenticate("a@b.c", "pw").await.unwrap();

        assert_eq!(session.token, "sess-1");
    }

    #[tokio::test]
    async fn characters_are_listed_with_the_session_as_a_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/characters"))
            .and(header("authorization", "Bearer sess-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 7, "name": "Loadbot Aab", "level": 20, "vocation": "Knight" }
            ])))
            .mount(&server)
            .await;

        let site = SiteClient::new(&server.uri()).unwrap();
        let characters = site
            .characters(&Session {
                token: "sess-1".into(),
            })
            .await
            .unwrap();

        assert_eq!(characters.len(), 1);
        assert_eq!(characters[0].id, CharacterId(7));
        assert_eq!(characters[0].name, "Loadbot Aab");
    }

    #[tokio::test]
    async fn a_game_token_is_issued_per_character() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/characters/7/token"))
            .and(header("authorization", "Bearer sess-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "auth_token": "game-xyz",
                "expires_at": "2026-09-22T00:01:00Z"
            })))
            .mount(&server)
            .await;

        let site = SiteClient::new(&server.uri()).unwrap();
        let token = site
            .game_token(
                &Session {
                    token: "sess-1".into(),
                },
                CharacterId(7),
            )
            .await
            .unwrap();

        assert_eq!(token, "game-xyz");
    }

    #[tokio::test]
    async fn a_redirect_from_creation_is_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/account/characters/new"))
            .respond_with(ResponseTemplate::new(303).insert_header("Location", "/account"))
            .mount(&server)
            .await;

        let site = SiteClient::new(&server.uri()).unwrap();
        let result = site
            .create_character(
                &Session {
                    token: "sess-1".into(),
                },
                "Loadbot Aab",
                Vocation::Sorcerer,
            )
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn a_refused_creation_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/account/characters/new"))
            .respond_with(ResponseTemplate::new(422))
            .mount(&server)
            .await;

        let site = SiteClient::new(&server.uri()).unwrap();
        let result = site
            .create_character(
                &Session {
                    token: "sess-1".into(),
                },
                "Loadbot Aab",
                Vocation::Sorcerer,
            )
            .await;

        assert!(result.is_err());
    }
}

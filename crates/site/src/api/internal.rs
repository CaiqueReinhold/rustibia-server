//! The API the game server calls, and nobody else.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rustibia_contract::{CharacterRecord, MAX_SAVE_BATCH, RedeemRequest, SaveBatch, SaveResults};

use crate::{
    db::{login, online, saves},
    error::AppError,
    state::AppState,
};

/// Spends an auth token and returns the character the game server should load.
pub async fn post_redeem(
    State(state): State<AppState>,
    Json(request): Json<RedeemRequest>,
) -> Result<Json<CharacterRecord>, AppError> {
    match login::redeem(&state.pool, &request.auth_token).await? {
        Some(record) => Ok(Json(record)),
        None => Err(AppError::NotFound),
    }
}

pub async fn post_saves(
    State(state): State<AppState>,
    Json(batch): Json<SaveBatch>,
) -> Result<Json<SaveResults>, AppError> {
    if batch.characters.len() > MAX_SAVE_BATCH {
        return Err(AppError::Validation(format!(
            "a save batch holds at most {MAX_SAVE_BATCH} characters"
        )));
    }
    Ok(Json(saves::apply(&state.pool, &batch.characters).await?))
}

pub async fn post_online(
    State(state): State<AppState>,
    Path(character_id): Path<i32>,
) -> Result<StatusCode, AppError> {
    online::mark_online(&state.pool, character_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_online(
    State(state): State<AppState>,
    Path(character_id): Path<i32>,
) -> Result<StatusCode, AppError> {
    online::mark_offline(&state.pool, character_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn post_online_reset(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    online::reset(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/game-tokens/redeem", axum::routing::post(post_redeem))
        .route("/saves", axum::routing::post(post_saves))
        .route("/online/reset", axum::routing::post(post_online_reset))
        .route(
            "/online/{id}",
            axum::routing::post(post_online).delete(delete_online),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, characters},
        domain::{sex::Sex, vocation::Vocation},
    };
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use time::{Duration, OffsetDateTime};
    use tower::ServiceExt;

    /// The internal router with TLS bypassed. Every test here is about the handler's
    /// answers; that the route is unreachable without a client certificate is proved in
    /// `internal_tls`, where the listener actually exists.
    fn test_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .nest("/internal", router())
            .with_state(AppState { pool, config })
    }

    fn redeem_req(token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/internal/game-tokens/redeem")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&RedeemRequest {
                    auth_token: token.to_string(),
                })
                .unwrap(),
            ))
            .unwrap()
    }

    async fn send(app: Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn an_account(pool: &PgPool, email: &str) -> i32 {
        create_account(pool, email, "hunter2hunter2")
            .await
            .unwrap()
            .id
    }

    async fn a_character(pool: &PgPool, account_id: i32, name: &str) -> i32 {
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        characters::create(
            pool,
            account_id,
            name,
            Vocation::Paladin,
            Sex::Male,
            &template,
        )
        .await
        .unwrap()
    }

    async fn a_token(pool: &PgPool, character_id: i32) -> String {
        let token = format!("token-{}", uuid::Uuid::now_v7());
        sqlx::query(
            "INSERT INTO game_tokens (token_hash, character_id, valid_until) VALUES ($1, $2, $3)",
        )
        .bind(crate::auth::token::hash_token(&token))
        .bind(character_id)
        .bind(OffsetDateTime::now_utc() + Duration::seconds(60))
        .execute(pool)
        .await
        .unwrap();
        token
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_valid_redemption_returns_the_record(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id).await;

        let (status, body) = send(test_app(pool), redeem_req(&token)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], character_id);
        assert_eq!(body["name"], "Rizael");
        assert!(body["position"]["x"].is_i64());
        assert!(!body["skills"].as_array().unwrap().is_empty());
    }

    /// The response has to deserialize into the contract type the game server parses,
    /// not merely into "some JSON". A field the site forgot to send would pass an
    /// assertion on `body["name"]` and fail on the game server at login time.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_body_deserializes_as_the_contract_type(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id).await;

        let (_, body) = send(test_app(pool), redeem_req(&token)).await;

        let record: CharacterRecord = serde_json::from_value(body)
            .expect("the response must satisfy rustibia_contract::CharacterRecord");
        assert_eq!(record.id, character_id);
    }

    /// What used to be the id-oracle test. A caller can no longer name a character, so
    /// there is no pair of refusals to compare; what is left to prove is that the token
    /// alone decides, and that it decides correctly with more than one candidate.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_token_alone_decides_which_character_loads(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let first_id = a_character(&pool, account_id, "Rizael").await;
        let second_id = a_character(&pool, account_id, "Anaia").await;
        let second_token = a_token(&pool, second_id).await;

        let (status, body) = send(test_app(pool), redeem_req(&second_token)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], second_id);
        assert_ne!(body["id"], first_id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_token_is_404(pool: PgPool) {
        let (status, _) = send(test_app(pool), redeem_req("never-issued")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_second_redemption_of_the_same_token_is_404(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let token = a_token(&pool, character_id).await;

        let (first, _) = send(test_app(pool.clone()), redeem_req(&token)).await;
        let (second, _) = send(test_app(pool), redeem_req(&token)).await;

        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_malformed_body_is_rejected(pool: PgPool) {
        let request = Request::builder()
            .method("POST")
            .uri("/internal/game-tokens/redeem")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{}"#))
            .unwrap();

        let (status, _) = send(test_app(pool), request).await;

        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a request missing auth_token must not be treated as the empty token"
        );
    }

    fn saves_req(batch: &rustibia_contract::SaveBatch) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/internal/saves")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(batch).unwrap()))
            .unwrap()
    }

    fn a_save_for(id: i32) -> rustibia_contract::CharacterSave {
        use rustibia_contract::{Coords, Outfit, PoolValue};
        rustibia_contract::CharacterSave {
            id,
            save_version: 1,
            position: Coords { x: 1030, y: 1031, z: 7 },
            origin: Coords { x: 1028, y: 1028, z: 7 },
            facing: 1,
            life: PoolValue { current: 37, maximum: 150 },
            mana: PoolValue { current: 5, maximum: 20 },
            capacity: 390,
            speed: 220,
            outfit: Outfit { id: 128, head: 1, body: 2, legs: 3, feet: 4 },
            skills: Vec::new(),
            inventory: std::collections::HashMap::new(),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_save_batch_answers_with_the_contract_type(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let batch = rustibia_contract::SaveBatch { characters: vec![a_save_for(character_id)] };

        let (status, body) = send(test_app(pool), saves_req(&batch)).await;

        assert_eq!(status, StatusCode::OK);
        let results: rustibia_contract::SaveResults = serde_json::from_value(body).unwrap();
        assert_eq!(
            results.results,
            vec![rustibia_contract::SaveResult {
                id: character_id,
                outcome: rustibia_contract::SaveOutcome::Applied,
            }]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_oversized_batch_is_refused(pool: PgPool) {
        let characters = (0..=rustibia_contract::MAX_SAVE_BATCH as i32).map(a_save_for).collect();

        let (status, _) = send(
            test_app(pool),
            saves_req(&rustibia_contract::SaveBatch { characters }),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    fn bare(method: &str, uri: &str) -> Request<Body> {
        Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()
    }

    async fn online_ids(pool: &PgPool) -> Vec<i32> {
        sqlx::query_scalar("SELECT character_id FROM online_players ORDER BY character_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn marking_online_twice_then_offline_leaves_nobody(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;
        let online = format!("/internal/online/{id}");

        let (first, _) = send(test_app(pool.clone()), bare("POST", &online)).await;
        let (second, _) = send(test_app(pool.clone()), bare("POST", &online)).await;
        assert_eq!((first, second), (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT));
        assert_eq!(online_ids(&pool).await, vec![id]);

        let (offline, _) = send(test_app(pool.clone()), bare("DELETE", &online)).await;
        assert_eq!(offline, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn marking_an_unknown_character_online_is_harmless(pool: PgPool) {
        let (status, _) = send(test_app(pool.clone()), bare("POST", "/internal/online/999999")).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_reset_empties_the_online_list(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;
        send(test_app(pool.clone()), bare("POST", &format!("/internal/online/{id}"))).await;

        let (status, _) = send(test_app(pool.clone()), bare("POST", "/internal/online/reset")).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }
}

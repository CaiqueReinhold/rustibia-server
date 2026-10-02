//! The API the game server calls, and nobody else.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rustibia_contract::{CharacterRecord, ChunkRow, RedeemRequest, WorldSave, WorldSaveResult};

use crate::{
    db::{login, online, world_saves},
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

pub async fn post_world_save(
    State(state): State<AppState>,
    Json(save): Json<WorldSave>,
) -> Result<Json<WorldSaveResult>, AppError> {
    Ok(Json(world_saves::apply(&state.pool, &save).await?))
}

pub async fn get_map_chunks(
    State(state): State<AppState>,
) -> Result<Json<Vec<ChunkRow>>, AppError> {
    Ok(Json(world_saves::load_chunks(&state.pool).await?))
}

/// A world save carries every online player and every changed chunk; axum's 2 MB default would
/// refuse a busy one.
const WORLD_SAVE_BODY_LIMIT: usize = 256 * 1024 * 1024;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/game-tokens/redeem", axum::routing::post(post_redeem))
        .route(
            "/world-saves",
            axum::routing::post(post_world_save)
                .layer(axum::extract::DefaultBodyLimit::max(WORLD_SAVE_BODY_LIMIT)),
        )
        .route("/map-chunks", axum::routing::get(get_map_chunks))
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

    fn bare(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
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
        assert_eq!(
            (first, second),
            (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT)
        );
        assert_eq!(online_ids(&pool).await, vec![id]);

        let (offline, _) = send(test_app(pool.clone()), bare("DELETE", &online)).await;
        assert_eq!(offline, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn marking_an_unknown_character_online_is_harmless(pool: PgPool) {
        let (status, _) = send(
            test_app(pool.clone()),
            bare("POST", "/internal/online/999999"),
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_reset_empties_the_online_list(pool: PgPool) {
        let account_id = an_account(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;
        send(
            test_app(pool.clone()),
            bare("POST", &format!("/internal/online/{id}")),
        )
        .await;

        let (status, _) = send(
            test_app(pool.clone()),
            bare("POST", "/internal/online/reset"),
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(online_ids(&pool).await.is_empty());
    }

    fn world_save_req(save: &rustibia_contract::WorldSave) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/internal/world-saves")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(save).unwrap()))
            .unwrap()
    }

    fn chunks_with_padding(count: i32) -> Vec<rustibia_contract::ChunkRow> {
        use rustibia_contract::{ChunkRow, PlacedItem, StoredItemRecord, TileRow};
        (0..count)
            .map(|cx| ChunkRow {
                cx,
                cy: 0,
                z: 7,
                tiles: (0..256)
                    .map(|index| TileRow {
                        index,
                        items: vec![PlacedItem {
                            stack_index: 1,
                            item: StoredItemRecord {
                                item_id: 3031,
                                amount: 1,
                                content: None,
                                action_id: None,
                            },
                        }],
                    })
                    .collect(),
            })
            .collect()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_world_save_larger_than_axums_default_limit_is_accepted(pool: PgPool) {
        let save = rustibia_contract::WorldSave {
            tick: 1,
            characters: Vec::new(),
            chunks: chunks_with_padding(150),
        };
        assert!(serde_json::to_vec(&save).unwrap().len() > 2 * 1024 * 1024);

        let (status, body) = send(test_app(pool.clone()), world_save_req(&save)).await;

        assert_eq!(status, StatusCode::OK);
        let result: rustibia_contract::WorldSaveResult = serde_json::from_value(body).unwrap();
        assert!(result.skipped.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_map_chunks_route_returns_what_was_saved(pool: PgPool) {
        let save = rustibia_contract::WorldSave {
            tick: 1,
            characters: Vec::new(),
            chunks: chunks_with_padding(2),
        };
        send(test_app(pool.clone()), world_save_req(&save)).await;

        let (status, body) = send(test_app(pool), bare("GET", "/internal/map-chunks")).await;

        assert_eq!(status, StatusCode::OK);
        let mut chunks: Vec<rustibia_contract::ChunkRow> = serde_json::from_value(body).unwrap();
        chunks.sort_by_key(|chunk| chunk.cx);
        assert_eq!(chunks, save.chunks);
    }
}

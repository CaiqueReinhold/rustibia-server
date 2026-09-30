use axum::{Json, extract::Path, extract::State};
use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::{
    auth::{extractor::CurrentAccount, token::generate_token},
    db::characters,
    error::AppError,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct CharacterSummary {
    pub id: i32,
    pub name: String,
    pub level: i16,
    pub vocation: String,
}

/// The character list the game client shows after login.
pub async fn get_characters(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<CharacterSummary>>, AppError> {
    let characters = characters::list_for_account(&state.pool, account.account_id).await?;

    Ok(Json(
        characters
            .into_iter()
            .map(|c| CharacterSummary {
                id: c.id,
                name: c.name,
                level: c.level,
                vocation: c.vocation.name().to_string(),
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct GameTokenResponse {
    pub auth_token: String,
    pub expires_at: String,
}

/// Issues a short-lived token the game client hands to the game server on connect.
pub async fn post_character_token(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(character_id): Path<i32>,
) -> Result<Json<GameTokenResponse>, AppError> {
    if !characters::belongs_to_account(&state.pool, character_id, account.account_id).await? {
        return Err(AppError::NotFound);
    }

    let token = format!("{character_id}.{}", generate_token());
    let valid_until =
        OffsetDateTime::now_utc() + Duration::seconds(state.config.auth_token_ttl_seconds);

    // Upsert, so the previous ticket for this character is replaced rather than joined.
    // The conflict clause is deliberately unconditional on `valid_until`: replacing an
    // expired row as readily as a live one is what keeps stale rows from accumulating
    // without a scheduled reaper.
    sqlx::query(
        "INSERT INTO game_tokens (token_hash, character_id, valid_until) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (character_id) DO UPDATE \
            SET token_hash = EXCLUDED.token_hash, valid_until = EXCLUDED.valid_until",
    )
    .bind(crate::auth::token::hash_token(&token))
    .bind(character_id)
    .bind(valid_until)
    .execute(&state.pool)
    .await?;

    Ok(Json(GameTokenResponse {
        auth_token: token,
        expires_at: valid_until
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, sessions},
        domain::{sex::Sex, vocation::Vocation},
    };
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        routing::{get, post},
    };
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;

    fn test_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .route("/api/characters", get(get_characters))
            .route("/api/characters/{id}/token", post(post_character_token))
            .with_state(AppState { pool, config })
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

    fn get_list(token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri("/api/characters");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    fn post_token_req(character_id: i32, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(format!("/api/characters/{character_id}/token"));
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn account_with_session(pool: &PgPool, email: &str) -> (i32, String) {
        let account = create_account(pool, email, "hunter2hunter2").await.unwrap();
        let session = sessions::issue(pool, account.id, 7).await.unwrap();
        (account.id, session.token)
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

    #[sqlx::test(migrations = "./migrations")]
    async fn returns_the_accounts_characters(pool: PgPool) {
        let (account_id, token) = account_with_session(&pool, "player@example.com").await;
        a_character(&pool, account_id, "Rizael").await;

        let (status, body) = send(test_app(pool), get_list(Some(&token))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["name"], "Rizael");
        assert_eq!(body[0]["vocation"], "Paladin");
        assert_eq!(body[0]["level"], 1);
        assert!(
            body[0]["sex"].is_null(),
            "sex must not be exposed by this endpoint"
        );
        assert!(
            body[0]["online"].is_null(),
            "online must not be exposed by this endpoint"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_missing_token_is_401(pool: PgPool) {
        let (status, _) = send(test_app(pool), get_list(None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_token_is_401(pool: PgPool) {
        let (status, _) = send(test_app(pool), get_list(Some("not-a-real-token"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn one_account_never_sees_anothers_characters(pool: PgPool) {
        let (owner_id, _) = account_with_session(&pool, "owner@example.com").await;
        let (_, stranger_token) = account_with_session(&pool, "stranger@example.com").await;
        a_character(&pool, owner_id, "Rizael").await;

        let (status, body) = send(test_app(pool), get_list(Some(&stranger_token))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.as_array().unwrap().len(),
            0,
            "must not leak another account's characters"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleted_characters_are_excluded(pool: PgPool) {
        let (account_id, token) = account_with_session(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;
        characters::soft_delete(&pool, id, account_id)
            .await
            .unwrap();

        let (_, body) = send(test_app(pool), get_list(Some(&token))).await;

        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn issues_a_token_the_game_server_would_accept(pool: PgPool) {
        let (account_id, token) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;

        let (status, body) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&token)),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let auth_token = body["auth_token"].as_str().unwrap();
        let (id, random) = auth_token.split_once('.').unwrap();
        assert_eq!(id, character_id.to_string());
        assert_eq!(random.len(), 64);

        // The lookup `db::login::redeem` performs: by digest, and returning the
        // character rather than the account.
        let resolved: Option<(i32,)> = sqlx::query_as(
            "SELECT character_id FROM game_tokens WHERE token_hash = $1 AND valid_until > NOW()",
        )
        .bind(crate::auth::token::hash_token(auth_token))
        .fetch_optional(&pool)
        .await
        .unwrap();

        assert_eq!(
            resolved.map(|r| r.0),
            Some(character_id),
            "the issued token must name the character it was minted for"
        );
    }

    /// The reason the column changed. A token that reaches a client must leave no copy
    /// behind that would let a reader of the table log in as that character.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_issued_token_is_not_recoverable_from_the_table(pool: PgPool) {
        let (account_id, session) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;

        let (_, body) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&session)),
        )
        .await;
        let auth_token = body["auth_token"].as_str().unwrap();

        let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM game_tokens")
            .fetch_all(&pool)
            .await
            .unwrap();

        assert_eq!(stored.len(), 1);
        assert_ne!(
            stored[0], auth_token,
            "the plaintext token must not be what is stored"
        );
        assert_eq!(stored[0], crate::auth::token::hash_token(auth_token));
    }

    /// Last click wins. Two "play" clicks must not leave two working tickets behind.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_second_mint_supersedes_the_first(pool: PgPool) {
        let (account_id, session) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;

        let (_, first) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&session)),
        )
        .await;
        let (status, second) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&session)),
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM game_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "a character must hold at most one ticket");

        let live: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM game_tokens WHERE token_hash = $1")
                .bind(crate::auth::token::hash_token(
                    first["auth_token"].as_str().unwrap(),
                ))
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(live, None, "the superseded token must be gone");

        let current: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM game_tokens WHERE token_hash = $1")
                .bind(crate::auth::token::hash_token(
                    second["auth_token"].as_str().unwrap(),
                ))
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(current.is_some(), "the newest token must be the live one");
    }

    /// Supersede is scoped to the character, not the account. Two characters must be
    /// able to hold live tickets at once, or an account cannot run two clients.
    #[sqlx::test(migrations = "./migrations")]
    async fn minting_for_one_character_leaves_anothers_token_alone(pool: PgPool) {
        let (account_id, session) = account_with_session(&pool, "player@example.com").await;
        let first_id = a_character(&pool, account_id, "Rizael").await;
        let second_id = a_character(&pool, account_id, "Anaia").await;

        let (_, first) = send(
            test_app(pool.clone()),
            post_token_req(first_id, Some(&session)),
        )
        .await;
        send(
            test_app(pool.clone()),
            post_token_req(second_id, Some(&session)),
        )
        .await;

        let survives: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM game_tokens WHERE token_hash = $1")
                .bind(crate::auth::token::hash_token(
                    first["auth_token"].as_str().unwrap(),
                ))
                .fetch_optional(&pool)
                .await
                .unwrap();

        assert!(
            survives.is_some(),
            "minting for one character must not revoke another's ticket"
        );
    }

    /// The stale-row cleanup that makes a scheduled reaper unnecessary: an expired row
    /// is replaced rather than colliding with the new one.
    #[sqlx::test(migrations = "./migrations")]
    async fn minting_over_an_expired_token_replaces_it(pool: PgPool) {
        let (account_id, session) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;

        sqlx::query(
            "INSERT INTO game_tokens (token_hash, character_id, valid_until) \
             VALUES ('stale-hash', $1, NOW() - INTERVAL '1 hour')",
        )
        .bind(character_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&session)),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::OK,
            "an expired row must not block a mint"
        );

        let rows: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM game_tokens")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(
            rows[0], "stale-hash",
            "the stale row must have been replaced"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn another_accounts_character_is_404_and_mints_nothing(pool: PgPool) {
        let (owner_id, _) = account_with_session(&pool, "owner@example.com").await;
        let (_, stranger_token) = account_with_session(&pool, "stranger@example.com").await;
        let character_id = a_character(&pool, owner_id, "Rizael").await;

        let (status, _) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&stranger_token)),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM game_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "no token may be minted for someone else's character. This is now the only \
             place ownership is decided anywhere in the system: redemption reads the \
             character off the token and never re-checks who owns it."
        );
    }

    /// The upsert is a blind overwrite of whatever row holds that `character_id`, so the
    /// only thing stopping a stranger from clobbering a live ticket is that the ownership
    /// check runs *before* the write. That ordering is currently just the order of two
    /// statements in one function; this pins the consequence, so a refactor that moved
    /// the check after the query would fail here rather than silently hand strangers a
    /// way to knock players offline.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_mint_leaves_the_owners_token_untouched(pool: PgPool) {
        let (owner_id, owner_session) = account_with_session(&pool, "owner@example.com").await;
        let (_, stranger_session) = account_with_session(&pool, "stranger@example.com").await;
        let character_id = a_character(&pool, owner_id, "Rizael").await;

        let (_, minted) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&owner_session)),
        )
        .await;
        let owners_hash = crate::auth::token::hash_token(minted["auth_token"].as_str().unwrap());

        let (status, _) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&stranger_session)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let live: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM game_tokens")
            .fetch_all(&pool)
            .await
            .unwrap();

        assert_eq!(
            live,
            vec![owners_hash],
            "the stranger's refused mint must leave the owner's ticket the live one"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_nonexistent_character_is_404_not_500(pool: PgPool) {
        let (_, token) = account_with_session(&pool, "player@example.com").await;

        let (status, _) = send(test_app(pool), post_token_req(999_999, Some(&token))).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_deleted_character_cannot_get_a_token(pool: PgPool) {
        let (account_id, token) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        characters::soft_delete(&pool, character_id, account_id)
            .await
            .unwrap();

        let (status, _) = send(test_app(pool), post_token_req(character_id, Some(&token))).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn no_session_is_401(pool: PgPool) {
        let (status, _) = send(test_app(pool), post_token_req(1, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_token_whose_character_id_was_edited_does_not_redeem(pool: PgPool) {
        let (account_id, session) = account_with_session(&pool, "player@example.com").await;
        let character_id = a_character(&pool, account_id, "Rizael").await;
        let (_, body) = send(
            test_app(pool.clone()),
            post_token_req(character_id, Some(&session)),
        )
        .await;
        let (_, random) = body["auth_token"]
            .as_str()
            .unwrap()
            .split_once('.')
            .unwrap();

        let edited = format!("{}.{random}", character_id + 1);

        assert!(
            crate::db::login::redeem(&pool, &edited)
                .await
                .unwrap()
                .is_none()
        );
    }
}

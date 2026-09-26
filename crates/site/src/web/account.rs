use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::{
    auth::{extractor::CurrentAccount, password::verify_password, viewer::Viewer},
    db::{accounts, characters, characters::Character},
    domain::{character_name, sex::Sex, vocation::Vocation},
    error::{AppError, Surface, SurfacedError},
    state::AppState,
    template::{HtmlTemplate, Nav},
};

#[derive(Template)]
#[template(path = "account.html")]
pub struct AccountPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub email: String,
    pub characters: Vec<Character>,
    pub error: Option<String>,
}

pub async fn get_account(
    State(state): State<AppState>,
    viewer: Viewer,
    account: CurrentAccount,
) -> Result<impl IntoResponse, SurfacedError> {
    render_account(&state, viewer, account.account_id, None)
        .await
        .map_err(|e| SurfacedError(Surface::Page, e))
}

/// Shared by the dashboard and by the delete handler, which re-renders this page
/// with an error rather than redirecting to a bare failure.
pub(super) async fn render_account(
    state: &AppState,
    viewer: Viewer,
    account_id: i32,
    error: Option<String>,
) -> Result<HtmlTemplate<AccountPage>, AppError> {
    let email: String = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&state.pool)
        .await?;

    let characters = characters::list_for_account(&state.pool, account_id).await?;

    Ok(HtmlTemplate(AccountPage {
        viewer,
        active: Nav::None,
        email,
        characters,
        error,
    }))
}

#[derive(Template)]
#[template(path = "character_new.html")]
pub struct CharacterNewPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub error: Option<String>,
    pub name: String,
    pub sexes: Vec<Sex>,
    pub vocations: Vec<Vocation>,
}

impl CharacterNewPage {
    fn new(viewer: Viewer, error: Option<String>, name: String) -> Self {
        Self {
            viewer,
            active: Nav::None,
            error,
            name,
            sexes: Sex::ALL.to_vec(),
            vocations: Vocation::ALL.to_vec(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateCharacterForm {
    pub name: String,
    pub sex: i16,
    pub vocation: i16,
}

pub async fn get_character_new(viewer: Viewer, _account: CurrentAccount) -> impl IntoResponse {
    HtmlTemplate(CharacterNewPage::new(viewer, None, String::new()))
}

pub async fn post_character_new(
    State(state): State<AppState>,
    viewer: Viewer,
    account: CurrentAccount,
    Form(form): Form<CreateCharacterForm>,
) -> Response {
    let rerender = |message: String, name: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            HtmlTemplate(CharacterNewPage::new(
                viewer,
                Some(message),
                name.to_string(),
            )),
        )
            .into_response()
    };

    let name = match character_name::validate(&form.name) {
        Ok(name) => name,
        Err(AppError::Validation(message)) => return rerender(message, &form.name),
        Err(err) => return rerender(err.public_message(), &form.name),
    };

    let sex = match Sex::from_i16(form.sex) {
        Ok(sex) => sex,
        Err(err) => return rerender(err.public_message(), &form.name),
    };
    let vocation = match Vocation::from_i16(form.vocation) {
        Ok(vocation) => vocation,
        Err(err) => return rerender(err.public_message(), &form.name),
    };

    match characters::create(
        &state.pool,
        account.account_id,
        &name,
        vocation,
        sex,
        &state.config.new_character,
    )
    .await
    {
        Ok(_) => Redirect::to("/account").into_response(),
        Err(AppError::Validation(message)) => rerender(message, &form.name),
        Err(err) => {
            tracing::error!("character creation failed: {err}");
            rerender(err.public_message(), &form.name)
        }
    }
}

/// Soft-deletes a character.
///
/// Refused while the character is online: a soft delete leaves the row in place, so
/// the player would keep playing a character the database considers deleted, and the
/// game server's next periodic save would write gameplay state to it. Refusing here
/// is cleaner than trying to evict a live session from the persistence path.
pub async fn post_character_delete(
    State(state): State<AppState>,
    viewer: Viewer,
    account: CurrentAccount,
    Path(character_id): Path<i32>,
) -> Response {
    let fail = |message: &'static str| {
        let state = state.clone();
        async move {
            match render_account(
                &state,
                viewer,
                account.account_id,
                Some(message.to_string()),
            )
            .await
            {
                Ok(page) => (StatusCode::UNPROCESSABLE_ENTITY, page).into_response(),
                Err(err) => SurfacedError(Surface::Page, err).into_response(),
            }
        }
    };

    match characters::belongs_to_account(&state.pool, character_id, account.account_id).await {
        Ok(true) => {}
        // Absent, already deleted, or someone else's — all indistinguishable, so the
        // endpoint cannot be used to probe which character ids exist or who is online.
        Ok(false) => return fail("That character does not exist.").await,
        Err(err) => {
            tracing::error!("ownership check failed: {err}");
            return fail("Something went wrong. Please try again.").await;
        }
    }

    match characters::is_online(&state.pool, character_id).await {
        Ok(true) => return fail("That character is currently online. Log out first.").await,
        Ok(false) => {}
        Err(err) => {
            tracing::error!("online check failed: {err}");
            return fail("Something went wrong. Please try again.").await;
        }
    }

    match characters::soft_delete(&state.pool, character_id, account.account_id).await {
        Ok(()) => Redirect::to("/account").into_response(),
        Err(AppError::NotFound) => fail("That character does not exist.").await,
        Err(err) => {
            tracing::error!("character deletion failed: {err}");
            fail("Something went wrong. Please try again.").await
        }
    }
}

#[derive(Template)]
#[template(path = "password.html")]
pub struct PasswordPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub error: Option<String>,
    pub changed: bool,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub new_password_confirm: String,
}

pub async fn get_password(viewer: Viewer, _account: CurrentAccount) -> impl IntoResponse {
    HtmlTemplate(PasswordPage {
        viewer,
        active: Nav::None,
        error: None,
        changed: false,
    })
}

pub async fn post_password(
    State(state): State<AppState>,
    viewer: Viewer,
    account: CurrentAccount,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    let rerender = |message: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            HtmlTemplate(PasswordPage {
                viewer,
                active: Nav::None,
                error: Some(message.to_string()),
                changed: false,
            }),
        )
            .into_response()
    };

    if form.new_password != form.new_password_confirm {
        return rerender("The two new passwords do not match.");
    }
    if form.new_password.len() < state.config.min_password_length {
        return rerender(&format!(
            "Password must be at least {} characters.",
            state.config.min_password_length
        ));
    }

    let stored = match accounts::find_by_id(&state.pool, account.account_id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return rerender("Something went wrong. Please try again."),
        Err(err) => {
            tracing::error!("password change lookup failed: {err}");
            return rerender("Something went wrong. Please try again.");
        }
    };

    if !verify_password(&form.current_password, &stored.password_hash) {
        return rerender("Your current password is incorrect.");
    }

    let keep = account.session_token.clone();

    match accounts::update_password(&state.pool, account.account_id, &form.new_password, &keep)
        .await
    {
        Ok(()) => HtmlTemplate(PasswordPage {
            viewer,
            active: Nav::None,
            error: None,
            changed: true,
        })
        .into_response(),
        Err(err) => {
            tracing::error!("password change failed: {err}");
            rerender("Something went wrong. Please try again.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, sessions},
    };
    use axum::{
        Router,
        body::Body,
        http::{Request, header},
        routing::get,
    };
    use sqlx::PgPool;
    use tower::ServiceExt;

    fn test_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .route("/account", get(get_account))
            .route(
                "/account/characters/new",
                get(get_character_new).post(post_character_new),
            )
            .with_state(AppState { pool, config })
    }

    async fn logged_in_session(pool: &PgPool, email: &str) -> (i32, String) {
        let account = create_account(pool, email, "hunter2hunter2").await.unwrap();
        let session = sessions::issue(pool, account.id, 7).await.unwrap();
        (account.id, session.token)
    }

    async fn post_create(app: Router, token: &str, body: &str) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/account/characters/new")
                .header(header::COOKIE, format!("session={token}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn creating_a_character_redirects_to_the_dashboard(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_create(
            test_app(pool.clone()),
            &token,
            "name=Rizael&sex=1&vocation=0",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/account"
        );

        let listed = characters::list_for_account(&pool, account_id)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Rizael");
        assert_eq!(listed[0].vocation, Vocation::Knight);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_invalid_name_is_rejected_without_creating_anything(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_create(
            test_app(pool.clone()),
            &token,
            "name=Riz4el&sex=1&vocation=0",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            characters::list_for_account(&pool, account_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_out_of_range_vocation_is_rejected(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_create(
            test_app(pool.clone()),
            &token,
            "name=Rizael&sex=1&vocation=9",
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a hand-crafted POST must not write an unknown vocation the game server will trust"
        );
        assert!(
            characters::list_for_account(&pool, account_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_duplicate_name_is_rejected(pool: PgPool) {
        let (_, token) = logged_in_session(&pool, "player@example.com").await;
        post_create(
            test_app(pool.clone()),
            &token,
            "name=Rizael&sex=1&vocation=0",
        )
        .await;

        let response = post_create(
            test_app(pool.clone()),
            &token,
            "name=RIZAEL&sex=0&vocation=3",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_dashboard_requires_a_session(pool: PgPool) {
        let response = test_app(pool)
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_dashboard_lists_the_accounts_characters(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        characters::create(
            &pool,
            account_id,
            "Rizael",
            Vocation::Druid,
            Sex::Female,
            &template,
        )
        .await
        .unwrap();

        let response = test_app(pool)
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .header(header::COOKIE, format!("session={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains("Rizael"),
            "the dashboard must list the character"
        );
        assert!(html.contains("Druid"), "and show its vocation");
        assert!(html.contains("player@example.com"), "and the account email");
        assert!(
            html.contains(r#"href="/account/password""#),
            "the dashboard must link to the password page"
        );
    }

    async fn post_delete(app: Router, token: &str, character_id: i32) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/account/characters/{character_id}/delete"))
                .header(header::COOKIE, format!("session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    fn full_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .route("/account", get(get_account))
            .route(
                "/account/characters/{id}/delete",
                axum::routing::post(post_character_delete),
            )
            .route("/account/password", get(get_password).post(post_password))
            .with_state(AppState { pool, config })
    }

    async fn a_character(pool: &PgPool, account_id: i32, name: &str) -> i32 {
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        characters::create(
            pool,
            account_id,
            name,
            Vocation::Knight,
            Sex::Male,
            &template,
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_a_character_removes_it_from_the_list(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;

        let response = post_delete(full_app(pool.clone()), &token, id).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            characters::list_for_account(&pool, account_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_an_online_character_is_refused(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;
        let id = a_character(&pool, account_id, "Rizael").await;

        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let response = post_delete(full_app(pool.clone()), &token, id).await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            characters::list_for_account(&pool, account_id)
                .await
                .unwrap()
                .len(),
            1,
            "an online character must survive the deletion attempt"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn one_account_cannot_delete_anothers_character(pool: PgPool) {
        let (owner_id, _) = logged_in_session(&pool, "owner@example.com").await;
        let (_, stranger_token) = logged_in_session(&pool, "stranger@example.com").await;
        let id = a_character(&pool, owner_id, "Rizael").await;

        let response = post_delete(full_app(pool.clone()), &stranger_token, id).await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            characters::list_for_account(&pool, owner_id)
                .await
                .unwrap()
                .len(),
            1,
            "the owner's character must be untouched"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_an_online_character_you_do_not_own_reveals_nothing(pool: PgPool) {
        let (owner_id, _) = logged_in_session(&pool, "owner@example.com").await;
        let (_, stranger_token) = logged_in_session(&pool, "stranger@example.com").await;
        let online_id = a_character(&pool, owner_id, "Rizael").await;
        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(online_id)
            .execute(&pool)
            .await
            .unwrap();

        let online_response = post_delete(full_app(pool.clone()), &stranger_token, online_id).await;
        let absent_response = post_delete(full_app(pool.clone()), &stranger_token, 999_999).await;

        assert_eq!(online_response.status(), absent_response.status());

        let online_body = axum::body::to_bytes(online_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let absent_body = axum::body::to_bytes(absent_response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            online_body, absent_body,
            "a character you do not own must look identical whether it is online or \
             does not exist — otherwise the endpoint is an online-status oracle"
        );
    }

    async fn post_password_form(app: Router, token: &str, body: &str) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/account/password")
                .header(header::COOKIE, format!("session={token}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_wrong_current_password_changes_nothing(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_password_form(
            full_app(pool.clone()),
            &token,
            "current_password=wrong&new_password=a-brand-new-password&new_password_confirm=a-brand-new-password",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let stored = crate::db::accounts::find_by_id(&pool, account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::auth::password::verify_password("hunter2hunter2", &stored.password_hash),
            "the original password must still work"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_correct_current_password_changes_it(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_password_form(
            full_app(pool.clone()),
            &token,
            "current_password=hunter2hunter2&new_password=a-brand-new-password&new_password_confirm=a-brand-new-password",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let stored = crate::db::accounts::find_by_id(&pool, account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(crate::auth::password::verify_password(
            "a-brand-new-password",
            &stored.password_hash
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_short_new_password_is_rejected(pool: PgPool) {
        let (_, token) = logged_in_session(&pool, "player@example.com").await;

        let response = post_password_form(
            full_app(pool),
            &token,
            "current_password=hunter2hunter2&new_password=short&new_password_confirm=short",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn changing_the_password_through_the_form_keeps_the_callers_session(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;
        let other = sessions::issue(&pool, account_id, 7).await.unwrap();

        post_password_form(
            full_app(pool.clone()),
            &token,
            "current_password=hunter2hunter2&new_password=a-brand-new-password&new_password_confirm=a-brand-new-password",
        )
        .await;

        assert_eq!(
            sessions::account_for_token(&pool, &token).await.unwrap(),
            Some(account_id),
            "the caller must not be logged out of the tab they used"
        );
        assert_eq!(
            sessions::account_for_token(&pool, &other.token)
                .await
                .unwrap(),
            None,
            "every other session must be revoked"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_bearer_caller_keeps_their_own_session_on_password_change(pool: PgPool) {
        let (account_id, token) = logged_in_session(&pool, "player@example.com").await;
        let other = sessions::issue(&pool, account_id, 7).await.unwrap();

        let response = full_app(pool.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/account/password")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "current_password=hunter2hunter2&new_password=a-brand-new-password\
                         &new_password_confirm=a-brand-new-password",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            sessions::account_for_token(&pool, &token).await.unwrap(),
            Some(account_id),
            "a bearer-authenticated caller must keep the session they authenticated with"
        );
        assert_eq!(
            sessions::account_for_token(&pool, &other.token)
                .await
                .unwrap(),
            None,
            "every other session must still be revoked"
        );
    }
}

use askama::Template;
use axum::{
    Form,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::{
    auth::{password::verify_password, viewer::Viewer},
    db::{accounts, sessions},
    state::AppState,
    template::{HtmlTemplate, Nav},
};

#[derive(Template)]
#[template(path = "register.html")]
pub struct RegisterPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub error: Option<String>,
    pub email: String,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub error: Option<String>,
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    pub email: String,
    pub password: String,
    pub password_confirm: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
}

pub async fn get_register(viewer: Viewer) -> impl IntoResponse {
    HtmlTemplate(RegisterPage {
        viewer,
        active: Nav::None,
        error: None,
        email: String::new(),
    })
}

pub async fn get_login(viewer: Viewer) -> impl IntoResponse {
    HtmlTemplate(LoginPage {
        viewer,
        active: Nav::None,
        error: None,
        email: String::new(),
    })
}

/// Builds the `Set-Cookie` header for a session token. `SameSite=Lax` allows the
/// cookie to survive a normal top-level navigation while blocking cross-site POSTs.
fn session_cookie(token: &str, max_age_days: i64) -> String {
    format!(
        "session={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        max_age_days * 24 * 60 * 60
    )
}

fn cleared_cookie() -> String {
    "session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string()
}

pub async fn post_register(
    State(state): State<AppState>,
    Form(form): Form<RegisterForm>,
) -> Response {
    let rerender = |error: &str, email: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            HtmlTemplate(RegisterPage {
                // A failed registration is by definition not authenticated, so the
                // anonymous nav is correct here regardless of what cookie was sent.
                viewer: Viewer::ANONYMOUS,
                active: Nav::None,
                error: Some(error.to_string()),
                email: email.to_string(),
            }),
        )
            .into_response()
    };

    if form.password != form.password_confirm {
        return rerender("The two passwords do not match.", &form.email);
    }
    if form.password.len() < state.config.min_password_length {
        return rerender(
            &format!(
                "Password must be at least {} characters.",
                state.config.min_password_length
            ),
            &form.email,
        );
    }
    if !form.email.contains('@') || form.email.starts_with('@') || form.email.ends_with('@') {
        return rerender("Please enter a valid email address.", &form.email);
    }

    let account = match accounts::create_account(&state.pool, &form.email, &form.password).await {
        Ok(account) => account,
        Err(crate::error::AppError::Validation(message)) => return rerender(&message, &form.email),
        Err(err) => {
            tracing::error!("registration failed: {err}");
            return rerender("Something went wrong. Please try again.", &form.email);
        }
    };

    match sessions::issue(&state.pool, account.id, state.config.session_ttl_days).await {
        Ok(session) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::SET_COOKIE,
                session_cookie(&session.token, state.config.session_ttl_days)
                    .parse()
                    .expect("cookie is valid header value"),
            );
            (headers, Redirect::to("/account")).into_response()
        }
        Err(err) => {
            tracing::error!("session issue failed after registration: {err}");
            rerender("Something went wrong. Please try again.", &form.email)
        }
    }
}

pub async fn post_login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let rerender = || {
        (
            StatusCode::UNAUTHORIZED,
            HtmlTemplate(LoginPage {
                viewer: Viewer::ANONYMOUS,
                active: Nav::None,
                error: Some("Invalid email or password.".to_string()),
                email: form.email.clone(),
            }),
        )
            .into_response()
    };

    let Ok(Some(account)) = accounts::find_by_email(&state.pool, &form.email).await else {
        crate::auth::password::spend_dummy_verification(&form.password);
        return rerender();
    };

    if !verify_password(&form.password, &account.password_hash) {
        return rerender();
    }

    match sessions::issue(&state.pool, account.id, state.config.session_ttl_days).await {
        Ok(session) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::SET_COOKIE,
                session_cookie(&session.token, state.config.session_ttl_days)
                    .parse()
                    .expect("cookie is valid header value"),
            );
            (headers, Redirect::to("/account")).into_response()
        }
        Err(err) => {
            tracing::error!("session issue failed after login: {err}");
            rerender()
        }
    }
}

pub async fn get_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(cookies) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for pair in cookies.split(';') {
            if let Some((name, value)) = pair.split_once('=')
                && name.trim() == "session"
                && let Err(err) = sessions::revoke(&state.pool, value.trim()).await
            {
                tracing::error!("failed to revoke session on logout: {err}");
            }
        }
    }

    let mut out = HeaderMap::new();
    out.insert(
        header::SET_COOKIE,
        cleared_cookie()
            .parse()
            .expect("cookie is valid header value"),
    );
    (out, Redirect::to("/login")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SiteConfig;
    use axum::{Router, body::Body, http::Request, routing::get};
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;

    fn test_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .route("/register", get(get_register).post(post_register))
            .route("/login", get(get_login).post(post_login))
            .route("/logout", get(get_logout))
            .with_state(AppState { pool, config })
    }

    async fn post_form(app: Router, uri: &str, body: &str) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn registering_sets_a_session_cookie_and_redirects(pool: PgPool) {
        let response = post_form(
            test_app(pool.clone()),
            "/register",
            "email=player%40example.com&password=hunter2hunter2&password_confirm=hunter2hunter2",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/account"
        );

        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.starts_with("session="));
        assert!(
            cookie.contains("HttpOnly"),
            "session cookie must be HttpOnly"
        );
        assert!(cookie.contains("SameSite=Lax"));

        assert!(
            accounts::find_by_email(&pool, "player@example.com")
                .await
                .unwrap()
                .is_some(),
            "the account must exist afterwards"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mismatched_passwords_are_rejected_without_creating_an_account(pool: PgPool) {
        let response = post_form(
            test_app(pool.clone()),
            "/register",
            "email=player%40example.com&password=hunter2hunter2&password_confirm=different",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            accounts::find_by_email(&pool, "player@example.com")
                .await
                .unwrap()
                .is_none(),
            "no account may be created when validation fails"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_short_password_is_rejected(pool: PgPool) {
        let response = post_form(
            test_app(pool.clone()),
            "/register",
            "email=player%40example.com&password=short&password_confirm=short",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            accounts::find_by_email(&pool, "player@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_malformed_email_is_rejected(pool: PgPool) {
        let response = post_form(
            test_app(pool.clone()),
            "/register",
            "email=notanemail&password=hunter2hunter2&password_confirm=hunter2hunter2",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            accounts::find_by_email(&pool, "notanemail")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn logging_in_with_valid_credentials_sets_a_cookie(pool: PgPool) {
        accounts::create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();

        let response = post_form(
            test_app(pool),
            "/login",
            "email=player%40example.com&password=hunter2hunter2",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(response.headers().get(header::SET_COOKIE).is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn logging_in_with_a_bad_password_sets_no_cookie(pool: PgPool) {
        accounts::create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();

        let response = post_form(
            test_app(pool),
            "/login",
            "email=player%40example.com&password=wrong",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "a failed login must not set a session cookie"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unknown_email_login_is_indistinguishable_from_a_wrong_password(pool: PgPool) {
        accounts::create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();

        let wrong_password = post_form(
            test_app(pool.clone()),
            "/login",
            "email=player%40example.com&password=wrong",
        )
        .await;
        let unknown_email = post_form(
            test_app(pool.clone()),
            "/login",
            "email=nobody%40example.com&password=wrong",
        )
        .await;

        assert_eq!(wrong_password.status(), unknown_email.status());
        assert!(
            unknown_email.headers().get(header::SET_COOKIE).is_none(),
            "an unknown email must not set a session cookie"
        );

        let wrong_body = wrong_password
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let unknown_body = unknown_email
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let wrong_text = String::from_utf8_lossy(&wrong_body);
        let unknown_text = String::from_utf8_lossy(&unknown_body);

        // The pages legitimately differ in one respect: each re-renders the address
        // that was actually submitted, so the user does not have to retype it. That
        // discloses nothing an attacker did not already know. Mask each page's own
        // submitted address and everything else must match exactly — otherwise the
        // response text distinguishes a registered email from an unregistered one.
        let masked_wrong = wrong_text.replace("player@example.com", "{submitted}");
        let masked_unknown = unknown_text.replace("nobody@example.com", "{submitted}");

        assert_eq!(
            masked_wrong, masked_unknown,
            "apart from echoing back the submitted address, the two failures must \
             render identically — any other difference reveals which emails are registered"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn logging_out_revokes_the_session_server_side(pool: PgPool) {
        let account = accounts::create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();
        let session = sessions::issue(&pool, account.id, 7).await.unwrap();

        let response = test_app(pool.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/logout")
                    .header(header::COOKIE, format!("session={}", session.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            sessions::account_for_token(&pool, &session.token)
                .await
                .unwrap(),
            None,
            "clearing the cookie is not enough — the session row must be gone, \
             or the token keeps working after logout"
        );
    }
}

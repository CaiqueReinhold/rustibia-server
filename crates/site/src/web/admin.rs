use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    auth::{admin::AdminAccount, viewer::Viewer},
    db::news,
    error::AppError,
    state::AppState,
    template::{HtmlTemplate, Nav},
};

#[derive(Template)]
#[template(path = "admin_news.html")]
pub struct AdminNewsPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub error: Option<String>,
    pub posted: bool,
    pub title: String,
    pub body: String,
}

impl AdminNewsPage {
    /// Only `AdminAccount` reaches this page, so the nav is known without asking.
    fn blank() -> Self {
        Self {
            viewer: Viewer {
                logged_in: true,
                is_admin: true,
            },
            active: Nav::None,
            error: None,
            posted: false,
            title: String::new(),
            body: String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct NewsForm {
    pub title: String,
    pub body: String,
}

pub async fn get_admin_news(_admin: AdminAccount) -> impl IntoResponse {
    HtmlTemplate(AdminNewsPage::blank())
}

pub async fn post_admin_news(
    State(state): State<AppState>,
    admin: AdminAccount,
    Form(form): Form<NewsForm>,
) -> Response {
    match news::create(&state.pool, &form.title, &form.body, admin.account_id).await {
        Ok(_) => HtmlTemplate(AdminNewsPage {
            posted: true,
            ..AdminNewsPage::blank()
        })
        .into_response(),
        Err(AppError::Validation(message)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            HtmlTemplate(AdminNewsPage {
                error: Some(message),
                title: form.title,
                body: form.body,
                ..AdminNewsPage::blank()
            }),
        )
            .into_response(),
        Err(err) => {
            tracing::error!("news post failed: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                HtmlTemplate(AdminNewsPage {
                    error: Some("Something went wrong. Please try again.".to_string()),
                    title: form.title,
                    body: form.body,
                    ..AdminNewsPage::blank()
                }),
            )
                .into_response()
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
            .route("/admin/news", get(get_admin_news).post(post_admin_news))
            .with_state(AppState { pool, config })
    }

    async fn session_for(pool: &PgPool, email: &str, admin: bool) -> String {
        let account = create_account(pool, email, "hunter2hunter2").await.unwrap();
        if admin {
            sqlx::query("UPDATE accounts SET is_admin = TRUE WHERE id = $1")
                .bind(account.id)
                .execute(pool)
                .await
                .unwrap();
        }
        sessions::issue(pool, account.id, 7).await.unwrap().token
    }

    async fn get_page(app: Router, token: Option<&str>) -> StatusCode {
        let mut b = Request::builder().uri("/admin/news");
        if let Some(t) = token {
            b = b.header(header::COOKIE, format!("session={t}"));
        }
        app.oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    async fn post_news(app: Router, token: &str, body: &str) -> StatusCode {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/news")
                .header(header::COOKIE, format!("session={token}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_admin_can_open_the_page(pool: PgPool) {
        let token = session_for(&pool, "admin@example.com", true).await;
        assert_eq!(get_page(test_app(pool), Some(&token)).await, StatusCode::OK);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_admin_gets_404_not_403(pool: PgPool) {
        let token = session_for(&pool, "player@example.com", false).await;

        assert_eq!(
            get_page(test_app(pool), Some(&token)).await,
            StatusCode::NOT_FOUND,
            "a non-admin must not learn that this page exists"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_anonymous_visitor_is_401(pool: PgPool) {
        assert_eq!(
            get_page(test_app(pool), None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_admin_can_publish(pool: PgPool) {
        let token = session_for(&pool, "admin@example.com", true).await;

        let status = post_news(
            test_app(pool.clone()),
            &token,
            "title=Server+is+open&body=Come+and+play.",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let posts = news::list_recent(&pool, 10).await.unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].title, "Server is open");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_admin_cannot_publish(pool: PgPool) {
        let token = session_for(&pool, "player@example.com", false).await;

        let status = post_news(test_app(pool.clone()), &token, "title=Hacked&body=Oops").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            news::list_recent(&pool, 10).await.unwrap().is_empty(),
            "a non-admin POST must not create a post"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_empty_title_is_rejected(pool: PgPool) {
        let token = session_for(&pool, "admin@example.com", true).await;

        let status = post_news(test_app(pool.clone()), &token, "title=+++&body=Something").await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(news::list_recent(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn revoking_admin_takes_effect_immediately(pool: PgPool) {
        let account = create_account(&pool, "admin@example.com", "hunter2hunter2")
            .await
            .unwrap();
        sqlx::query("UPDATE accounts SET is_admin = TRUE WHERE id = $1")
            .bind(account.id)
            .execute(&pool)
            .await
            .unwrap();
        let token = sessions::issue(&pool, account.id, 7).await.unwrap().token;

        assert_eq!(
            get_page(test_app(pool.clone()), Some(&token)).await,
            StatusCode::OK
        );

        sqlx::query("UPDATE accounts SET is_admin = FALSE WHERE id = $1")
            .bind(account.id)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            get_page(test_app(pool), Some(&token)).await,
            StatusCode::NOT_FOUND,
            "admin is read per request, so revocation must not wait for the next login"
        );
    }
}

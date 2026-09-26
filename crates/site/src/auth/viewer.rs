use std::convert::Infallible;

use axum::{extract::FromRequestParts, http::request::Parts};

use crate::{auth::extractor::CurrentAccount, db::accounts, state::AppState};

/// Everything the header needs to know about the caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewer {
    pub logged_in: bool,
    pub is_admin: bool,
}

impl Viewer {
    /// The logged-out nav, for the two places that render a page with no request in
    /// hand: the error page and tests.
    pub const ANONYMOUS: Viewer = Viewer {
        logged_in: false,
        is_admin: false,
    };
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        let Ok(current) = CurrentAccount::from_request_parts(parts, state).await else {
            return Ok(Viewer::ANONYMOUS);
        };

        let is_admin = match accounts::find_by_id(&state.pool, current.account_id).await {
            Ok(Some(account)) => account.is_admin,
            Ok(None) => false,
            Err(err) => {
                tracing::error!("could not read admin status for the navigation: {err}");
                false
            }
        };

        Ok(Viewer {
            logged_in: true,
            is_admin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SiteConfig,
        db::{accounts::create_account, sessions},
    };
    use axum::http::{Request, header};
    use sqlx::PgPool;

    async fn viewer_for(pool: PgPool, cookie: Option<&str>) -> Viewer {
        let config = SiteConfig::load("config.yaml").unwrap();
        let mut builder = Request::builder().uri("/");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let mut parts = builder.body(()).unwrap().into_parts().0;

        Viewer::from_request_parts(&mut parts, &AppState { pool, config })
            .await
            .unwrap()
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

    #[sqlx::test(migrations = "./migrations")]
    async fn no_cookie_is_anonymous(pool: PgPool) {
        let viewer = viewer_for(pool, None).await;
        assert!(!viewer.logged_in);
        assert!(!viewer.is_admin);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_live_session_is_logged_in(pool: PgPool) {
        let token = session_for(&pool, "player@example.com", false).await;

        let viewer = viewer_for(pool, Some(&format!("session={token}"))).await;

        assert!(viewer.logged_in);
        assert!(!viewer.is_admin, "a player is not an admin");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_admin_session_reports_admin(pool: PgPool) {
        let token = session_for(&pool, "admin@example.com", true).await;

        let viewer = viewer_for(pool, Some(&format!("session={token}"))).await;

        assert!(viewer.logged_in);
        assert!(viewer.is_admin);
    }

    /// The nav must degrade, not fail: a page anyone may read stays readable when the
    /// cookie is stale.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_token_is_anonymous_rather_than_an_error(pool: PgPool) {
        let viewer = viewer_for(pool, Some("session=not-a-real-token")).await;
        assert!(!viewer.logged_in);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_expired_session_is_anonymous(pool: PgPool) {
        let account = create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();
        let token = sessions::issue(&pool, account.id, -1).await.unwrap().token;

        let viewer = viewer_for(pool, Some(&format!("session={token}"))).await;

        assert!(!viewer.logged_in);
    }
}

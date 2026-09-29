use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::{
    auth::viewer::Viewer,
    db::{
        characters::{
            Character, HighscoreEntry, OnlineCharacter, find_character_by_name, highscores,
            who_is_online,
        },
        news,
    },
    error::{AppError, Surface, SurfacedError},
    state::AppState,
    template::{HtmlTemplate, Nav},
};

const NEWS_ON_HOMEPAGE: i64 = 10;
const HIGHSCORE_LIMIT: i64 = 100;
const TOP_PLAYERS_ON_HOMEPAGE: i64 = 5;

pub struct NewsItem {
    pub title: String,
    pub body: String,
    pub posted_at: String,
}

fn format_posted_at(at: time::OffsetDateTime) -> String {
    const FORMAT: &[time::format_description::BorrowedFormatItem] = time::macros::format_description!(
        "[day padding:none] [month repr:long] [year], [hour]:[minute] UTC"
    );

    at.to_offset(time::UtcOffset::UTC)
        .format(FORMAT)
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "news.html")]
pub struct NewsPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub posts: Vec<NewsItem>,
    pub online: usize,
    pub top_players: Vec<HighscoreEntry>,
}

#[derive(Template)]
#[template(path = "character_search.html")]
pub struct CharacterSearchPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub name: String,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "character_detail.html")]
pub struct CharacterDetailPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub character: Character,
    pub created: String,
}

#[derive(Template)]
#[template(path = "online.html")]
pub struct OnlinePage {
    pub viewer: Viewer,
    pub active: Nav,
    pub characters: Vec<OnlineCharacter>,
}

#[derive(Template)]
#[template(path = "highscores.html")]
pub struct HighscoresPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub entries: Vec<HighscoreEntry>,
}

#[derive(Template)]
#[template(path = "download.html")]
pub struct DownloadPage {
    pub viewer: Viewer,
    pub active: Nav,
}

#[derive(Template)]
#[template(path = "rules.html")]
pub struct RulesPage {
    pub viewer: Viewer,
    pub active: Nav,
}

#[derive(Template)]
#[template(path = "support.html")]
pub struct SupportPage {
    pub viewer: Viewer,
    pub active: Nav,
}

fn page_error(err: AppError) -> SurfacedError {
    SurfacedError(Surface::Page, err)
}

pub async fn get_news(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<impl IntoResponse, SurfacedError> {
    let posts = news::list_recent(&state.pool, NEWS_ON_HOMEPAGE)
        .await
        .map_err(page_error)?
        .into_iter()
        .map(|post| NewsItem {
            title: post.title,
            body: post.body,
            posted_at: format_posted_at(post.posted_at),
        })
        .collect();

    let online = who_is_online(&state.pool).await.map_err(page_error)?.len();
    let top_players = highscores(&state.pool, TOP_PLAYERS_ON_HOMEPAGE)
        .await
        .map_err(page_error)?;

    Ok(HtmlTemplate(NewsPage {
        viewer,
        active: Nav::News,
        posts,
        online,
        top_players,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CharacterQuery {
    pub name: Option<String>,
}

/// Exact-name lookup, redirecting to the detail page on a hit.
pub async fn get_character_search(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<CharacterQuery>,
) -> Result<Response, SurfacedError> {
    let Some(name) = query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return Ok(HtmlTemplate(CharacterSearchPage {
            viewer,
            active: Nav::Characters,
            name: String::new(),
            error: None,
        })
        .into_response());
    };

    match find_character_by_name(&state.pool, name)
        .await
        .map_err(page_error)?
    {
        Some(character) => {
            Ok(Redirect::to(&format!("/characters/{}", character.name)).into_response())
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            HtmlTemplate(CharacterSearchPage {
                viewer,
                active: Nav::Characters,
                name: name.to_string(),
                error: Some("Character not found.".to_string()),
            }),
        )
            .into_response()),
    }
}

pub async fn get_character_detail(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(name): Path<String>,
) -> Result<Response, SurfacedError> {
    let Some(character) = find_character_by_name(&state.pool, &name)
        .await
        .map_err(page_error)?
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            HtmlTemplate(CharacterSearchPage {
                viewer,
                active: Nav::Characters,
                name,
                error: Some("Character not found.".to_string()),
            }),
        )
            .into_response());
    };

    let created = character
        .created_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    Ok(HtmlTemplate(CharacterDetailPage {
        viewer,
        active: Nav::Characters,
        character,
        created,
    })
    .into_response())
}

pub async fn get_online(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<impl IntoResponse, SurfacedError> {
    let characters = who_is_online(&state.pool).await.map_err(page_error)?;
    Ok(HtmlTemplate(OnlinePage {
        viewer,
        active: Nav::Online,
        characters,
    }))
}

pub async fn get_highscores(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<impl IntoResponse, SurfacedError> {
    let entries = highscores(&state.pool, HIGHSCORE_LIMIT)
        .await
        .map_err(page_error)?;
    Ok(HtmlTemplate(HighscoresPage {
        viewer,
        active: Nav::Highscores,
        entries,
    }))
}

pub async fn get_download(viewer: Viewer) -> impl IntoResponse {
    HtmlTemplate(DownloadPage {
        viewer,
        active: Nav::Download,
    })
}

pub async fn get_rules(viewer: Viewer) -> impl IntoResponse {
    HtmlTemplate(RulesPage {
        viewer,
        active: Nav::Rules,
    })
}

pub async fn get_support(viewer: Viewer) -> impl IntoResponse {
    HtmlTemplate(SupportPage {
        viewer,
        active: Nav::Support,
    })
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
        http::{Request, header},
        routing::get,
    };
    use sqlx::PgPool;
    use tower::ServiceExt;

    fn test_app(pool: PgPool) -> Router {
        let config = SiteConfig::load("config.yaml").unwrap();
        Router::new()
            .route("/", get(get_news))
            .route("/characters", get(get_character_search))
            .route("/characters/{name}", get(get_character_detail))
            .route("/online", get(get_online))
            .route("/highscores", get(get_highscores))
            .route("/download", get(get_download))
            .route("/rules", get(get_rules))
            .route("/support", get(get_support))
            .with_state(AppState { pool, config })
    }

    async fn fetch(app: Router, uri: &str) -> (StatusCode, String, Option<String>) {
        fetch_as(app, uri, None).await
    }

    /// `fetch`, but carrying a session cookie — the public pages render the navigation,
    /// so what they show depends on who is asking.
    async fn fetch_as(
        app: Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::COOKIE, format!("session={token}"));
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).to_string(), location)
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
        crate::db::sessions::issue(pool, account.id, 7)
            .await
            .unwrap()
            .token
    }

    async fn a_character(pool: &PgPool, email: &str, name: &str) -> i32 {
        let template = SiteConfig::load("config.yaml").unwrap().new_character;
        let account = create_account(pool, email, "hunter2hunter2").await.unwrap();
        characters::create(
            pool,
            account.id,
            name,
            Vocation::Druid,
            Sex::Female,
            &template,
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_homepage_renders_with_no_news(pool: PgPool) {
        let (status, body, _) = fetch(test_app(pool), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No news yet"));
        assert!(
            body.contains(r#"aria-label="Main""#),
            "the header nav must render"
        );
        assert!(body.contains("Rustibia is a non-commercial fan project."));
        assert!(body.contains(r#"<link rel="icon" href="/static/favicon.svg""#));
    }

    async fn set_level(pool: &PgPool, character_id: i32, level: i16) {
        sqlx::query("UPDATE player_skills SET value = $1 WHERE player_id = $2 AND skill_type = 0")
            .bind(level)
            .bind(character_id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_homepage_counts_players_online(pool: PgPool) {
        let online = a_character(&pool, "a@example.com", "Rizael").await;
        a_character(&pool, "b@example.com", "Brisa").await;
        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(online)
            .execute(&pool)
            .await
            .unwrap();

        let (_, body, _) = fetch(test_app(pool), "/").await;

        assert!(body.contains("<strong>1</strong> player online"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_homepage_lists_the_five_highest_players(pool: PgPool) {
        for (i, name) in ["Aldor", "Brisa", "Cedric", "Dorian", "Elowen", "Fenwick"]
            .into_iter()
            .enumerate()
        {
            let id = a_character(&pool, &format!("p{i}@example.com"), name).await;
            set_level(&pool, id, 60 - 10 * i as i16).await;
        }

        let (_, body, _) = fetch(test_app(pool), "/").await;

        let positions: Vec<usize> = ["Aldor", "Brisa", "Cedric", "Dorian", "Elowen"]
            .iter()
            .map(|name| body.find(&format!(">{name}</a>")).expect(name))
            .collect();
        assert!(positions.is_sorted(), "highest level first");
        assert!(!body.contains(">Fenwick</a>"), "only five are shown");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_homepage_says_when_there_are_no_players(pool: PgPool) {
        let (_, body, _) = fetch(test_app(pool), "/").await;
        assert!(body.contains("No adventurers yet."));
        assert!(body.contains("<strong>0</strong> players online"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_homepage_shows_a_post(pool: PgPool) {
        let account = create_account(&pool, "admin@example.com", "hunter2hunter2")
            .await
            .unwrap();
        crate::db::news::create(&pool, "Server is open", "Come and play.", account.id)
            .await
            .unwrap();

        let (status, body, _) = fetch(test_app(pool), "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Server is open"));
        assert!(body.contains("Come and play."));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_news_post_cannot_inject_markup(pool: PgPool) {
        let account = create_account(&pool, "admin@example.com", "hunter2hunter2")
            .await
            .unwrap();
        crate::db::news::create(&pool, "<script>x</script>", "<b>bold</b>", account.id)
            .await
            .unwrap();

        let (_, body, _) = fetch(test_app(pool), "/").await;

        assert!(
            !body.contains("<script>x</script>"),
            "askama must escape the title"
        );
        assert!(!body.contains("<b>bold</b>"), "askama must escape the body");
        // askama 0.14 escapes to numeric entities (`&#60;`), not named ones (`&lt;`).
        assert!(
            body.contains("&#60;script&#62;"),
            "and render it as text instead"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_search_page_renders_empty(pool: PgPool) {
        let (status, body, _) = fetch(test_app(pool), "/characters").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Look up any adventurer by name."));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn searching_an_existing_name_redirects_to_the_detail_page(pool: PgPool) {
        a_character(&pool, "a@example.com", "Rizael").await;

        let (status, _, location) = fetch(test_app(pool), "/characters?name=rIzAeL").await;

        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            location.as_deref(),
            Some("/characters/Rizael"),
            "the redirect must use the stored capitalisation"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn searching_an_unknown_name_says_not_found(pool: PgPool) {
        let (status, body, _) = fetch(test_app(pool), "/characters?name=Nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Character not found"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_detail_page_shows_the_character(pool: PgPool) {
        a_character(&pool, "a@example.com", "Rizael").await;

        let (status, body, _) = fetch(test_app(pool), "/characters/Rizael").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Rizael"));
        assert!(body.contains("Druid"));
        assert!(body.contains("Female"));
        assert!(!body.contains("has been deleted"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_detail_page_flags_a_deleted_character(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael").await;
        sqlx::query("UPDATE players SET deleted_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let (status, body, _) = fetch(test_app(pool), "/characters/Rizael").await;

        assert_eq!(status, StatusCode::OK, "old links must still resolve");
        assert!(body.contains("has been deleted"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_detail_page_is_404(pool: PgPool) {
        let (status, _, _) = fetch(test_app(pool), "/characters/Nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_online_page_reports_the_count(pool: PgPool) {
        let id = a_character(&pool, "a@example.com", "Rizael").await;
        sqlx::query("INSERT INTO online_players (character_id) VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let (status, body, _) = fetch(test_app(pool), "/online").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Rizael"));
        assert!(body.contains("<strong>1</strong> player online"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_highscores_page_lists_characters(pool: PgPool) {
        a_character(&pool, "a@example.com", "Rizael").await;

        let (status, body, _) = fetch(test_app(pool), "/highscores").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Rizael"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_static_pages_all_render(pool: PgPool) {
        for uri in ["/download", "/rules", "/support"] {
            let (status, body, _) = fetch(test_app(pool.clone()), uri).await;
            assert_eq!(status, StatusCode::OK, "{uri} must render");
            assert!(
                body.contains("Rustibia is a non-commercial fan project."),
                "{uri} must use the site shell"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_nav_marks_the_page_being_viewed(pool: PgPool) {
        for uri in [
            "/",
            "/download",
            "/rules",
            "/support",
            "/characters",
            "/online",
            "/highscores",
        ] {
            let (_, body, _) = fetch(test_app(pool.clone()), uri).await;
            assert!(
                body.contains(&format!(r#"href="{uri}" aria-current="page""#)),
                "{uri} must be marked current"
            );
            assert_eq!(
                body.matches(r#"aria-current="page""#).count(),
                2,
                "{uri}: once in the header, once in the mobile menu, nowhere else"
            );
        }
    }

    #[test]
    fn a_posted_at_date_reads_as_prose_not_rfc_3339() {
        let at = time::macros::datetime!(2026-08-10 14:03:22 UTC);
        assert_eq!(format_posted_at(at), "10 August 2026, 14:03 UTC");
    }

    /// The stored offset must not change which day a post claims to be from.
    #[test]
    fn a_posted_at_date_is_normalised_to_utc() {
        let at = time::macros::datetime!(2026-08-10 01:30:00 +05:00);
        assert_eq!(format_posted_at(at), "9 August 2026, 20:30 UTC");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_post_shows_when_it_was_posted(pool: PgPool) {
        let account = create_account(&pool, "admin@example.com", "hunter2hunter2")
            .await
            .unwrap();
        crate::db::news::create(&pool, "Server is open", "Come and play.", account.id)
            .await
            .unwrap();
        sqlx::query("UPDATE news_posts SET posted_at = '2026-08-10 14:03:22+00'")
            .execute(&pool)
            .await
            .unwrap();

        let (_, body, _) = fetch(test_app(pool), "/").await;

        assert!(body.contains("Posted at: 10 August 2026, 14:03 UTC"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_public_page_shows_the_logged_out_menu_to_a_stranger(pool: PgPool) {
        let (_, body, _) = fetch(test_app(pool), "/").await;

        assert!(body.contains(r#"href="/login""#));
        assert!(body.contains(r#"href="/register""#));
        assert!(!body.contains(r#"href="/logout""#));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_public_page_shows_the_logged_in_menu_to_a_player(pool: PgPool) {
        let token = session_for(&pool, "player@example.com", false).await;

        for uri in ["/", "/online", "/highscores", "/characters", "/rules"] {
            let (status, body, _) = fetch_as(test_app(pool.clone()), uri, Some(&token)).await;

            assert_eq!(status, StatusCode::OK, "{uri} must render");
            assert!(
                body.contains(r#"href="/logout""#),
                "{uri} must offer Log Out to a logged-in player"
            );
            assert!(
                body.contains(r#"href="/account""#),
                "{uri} must link a logged-in player to their account"
            );
            assert!(
                !body.contains(r#"href="/login""#),
                "{uri} must not invite a logged-in player to log in again"
            );
            assert!(
                !body.contains("Post News"),
                "{uri} must not show a player the admin menu"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_public_page_shows_the_admin_menu_to_an_admin(pool: PgPool) {
        let token = session_for(&pool, "admin@example.com", true).await;

        let (_, body, _) = fetch_as(test_app(pool), "/", Some(&token)).await;

        assert!(body.contains(r#"href="/logout""#));
        assert!(body.contains("Post News"), "an admin keeps the admin menu");
    }

    /// A stale cookie must not cost a reader the page — the nav degrades instead.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_expired_session_still_renders_the_page(pool: PgPool) {
        let account = create_account(&pool, "player@example.com", "hunter2hunter2")
            .await
            .unwrap();
        let token = crate::db::sessions::issue(&pool, account.id, -1)
            .await
            .unwrap()
            .token;

        let (status, body, _) = fetch_as(test_app(pool), "/", Some(&token)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"href="/login""#));
    }
}

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, header},
};
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ClientVersionResponse {
    pub version: String,
    pub download_url: String,
}

pub async fn get_client_version(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<ClientVersionResponse> {
    Json(ClientVersionResponse {
        version: state.config.client_version.clone(),
        download_url: download_url(&headers),
    })
}

fn download_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("{proto}://{host}/download")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SiteConfig;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;

    #[sqlx::test(migrations = "./migrations")]
    async fn reports_the_configured_version_and_where_to_download_it(pool: PgPool) {
        let config = SiteConfig::load("config.yaml").unwrap();
        let expected = config.client_version.clone();
        let app = Router::new()
            .route("/api/client-version", get(get_client_version))
            .with_state(AppState { pool, config });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/client-version")
                    .header(header::HOST, "rustibia.online")
                    .header("x-forwarded-proto", "https")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["version"], expected);
        assert_eq!(body["download_url"], "https://rustibia.online/download");
    }

    #[test]
    fn a_request_without_a_forwarded_proto_is_plain_http() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());

        assert_eq!(download_url(&headers), "http://127.0.0.1:8080/download");
    }
}

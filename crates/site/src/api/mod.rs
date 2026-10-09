pub mod auth;
pub mod characters;
pub mod client_version;
pub mod internal;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth", post(auth::post_auth))
        .route("/client-version", get(client_version::get_client_version))
        .route("/characters", get(characters::get_characters))
        .route(
            "/characters/{id}/token",
            post(characters::post_character_token),
        )
}

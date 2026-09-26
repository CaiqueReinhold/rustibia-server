use askama::Template;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{
    auth::viewer::Viewer,
    template::{HtmlTemplate, Nav},
};

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub viewer: Viewer,
    pub active: Nav,
    pub message: String,
}

/// Whether a failing route should answer with HTML or JSON. Set when the error is
/// constructed, because by the time `into_response` runs there is no request to inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Page,
    Api,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid email or password")]
    InvalidCredentials,
    #[error("not authenticated")]
    Unauthenticated,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Validation(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("password hashing error: {0}")]
    Password(#[from] crate::auth::password::PasswordError),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::InvalidCredentials | AppError::Unauthenticated => StatusCode::UNAUTHORIZED,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Database(_) | AppError::Password(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The message safe to show a client. Internal failures are never described.
    pub fn public_message(&self) -> String {
        match self {
            AppError::Database(_) | AppError::Password(_) => {
                "Something went wrong. Please try again.".to_string()
            }
            other => other.to_string(),
        }
    }
}

/// An `AppError` tagged with the surface that produced it.
pub struct SurfacedError(pub Surface, pub AppError);

impl IntoResponse for SurfacedError {
    fn into_response(self) -> Response {
        let SurfacedError(surface, err) = self;

        if matches!(err, AppError::Database(_) | AppError::Password(_)) {
            tracing::error!("internal error: {err}");
        }

        let status = err.status();
        let message = err.public_message();

        match surface {
            Surface::Api => (status, Json(json!({ "error": message }))).into_response(),
            // The anonymous nav, not the caller's: by the time an error renders there
            // is no request left to resolve a viewer from. Same reason `Surface` has to
            // be chosen when the error is built.
            Surface::Page => (
                status,
                HtmlTemplate(ErrorPage {
                    viewer: Viewer::ANONYMOUS,
                    active: Nav::None,
                    message,
                }),
            )
                .into_response(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        SurfacedError(Surface::Api, self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_failure_is_401() {
        assert_eq!(
            AppError::InvalidCredentials.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn validation_failure_is_422() {
        assert_eq!(
            AppError::Validation("bad".into()).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn database_errors_never_leak_their_detail() {
        let err = AppError::Database(sqlx::Error::RowNotFound);
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            err.public_message(),
            "Something went wrong. Please try again."
        );
        assert!(!err.public_message().contains("RowNotFound"));
    }

    #[test]
    fn credential_failure_message_does_not_say_which_half_was_wrong() {
        let msg = AppError::InvalidCredentials.public_message();
        assert!(!msg.to_lowercase().contains("email not found"));
        assert!(!msg.to_lowercase().contains("wrong password"));
    }
}

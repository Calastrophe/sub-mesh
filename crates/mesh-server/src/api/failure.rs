use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mesh_core::ApiError;

/// A failed management API call: a status and an [`ApiError`] body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiFailure {
    status: StatusCode,
    message: String,
}

impl ApiFailure {
    /// A failure with `status`, for example a status the daemon sent.
    pub fn new(status: StatusCode, message: impl Into<String>) -> ApiFailure {
        ApiFailure {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiFailure {
        ApiFailure::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden(message: impl Into<String>) -> ApiFailure {
        ApiFailure::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> ApiFailure {
        ApiFailure::new(StatusCode::NOT_FOUND, message)
    }

    /// The owning daemon cannot be reached or answered badly.
    pub fn bad_gateway(message: impl Into<String>) -> ApiFailure {
        ApiFailure::new(StatusCode::BAD_GATEWAY, message)
    }

    pub fn internal(message: impl Into<String>) -> ApiFailure {
        ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiError {
                error: self.message,
            }),
        )
            .into_response()
    }
}

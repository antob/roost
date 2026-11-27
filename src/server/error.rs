use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

#[derive(Debug)]
pub enum AppError {
    Status(StatusCode, String),
    Contract {
        code: &'static str,
        message: String,
        extra: Map<String, Value>,
    },
    Internal(anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::Status(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn unauthorized() -> Self {
        Self::Status(
            StatusCode::UNAUTHORIZED,
            "missing or invalid Bearer token".into(),
        )
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Status(StatusCode::FORBIDDEN, msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::Status(StatusCode::NOT_FOUND, msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Status(StatusCode::CONFLICT, msg.into())
    }
    pub fn contract(code: &'static str, msg: impl Into<String>) -> Self {
        Self::Contract {
            code,
            message: msg.into(),
            extra: Map::new(),
        }
    }
    pub fn contract_with(code: &'static str, msg: impl Into<String>, extra: Value) -> Self {
        let extra = match extra {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        Self::Contract {
            code,
            message: msg.into(),
            extra,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status, msg) => (status, Json(json!({ "error": msg }))).into_response(),
            Self::Contract {
                code,
                message,
                mut extra,
            } => {
                extra.insert("error".into(), Value::String(message));
                extra.insert("code".into(), Value::String(code.into()));
                (StatusCode::UNPROCESSABLE_ENTITY, Json(Value::Object(extra))).into_response()
            }
            Self::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal server error" })),
                )
                    .into_response()
            }
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

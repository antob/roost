use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use percent_encoding::percent_decode_str;

use crate::server::AppState;
use crate::server::error::AppError;
use crate::util::sha256_hex;

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// A request authenticated with a channel key (`rst_…`).
pub struct ChannelAuth {
    pub channel_id: i64,
}

impl FromRequestParts<AppState> for ChannelAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = bearer(parts).ok_or_else(AppError::unauthorized)?;
        let hash = sha256_hex(token.as_bytes());
        let row = sqlx::query_scalar!("SELECT id FROM channels WHERE key_hash = ?", hash)
            .fetch_optional(&state.db)
            .await?;
        match row {
            Some(id) => Ok(Self { channel_id: id }),
            None => Err(AppError::unauthorized()),
        }
    }
}

/// The advisory per-request agent identity headers the CLI sends.
pub struct Identity {
    pub fingerprint: Option<String>,
    pub label: Option<String>,
    pub kind: Option<String>,
}

impl Identity {
    pub fn attribution(&self) -> Option<String> {
        self.label.clone().or_else(|| self.fingerprint.clone())
    }

    pub fn who(&self) -> crate::server::presence::Who<'_> {
        crate::server::presence::Who {
            fingerprint: self.fingerprint.as_deref(),
            label: self.label.as_deref(),
            kind: self.kind.as_deref(),
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Identity {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let header = |name: &str| {
            parts
                .headers
                .get(format!("x-roost-{name}"))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let fingerprint = header("fingerprint").map(|s| s.chars().take(64).collect());
        let label = header("label").map(|s| {
            percent_decode_str(s)
                .decode_utf8_lossy()
                .chars()
                .take(80)
                .collect::<String>()
        });
        let kind = header("consumer-kind").map(|s| s.chars().take(32).collect());
        Ok(Self {
            fingerprint,
            label,
            kind,
        })
    }
}

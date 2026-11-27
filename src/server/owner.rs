//! The single owner: passkey enrollment and login, sessions, and the `Human` extractor.

use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;
use url::Url;
use webauthn_rs::prelude::*;

use crate::server::AppState;
use crate::server::error::{AppError, AppResult};
use crate::util::{now, now_plus, random_token, sha256_hex};

pub const SESSION_COOKIE: &str = "roost_session";
pub const SESSION_TTL_SECS: i64 = 90 * 24 * 3600;
const ENROLL_TTL_SECS: i64 = 15 * 60;
const CHALLENGE_TTL_SECS: i64 = 5 * 60;

pub fn webauthn(base_url: &str) -> anyhow::Result<Webauthn> {
    let origin = Url::parse(base_url)?;
    let rp_id = origin
        .domain()
        .ok_or_else(|| anyhow::anyhow!("base URL {base_url} has no domain name"))?
        .to_string();
    Ok(WebauthnBuilder::new(&rp_id, &origin)?
        .rp_name("roost")
        .build()?)
}

pub fn webauthn_or_404(st: &AppState) -> AppResult<&Webauthn> {
    st.webauthn
        .as_ref()
        .ok_or_else(|| AppError::not_found("passkeys need a base URL with a domain name"))
}

pub async fn setting(db: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!("SELECT value FROM settings WHERE key = ?", key)
        .fetch_optional(db)
        .await
}

/// Returns the stored value for `key`, storing `make()` first if there is none.
pub async fn setting_or_init(
    db: &SqlitePool,
    key: &str,
    make: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    if let Some(v) = setting(db, key).await? {
        return Ok(v);
    }
    let v = make()?;
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO NOTHING",
        key,
        v
    )
    .execute(db)
    .await?;
    Ok(setting(db, key).await?.unwrap_or(v))
}

pub async fn owner_uuid(db: &SqlitePool) -> anyhow::Result<Uuid> {
    let raw = setting_or_init(db, "owner_uuid", || Ok(Uuid::new_v4().to_string())).await?;
    Ok(Uuid::parse_str(&raw)?)
}

pub async fn mint_enrollment_code(db: &SqlitePool) -> sqlx::Result<(String, String)> {
    let code = random_token(24);
    let hash = sha256_hex(code.as_bytes());
    let expires_at = now_plus(ENROLL_TTL_SECS);
    sqlx::query!(
        "INSERT INTO enrollment_codes (code_hash, expires_at) VALUES (?, ?)",
        hash,
        expires_at
    )
    .execute(db)
    .await?;
    Ok((code, expires_at))
}

/// Validates an unused, unexpired enrollment code and returns its hash.
pub async fn check_enrollment_code(db: &SqlitePool, code: &str) -> AppResult<String> {
    let hash = sha256_hex(code.trim().as_bytes());
    let at = now();
    sqlx::query_scalar!(
        "SELECT code_hash as \"code_hash!\" FROM enrollment_codes WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?",
        hash,
        at
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::forbidden("enrollment code is invalid, used or expired"))
}

pub async fn passkeys(db: &SqlitePool) -> sqlx::Result<Vec<(i64, Passkey)>> {
    let rows = sqlx::query!(r#"SELECT id as "id!", passkey FROM passkeys ORDER BY id"#)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| Some((r.id, serde_json::from_str(&r.passkey).ok()?)))
        .collect())
}

pub struct Challenge {
    pub kind: &'static str,
    pub enrollment_code_hash: Option<String>,
    pub notification_id: Option<i64>,
    pub action_id: Option<String>,
}

pub async fn store_challenge(
    db: &SqlitePool,
    c: Challenge,
    state: &impl Serialize,
) -> AppResult<String> {
    let id = random_token(32);
    let state = serde_json::to_string(state).map_err(|e| AppError::Internal(e.into()))?;
    let expires_at = now_plus(CHALLENGE_TTL_SECS);
    sqlx::query!(
        "INSERT INTO webauthn_challenges (id, kind, state, enrollment_code_hash, notification_id, action_id, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        c.kind,
        state,
        c.enrollment_code_hash,
        c.notification_id,
        c.action_id,
        expires_at
    )
    .execute(db)
    .await?;
    Ok(id)
}

pub struct TakenChallenge<T> {
    pub state: T,
    pub enrollment_code_hash: Option<String>,
    pub notification_id: Option<i64>,
    pub action_id: Option<String>,
}

/// Consumes a challenge (single use) of the given kind.
pub async fn take_challenge<T: DeserializeOwned>(
    db: &SqlitePool,
    id: &str,
    kind: &str,
) -> AppResult<TakenChallenge<T>> {
    let at = now();
    let row = sqlx::query!(
        "DELETE FROM webauthn_challenges WHERE id = ? AND kind = ? AND expires_at > ?
         RETURNING state, enrollment_code_hash, notification_id, action_id",
        id,
        kind,
        at
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::forbidden("challenge is unknown or expired"))?;
    Ok(TakenChallenge {
        state: serde_json::from_str(&row.state).map_err(|e| AppError::Internal(e.into()))?,
        enrollment_code_hash: row.enrollment_code_hash,
        notification_id: row.notification_id,
        action_id: row.action_id,
    })
}

/// Verifies a passkey assertion (user verification is always required) and bumps its counter.
pub async fn verify_assertion(
    st: &AppState,
    credential: &PublicKeyCredential,
    state: &PasskeyAuthentication,
) -> AppResult<i64> {
    let wa = webauthn_or_404(st)?;
    let result = wa
        .finish_passkey_authentication(credential, state)
        .map_err(|e| AppError::forbidden(format!("passkey verification failed: {e}")))?;
    if !result.user_verified() {
        return Err(AppError::forbidden("user verification is required"));
    }
    let cred_id = cred_id_text(result.cred_id());
    let row = sqlx::query!(
        r#"SELECT id as "id!", passkey FROM passkeys WHERE cred_id = ?"#,
        cred_id
    )
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| AppError::forbidden("unknown passkey"))?;
    let mut pk: Passkey =
        serde_json::from_str(&row.passkey).map_err(|e| AppError::Internal(e.into()))?;
    pk.update_credential(&result);
    let json = serde_json::to_string(&pk).map_err(|e| AppError::Internal(e.into()))?;
    let at = now();
    sqlx::query!(
        "UPDATE passkeys SET passkey = ?, last_used_at = ? WHERE id = ?",
        json,
        at,
        row.id
    )
    .execute(&st.db)
    .await?;
    Ok(row.id)
}

pub fn cred_id_text(id: &CredentialID) -> String {
    let bytes: &[u8] = id.as_ref();
    hex::encode(bytes)
}

pub async fn create_session(
    db: &SqlitePool,
    passkey_id: i64,
    user_agent: Option<&str>,
) -> sqlx::Result<String> {
    let token = random_token(43);
    let hash = sha256_hex(token.as_bytes());
    let at = now();
    let expires_at = now_plus(SESSION_TTL_SECS);
    sqlx::query!(
        "INSERT INTO sessions (token_hash, passkey_id, user_agent, created_at, expires_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        hash,
        passkey_id,
        user_agent,
        at,
        expires_at,
        at
    )
    .execute(db)
    .await?;
    Ok(token)
}

pub fn session_cookie(st: &AppState, token: &str, max_age: i64) -> String {
    let secure = if st.config.base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    )
}

fn cookie_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())
}

/// Who is acting on the human side.
pub enum Human {
    /// The static admin token (tests and development). It skips the biometric gate.
    Admin,
    /// A passkey session from the PWA.
    Owner { token_hash: String },
}

impl Human {
    pub fn needs_gate(&self) -> bool {
        matches!(self, Human::Owner { .. })
    }
}

impl FromRequestParts<AppState> for Human {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, AppError> {
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if let Some(token) = bearer {
            return match st.config.admin_token.as_deref() {
                Some(expected) if expected == token => Ok(Human::Admin),
                _ => Err(AppError::unauthorized()),
            };
        }
        let token = cookie_token(parts).ok_or_else(AppError::unauthorized)?;
        let hash = sha256_hex(token.as_bytes());
        let at = now();
        let found = sqlx::query!(
            "UPDATE sessions SET last_seen_at = ? WHERE token_hash = ? AND expires_at > ? RETURNING token_hash",
            at,
            hash,
            at
        )
        .fetch_optional(&st.db)
        .await?;
        match found {
            Some(_) => Ok(Human::Owner { token_hash: hash }),
            None => Err(AppError::unauthorized()),
        }
    }
}

impl axum::extract::OptionalFromRequestParts<AppState> for Human {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &AppState,
    ) -> Result<Option<Self>, AppError> {
        match <Human as FromRequestParts<AppState>>::from_request_parts(parts, st).await {
            Ok(h) => Ok(Some(h)),
            Err(AppError::Status(code, _)) if code == axum::http::StatusCode::UNAUTHORIZED => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

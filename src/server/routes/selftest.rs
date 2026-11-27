//! `roost selftest`: a nonce on the channel's own queue that only a live listener can ack in time.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::server::AppState;
use crate::server::auth::ChannelAuth;
use crate::server::error::{AppError, AppResult};
use crate::util::{now, now_plus, random_token};

pub async fn create(
    State(st): State<AppState>,
    auth: ChannelAuth,
    body: Result<Json<Value>, JsonRejection>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let window = body
        .ok()
        .and_then(|Json(b)| b.get("window_seconds").and_then(Value::as_i64))
        .unwrap_or(30)
        .clamp(5, 600);
    let nonce = random_token(16);
    let expires_at = now_plus(window);
    let reference =
        json!({ "type": "selftest", "nonce": nonce, "expires_at": expires_at }).to_string();
    let at = now();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO messages (channel_id, kind, body, ref, created_at) VALUES (?, 'system', ?, ?, ?)
           RETURNING id as "id!""#,
        auth.channel_id,
        "selftest nonce — ack it to prove your listener works",
        reference,
        at
    )
    .fetch_one(&st.db)
    .await?;
    let _ = st.hub.messages.send(auth.channel_id);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id, "status": "pending", "nonce": nonce,
            "window_seconds": window, "expires_at": expires_at,
        })),
    ))
}

pub async fn show(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let row = sqlx::query!(
        "SELECT ref, delivered_at, processed_at FROM messages WHERE id = ? AND channel_id = ? AND kind = 'system'",
        id,
        auth.channel_id
    )
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| AppError::not_found("no such selftest"))?;
    let reference: Value = row
        .r#ref
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or(Value::Null);
    if reference["type"] != "selftest" {
        return Err(AppError::not_found("no such selftest"));
    }
    let expires_at = reference["expires_at"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let status = match &row.processed_at {
        Some(p) if *p <= expires_at => "passed",
        Some(_) => "failed",
        None if now() > expires_at => "failed",
        None => "pending",
    };
    Ok(Json(json!({
        "id": id, "status": status, "nonce": reference["nonce"], "expires_at": expires_at,
        "delivered_at": row.delivered_at, "processed_at": row.processed_at,
    })))
}

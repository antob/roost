use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppState;
use crate::server::auth::{ChannelAuth, Identity};
use crate::server::channels;
use crate::server::error::{AppError, AppResult};
use crate::util::{new_channel_key, now, parse_ts, sha256_hex};

#[derive(Deserialize)]
pub struct ClaimBody {
    code: String,
}

fn unknown_code() -> AppError {
    AppError::not_found("claim code unknown, expired or already used")
}

pub async fn claim(
    State(st): State<AppState>,
    ident: Identity,
    Json(body): Json<ClaimBody>,
) -> AppResult<Json<Value>> {
    let code = body.code.trim().to_uppercase();
    let row = sqlx::query!(
        "SELECT channel_id, expires_at, exchanged_at, exchanged_fingerprint FROM claim_codes WHERE code = ?",
        code
    )
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(unknown_code)?;

    let expired = parse_ts(&row.expires_at).is_none_or(|t| t < chrono::Utc::now());
    // A consumed code may be re-exchanged within its TTL by the same install (setup retry).
    let retry_ok = row.exchanged_at.is_none()
        || (ident.fingerprint.is_some() && row.exchanged_fingerprint == ident.fingerprint);
    if expired || !retry_ok {
        return Err(unknown_code());
    }

    let channel = channels::load(&st.db, row.channel_id)
        .await?
        .ok_or_else(unknown_code)?;
    let key = new_channel_key();
    let key_hash = sha256_hex(key.as_bytes());
    let new_owner =
        channel.claim_generation == 0 || channel.claimed_by_fingerprint != ident.fingerprint;
    let generation = channel.claim_generation + i64::from(new_owner);
    let at = now();
    let claimed_at = if new_owner {
        Some(at.clone())
    } else {
        channel.claimed_at.clone()
    };
    let label = ident.label.clone().or(channel.claimed_by_label.clone());

    let mut tx = st.db.begin().await?;
    sqlx::query!(
        "UPDATE channels SET key_hash = ?, claim_generation = ?, claimed_by_fingerprint = ?, claimed_by_label = ?,
                claimed_at = ? WHERE id = ?",
        key_hash,
        generation,
        ident.fingerprint,
        label,
        claimed_at,
        channel.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE claim_codes SET exchanged_at = ?, exchanged_fingerprint = ? WHERE code = ?",
        at,
        ident.fingerprint,
        code
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    st.changed(channel.id);

    Ok(Json(json!({
        "key": key,
        "channel": channel.brief(),
        "base_url": st.config.base_url,
    })))
}

#[derive(Deserialize)]
pub struct OwnershipBody {
    fingerprint: Option<String>,
    label: Option<String>,
}

pub async fn ownership(
    State(st): State<AppState>,
    auth: ChannelAuth,
    ident: Identity,
    Json(body): Json<OwnershipBody>,
) -> AppResult<Json<Value>> {
    let channel = channels::load(&st.db, auth.channel_id)
        .await?
        .ok_or_else(AppError::unauthorized)?;
    let fingerprint = body.fingerprint.or(ident.fingerprint);
    let label = body
        .label
        .or(ident.label)
        .map(|l| l.chars().take(80).collect::<String>());
    // claimed_at and the generation move only when a different install takes over.
    let changed = fingerprint.is_some() && fingerprint != channel.claimed_by_fingerprint;
    let generation =
        channel.claim_generation + i64::from(changed && channel.claimed_by_fingerprint.is_some());
    let claimed_at = if changed {
        Some(now())
    } else {
        channel.claimed_at.clone()
    };
    sqlx::query!(
        "UPDATE channels SET claimed_by_fingerprint = COALESCE(?, claimed_by_fingerprint),
                claimed_by_label = COALESCE(?, claimed_by_label), claimed_at = ?, claim_generation = ? WHERE id = ?",
        fingerprint,
        label,
        claimed_at,
        generation,
        channel.id
    )
    .execute(&st.db)
    .await?;
    let channel = channels::load(&st.db, auth.channel_id)
        .await?
        .ok_or_else(AppError::unauthorized)?;
    st.changed(channel.id);
    Ok(Json(json!({ "claim": channel.claim() })))
}

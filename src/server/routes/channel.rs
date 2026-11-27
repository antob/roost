use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Map, Value, json};

use crate::server::AppState;
use crate::server::auth::ChannelAuth;
use crate::server::channels::{self, Channel};
use crate::server::error::{AppError, AppResult};

async fn own_channel(st: &AppState, auth: &ChannelAuth, id: Option<i64>) -> AppResult<Channel> {
    if id.is_some_and(|id| id != auth.channel_id) {
        return Err(AppError::forbidden(
            "this key speaks for a different channel",
        ));
    }
    channels::load(&st.db, auth.channel_id)
        .await?
        .ok_or_else(AppError::unauthorized)
}

pub async fn whoami(State(st): State<AppState>, auth: ChannelAuth) -> AppResult<Json<Value>> {
    let ch = own_channel(&st, &auth, None).await?;
    let devices = crate::server::push::device_count(&st.db).await?;
    Ok(Json(json!({
        "channel": ch.brief(),
        "operating_contract": ch.operating_contract(),
        "notify_contract": ch.notify_contract(),
        "user": { "name": null, "timezone": st.config.timezone, "plan_state": "active", "plan_tier": "pro" },
        "claim": ch.claim(),
        "stale_from_prior_claim": false,
        "devices": devices,
        "device_reach": { "total": devices, "pushable": devices, "deliverable": devices },
        "listening": st.presence.listening(ch.id),
        "listening_state": if st.presence.listening(ch.id) { "listening" } else { "offline" },
        "consumer_conflict": st.presence.consumers(ch.id).iter().filter(|c| c["live"] == true).count() > 1,
        "stuck_message_count": 0,
        "consumers": st.presence.consumers(ch.id),
        "e2e_enabled": false,
    })))
}

pub async fn list(State(st): State<AppState>, auth: ChannelAuth) -> AppResult<Json<Value>> {
    let ch = own_channel(&st, &auth, None).await?;
    Ok(Json(json!({ "channels": [ch.full()] })))
}

pub async fn show(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    Ok(Json(own_channel(&st, &auth, Some(id)).await?.full()))
}

fn validate_operating_contract(patch: &Map<String, Value>) -> AppResult<()> {
    for (key, value) in patch {
        let ok = match key.as_str() {
            "keep_connection_alive" | "mirror_in_origin_session" | "quiet_when_idle" => {
                value.is_boolean() || value.is_null()
            }
            "listen_mode" => {
                value.is_null()
                    || value.as_str().is_some_and(|m| {
                        ["turn_based", "persistent", "external_daemon", "always_on"].contains(&m)
                    })
            }
            _ => false,
        };
        if !ok {
            return Err(AppError::contract(
                "invalid_operating_contract",
                format!("invalid operating_contract key or value: {key}"),
            ));
        }
    }
    Ok(())
}

pub async fn update(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Path(id): Path<i64>,
    Json(body): Json<Map<String, Value>>,
) -> AppResult<Json<Value>> {
    let ch = own_channel(&st, &auth, Some(id)).await?;
    if body.contains_key("notify_contract") {
        return Err(AppError::forbidden(
            "notify_contract is the human's to set; a channel key can only read it",
        ));
    }
    if let Some(patch) = body.get("operating_contract") {
        let patch = patch.as_object().ok_or_else(|| {
            AppError::contract(
                "invalid_operating_contract",
                "operating_contract must be an object",
            )
        })?;
        validate_operating_contract(patch)?;
        let mut merged = ch
            .operating_contract()
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (k, v) in patch {
            if v.is_null() {
                merged.remove(k);
            } else {
                merged.insert(k.clone(), v.clone());
            }
        }
        let stored = Value::Object(merged).to_string();
        sqlx::query!(
            "UPDATE channels SET operating_contract = ? WHERE id = ?",
            stored,
            ch.id
        )
        .execute(&st.db)
        .await?;
    }
    let ch = own_channel(&st, &auth, Some(id)).await?;
    Ok(Json(ch.full()))
}

pub const TYPING_DEFAULT_SECS: i64 = 60;
const TYPING_SECS: std::ops::RangeInclusive<i64> = 3..=300;

/// `{seconds}` shows the typing dots for that long (default 60, clamped to 3..300); 0 clears them.
pub async fn typing(
    State(st): State<AppState>,
    auth: ChannelAuth,
    body: Option<Json<Map<String, Value>>>,
) -> AppResult<Json<Value>> {
    let asked = match body.as_ref().and_then(|b| b.get("seconds")) {
        None | Some(Value::Null) => TYPING_DEFAULT_SECS,
        Some(v) => v.as_i64().ok_or_else(|| {
            AppError::contract("invalid_seconds", "seconds must be a whole number")
        })?,
    };
    let out = if asked <= 0 {
        st.stop_typing(auth.channel_id);
        json!({ "typing": false, "expires_at": null })
    } else {
        let secs = asked.clamp(*TYPING_SECS.start(), *TYPING_SECS.end());
        let until = st.start_typing(auth.channel_id, secs);
        json!({ "typing": true, "seconds": secs, "clamped": secs != asked, "expires_at": until })
    };
    st.changed(auth.channel_id);
    Ok(Json(out))
}

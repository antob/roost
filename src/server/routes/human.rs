use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::channels;
use crate::server::error::{AppError, AppResult};
use crate::server::owner::{self, Human};
use crate::server::routes::notifications::{Notification, builtin, load_by_cid, load_by_id};
use crate::server::{AppState, EventKind, webhook};
use crate::util::{now, now_plus};
use webauthn_rs::prelude::{PasskeyAuthentication, PublicKeyCredential};

#[derive(Deserialize)]
pub struct CreateChannel {
    name: String,
}

pub async fn create_channel(
    State(st): State<AppState>,
    _: Human,
    Json(body): Json<CreateChannel>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    let (id, code) = channels::create(&st.db, name).await?;
    st.changed(id);
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "channel": { "id": id, "name": name }, "claim_code": code.code, "expires_at": code.expires_at }),
        ),
    ))
}

pub async fn delete_channel(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    if !channels::delete(&st.db, id).await? {
        return Err(AppError::not_found("no such channel"));
    }
    st.presence.forget(id);
    st.changed(id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UpdateChannel {
    name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    color: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    icon: Option<Option<String>>,
    max_urgency: Option<String>,
    escalate_after_secs: Option<i64>,
    default_profile: Option<String>,
}

/// The owner's channel settings: name, look, urgency ceiling, re-alert delay and default send type.
pub async fn update_channel(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
    Json(body): Json<UpdateChannel>,
) -> AppResult<Json<Value>> {
    let ch = channels::load(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such channel"))?;
    let name = match body.name.as_deref().map(str::trim) {
        Some("") => return Err(AppError::bad_request("name can't be empty")),
        Some(n) => n.chars().take(80).collect(),
        None => ch.name,
    };
    let color = match body.color {
        Some(Some(c)) if !is_hex_color(&c) => {
            return Err(AppError::bad_request("color must be #rrggbb"));
        }
        Some(c) => c,
        None => ch.color,
    };
    let icon = match body.icon {
        Some(Some(i))
            if i.is_empty()
                || i.len() > 32
                || !i.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') =>
        {
            return Err(AppError::bad_request("icon must be a glyph name (a-z, -)"));
        }
        Some(i) => i,
        None => ch.icon,
    };
    let max_urgency = match body.max_urgency {
        Some(u) if !["normal", "persistent", "alarm"].contains(&u.as_str()) => {
            return Err(AppError::bad_request(
                "max_urgency is normal, persistent or alarm",
            ));
        }
        Some(u) => u,
        None => ch.max_urgency,
    };
    let escalate_after_secs = match body.escalate_after_secs {
        Some(s) if !(60..=86_400).contains(&s) => {
            return Err(AppError::bad_request("escalate_after_secs is 60..86400"));
        }
        Some(s) => s,
        None => ch.escalate_after_secs,
    };
    let default_profile = match body.default_profile {
        Some(p) if !["message", "important", "urgent"].contains(&p.as_str()) => {
            return Err(AppError::bad_request(
                "default_profile is message, important or urgent",
            ));
        }
        Some(p) => p,
        None => ch.default_profile,
    };
    sqlx::query!(
        "UPDATE channels SET name = ?, color = ?, icon = ?, max_urgency = ?, escalate_after_secs = ?,
                default_profile = ? WHERE id = ?",
        name,
        color,
        icon,
        max_urgency,
        escalate_after_secs,
        default_profile,
        id
    )
    .execute(&st.db)
    .await?;
    st.changed(id);
    let ch = channels::load(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such channel"))?;
    Ok(Json(ch.full()))
}

/// Tells an explicit `null` (clear it) apart from a missing field (leave it).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

fn is_hex_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn claim_code(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    channels::load(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such channel"))?;
    let code = channels::mint_claim_code(&st.db, id).await?;
    Ok(Json(
        json!({ "code": code.code, "expires_at": code.expires_at }),
    ))
}

#[derive(Deserialize)]
pub struct PostMessage {
    body: String,
    thread_id: Option<String>,
}

pub async fn post_message(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
    Json(body): Json<PostMessage>,
) -> AppResult<(StatusCode, Json<Value>)> {
    channels::load(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such channel"))?;
    let at = now();
    let mid = sqlx::query_scalar!(
        r#"INSERT INTO messages (channel_id, kind, body, thread_id, created_at) VALUES (?, 'message', ?, ?, ?)
           RETURNING id as "id!""#,
        id,
        body.body,
        body.thread_id,
        at
    )
    .fetch_one(&st.db)
    .await?;
    let _ = st.hub.messages.send(id);
    st.changed(id);
    Ok((StatusCode::CREATED, Json(json!({ "id": mid }))))
}

#[derive(Deserialize)]
pub struct Respond {
    action_id: String,
    text: Option<String>,
    snooze_minutes: Option<i64>,
    snooze_seconds: Option<i64>,
    gate: Option<Gate>,
}

#[derive(Deserialize)]
pub struct Gate {
    challenge_id: String,
    credential: PublicKeyCredential,
}

fn snooze_default_minutes(profile: &str) -> i64 {
    if profile == "important" { 30 } else { 10 }
}

pub async fn respond(
    State(st): State<AppState>,
    human: Human,
    Path((channel_id, cid)): Path<(i64, String)>,
    Json(body): Json<Respond>,
) -> AppResult<Json<Value>> {
    let n = load_by_cid(&st.db, channel_id, &cid)
        .await?
        .ok_or_else(|| AppError::not_found("no such notification"))?;
    respond_to(&st, &human, &n, body).await
}

pub async fn respond_by_id(
    State(st): State<AppState>,
    human: Human,
    Path(id): Path<i64>,
    Json(body): Json<Respond>,
) -> AppResult<Json<Value>> {
    let n = load_by_id(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such notification"))?;
    respond_to(&st, &human, &n, body).await
}

pub fn is_biometric(n: &Notification, action_id: &str) -> bool {
    n.custom_actions().iter().any(|c| {
        c.get("id").and_then(Value::as_str) == Some(action_id)
            && c.get("biometric").and_then(Value::as_bool) == Some(true)
    })
}

async fn respond_to(
    st: &AppState,
    human: &Human,
    n: &Notification,
    body: Respond,
) -> AppResult<Json<Value>> {
    if human.needs_gate() && is_biometric(n, &body.action_id) {
        let gate = body
            .gate
            .ok_or_else(|| AppError::forbidden("this action needs Face ID: start a gate first"))?;
        let taken =
            owner::take_challenge::<PasskeyAuthentication>(&st.db, &gate.challenge_id, "gate")
                .await?;
        if taken.notification_id != Some(n.id)
            || taken.action_id.as_deref() != Some(body.action_id.as_str())
        {
            return Err(AppError::forbidden(
                "the gate was opened for a different action",
            ));
        }
        owner::verify_assertion(st, &gate.credential, &taken.state).await?;
    }
    let chosen = record_answer(
        st,
        n,
        &body.action_id,
        body.text,
        body.snooze_seconds.or(body.snooze_minutes.map(|m| m * 60)),
    )
    .await?;
    Ok(Json(chosen))
}

/// Apply the human's answer: store it, mirror it onto the queue, wake waiters, fire the webhook.
pub async fn record_answer(
    st: &AppState,
    n: &Notification,
    action_id: &str,
    text: Option<String>,
    snooze_secs: Option<i64>,
) -> AppResult<Value> {
    if n.is_terminal() {
        return Err(AppError::conflict("already answered"));
    }
    if n.status == "cancelled" {
        return Err(AppError::conflict("cancelled"));
    }
    let custom = n
        .custom_actions()
        .into_iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(action_id));
    let listed = n.actions().iter().any(|a| a == action_id);
    let universal = matches!(action_id, "done" | "dismiss" | "reply" | "snooze");
    if custom.is_none() && !listed && !universal {
        return Err(AppError::bad_request(format!(
            "action {action_id:?} is not offered on this notification"
        )));
    }

    let kind = match action_id {
        "done" => "completed",
        "mute" => "muted",
        "reply" => "replied",
        "snooze" => "snoozed",
        "dismiss" => "dismissed",
        "reschedule" => "replied",
        _ => "acted",
    };
    let label = match (&custom, builtin(action_id)) {
        (Some(c), _) => c
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(action_id)
            .to_string(),
        (None, Some((_, l, _))) => l.to_string(),
        (None, None) => action_id.to_string(),
    };
    let text = text
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(20_000).collect::<String>());
    let at = now();
    let snooze_until = (kind == "snoozed").then(|| {
        now_plus(
            snooze_secs
                .unwrap_or_else(|| 60 * snooze_default_minutes(&n.profile))
                .clamp(1, 7 * 24 * 3600),
        )
    });
    let chosen = json!({
        "kind": kind,
        "action_id": action_id,
        "label": label,
        "text": text,
        "at": at,
        "snooze_until": snooze_until,
    });
    // An accepted or acknowledged event still gets its "starting soon" push.
    let keeps_reminder = kind != "snoozed"
        && !matches!(kind, "muted" | "dismissed")
        && !matches!(action_id, "no" | "reject" | "decline");
    let reminder = n.remind_at.clone().filter(|r| {
        keeps_reminder && crate::util::parse_ts(r).is_some_and(|t| t > chrono::Utc::now())
    });
    let next_fire_at = snooze_until.clone().or(reminder);
    let status = if kind == "snoozed" {
        "snoozed"
    } else {
        "responded"
    };
    let chosen_str = chosen.to_string();

    let mut tx = st.db.begin().await?;
    // Only from the state read above, so a simultaneous answer or cancel can't also land.
    let updated = sqlx::query!(
        "UPDATE notifications SET chosen_action = ?, responded_at = ?, status = ?, seen_at = COALESCE(seen_at, ?),
                next_fire_at = ?,
                escalation_state = CASE WHEN escalation_state IN ('pending', 'armed') THEN 'defused'
                                        ELSE escalation_state END
         WHERE id = ? AND status = ? AND responded_at IS ?",
        chosen_str,
        at,
        status,
        at,
        next_fire_at,
        n.id,
        n.status,
        n.responded_at
    )
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::conflict("already answered"));
    }
    if kind != "snoozed" && n.mirror_reply {
        let reference = json!({
            "correlation_id": n.correlation_id,
            "notification_id": n.id,
            "title": n.title,
            "action_id": action_id,
            "action_data": n.action_data.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        })
        .to_string();
        let reply_body = text.clone().unwrap_or_else(|| label.clone());
        let mid = sqlx::query_scalar!(
            r#"INSERT INTO messages (channel_id, kind, body, thread_id, correlation_id, ref, action_id, text, created_at)
               VALUES (?, 'notification_reply', ?, ?, ?, ?, ?, ?, ?) RETURNING id as "id!""#,
            n.channel_id,
            reply_body,
            n.thread_id,
            n.correlation_id,
            reference,
            action_id,
            text,
            at
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE notifications SET reply_message_id = ? WHERE id = ?",
            mid,
            n.id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    st.notify_event(n.id, EventKind::Answer);
    st.changed(n.channel_id);
    if next_fire_at.is_some() {
        st.hub.schedule.notify_one();
    }
    if kind != "snoozed" {
        crate::server::scheduler::release_queue(st, n.channel_id, &n.correlation_id).await?;
    }
    if kind != "snoozed" && n.mirror_reply {
        let _ = st.hub.messages.send(n.channel_id);
    }
    if let Some(updated) = load_by_id(&st.db, n.id).await? {
        webhook::dispatch(st, &updated);
    }
    Ok(chosen)
}

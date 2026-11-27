//! Endpoints only the PWA uses: passkeys, push subscriptions, the feed and receipts.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::{SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use webauthn_rs::prelude::*;

use crate::server::error::{AppError, AppResult};
use crate::server::owner::{self, Challenge, Human};
use crate::server::routes::human::is_biometric;
use crate::server::routes::notifications::{Notification, load_by_id};
use crate::server::{AppState, EventKind, OwnerView, channels, push};
use crate::util::now;

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok())
}

pub async fn session(State(st): State<AppState>, human: Option<Human>) -> AppResult<Json<Value>> {
    let enrolled = !owner::passkeys(&st.db).await?.is_empty();
    Ok(Json(json!({
        "authenticated": human.is_some(),
        "enrolled": enrolled,
        "passkeys_available": st.webauthn.is_some(),
        "vapid_public_key": st.vapid.public_key,
    })))
}

#[derive(Deserialize)]
pub struct EnrollStart {
    code: String,
}

pub async fn enroll_start(
    State(st): State<AppState>,
    Json(body): Json<EnrollStart>,
) -> AppResult<Json<Value>> {
    let wa = owner::webauthn_or_404(&st)?;
    let code_hash = owner::check_enrollment_code(&st.db, &body.code).await?;
    let existing: Vec<CredentialID> = owner::passkeys(&st.db)
        .await?
        .iter()
        .map(|(_, pk)| pk.cred_id().clone())
        .collect();
    let uuid = owner::owner_uuid(&st.db).await?;
    let (options, state) = wa
        .start_passkey_registration(uuid, "owner", "roost owner", Some(existing))
        .map_err(|e| AppError::Internal(e.into()))?;
    let challenge_id = owner::store_challenge(
        &st.db,
        Challenge {
            kind: "enroll",
            enrollment_code_hash: Some(code_hash),
            notification_id: None,
            action_id: None,
        },
        &state,
    )
    .await?;
    Ok(Json(
        json!({ "challenge_id": challenge_id, "options": options }),
    ))
}

#[derive(Deserialize)]
pub struct EnrollFinish {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
    label: Option<String>,
}

pub async fn enroll_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EnrollFinish>,
) -> AppResult<Response> {
    let wa = owner::webauthn_or_404(&st)?;
    let taken =
        owner::take_challenge::<PasskeyRegistration>(&st.db, &body.challenge_id, "enroll").await?;
    let at = now();
    let used = sqlx::query!(
        "UPDATE enrollment_codes SET used_at = ? WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?",
        at,
        taken.enrollment_code_hash,
        at
    )
    .execute(&st.db)
    .await?
    .rows_affected();
    if used != 1 {
        return Err(AppError::forbidden(
            "enrollment code is invalid, used or expired",
        ));
    }
    let pk = wa
        .finish_passkey_registration(&body.credential, &taken.state)
        .map_err(|e| AppError::forbidden(format!("passkey registration failed: {e}")))?;
    let cred_id = owner::cred_id_text(pk.cred_id());
    let json = serde_json::to_string(&pk).map_err(|e| AppError::Internal(e.into()))?;
    let label = body
        .label
        .filter(|l| !l.trim().is_empty())
        .or_else(|| user_agent(&headers).map(String::from))
        .unwrap_or_else(|| "passkey".into());
    let id = sqlx::query_scalar!(
        r#"INSERT INTO passkeys (cred_id, passkey, label, created_at) VALUES (?, ?, ?, ?) RETURNING id as "id!""#,
        cred_id,
        json,
        label,
        at
    )
    .fetch_one(&st.db)
    .await?;
    login_response(&st, id, &headers).await
}

pub async fn login_start(State(st): State<AppState>) -> AppResult<Json<Value>> {
    let wa = owner::webauthn_or_404(&st)?;
    let keys: Vec<Passkey> = owner::passkeys(&st.db)
        .await?
        .into_iter()
        .map(|(_, pk)| pk)
        .collect();
    if keys.is_empty() {
        return Err(AppError::not_found("no passkey enrolled yet"));
    }
    let (options, state) = wa
        .start_passkey_authentication(&keys)
        .map_err(|e| AppError::Internal(e.into()))?;
    let challenge_id = owner::store_challenge(
        &st.db,
        Challenge {
            kind: "login",
            enrollment_code_hash: None,
            notification_id: None,
            action_id: None,
        },
        &state,
    )
    .await?;
    Ok(Json(
        json!({ "challenge_id": challenge_id, "options": options }),
    ))
}

#[derive(Deserialize)]
pub struct Assertion {
    challenge_id: String,
    credential: PublicKeyCredential,
}

pub async fn login_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Assertion>,
) -> AppResult<Response> {
    let taken =
        owner::take_challenge::<PasskeyAuthentication>(&st.db, &body.challenge_id, "login").await?;
    let passkey_id = owner::verify_assertion(&st, &body.credential, &taken.state).await?;
    login_response(&st, passkey_id, &headers).await
}

async fn login_response(
    st: &AppState,
    passkey_id: i64,
    headers: &HeaderMap,
) -> AppResult<Response> {
    let token = owner::create_session(&st.db, passkey_id, user_agent(headers)).await?;
    let cookie = owner::session_cookie(st, &token, owner::SESSION_TTL_SECS);
    Ok((
        [(SET_COOKIE, cookie)],
        Json(json!({ "authenticated": true })),
    )
        .into_response())
}

pub async fn logout(State(st): State<AppState>, human: Human) -> AppResult<Response> {
    if let Human::Owner { token_hash } = &human {
        sqlx::query!("DELETE FROM sessions WHERE token_hash = ?", token_hash)
            .execute(&st.db)
            .await?;
    }
    let cookie = owner::session_cookie(&st, "", 0);
    Ok(([(SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

#[derive(Deserialize)]
pub struct SubscriptionKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
pub struct Subscription {
    endpoint: String,
    keys: SubscriptionKeys,
}

pub async fn subscribe(
    State(st): State<AppState>,
    _: Human,
    headers: HeaderMap,
    Json(body): Json<Subscription>,
) -> AppResult<StatusCode> {
    let endpoint = url::Url::parse(&body.endpoint)
        .map_err(|_| AppError::bad_request("endpoint must be a URL"))?;
    if endpoint.scheme() != "https" {
        return Err(AppError::bad_request("endpoint must be https"));
    }
    let at = now();
    let ua = user_agent(&headers);
    sqlx::query!(
        "INSERT INTO push_subscriptions (endpoint, p256dh, auth, user_agent, created_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth,
                user_agent = excluded.user_agent, failures = 0",
        body.endpoint,
        body.keys.p256dh,
        body.keys.auth,
        ua,
        at
    )
    .execute(&st.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Unsubscribe {
    endpoint: String,
}

pub async fn unsubscribe(
    State(st): State<AppState>,
    _: Human,
    Json(body): Json<Unsubscribe>,
) -> AppResult<StatusCode> {
    sqlx::query!(
        "DELETE FROM push_subscriptions WHERE endpoint = ?",
        body.endpoint
    )
    .execute(&st.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn push_test(State(st): State<AppState>, _: Human) -> AppResult<Json<Value>> {
    let body =
        json!({ "id": null, "title": "roost", "body": "Push works.", "url": "/" }).to_string();
    let accepted = push::broadcast(&st, body.as_bytes(), web_push::Urgency::Normal, None).await;
    Ok(Json(
        json!({ "accepted": accepted, "devices": push::device_count(&st.db).await? }),
    ))
}

async fn channel_names(st: &AppState) -> AppResult<std::collections::HashMap<i64, String>> {
    Ok(sqlx::query!(r#"SELECT id as "id!", name FROM channels"#)
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .map(|r| (r.id, r.name))
        .collect())
}

fn app_json(n: &Notification, channel: Option<&String>) -> Value {
    let mut v = n.poll_json();
    v["channel_id"] = n.channel_id.into();
    v["channel_name"] = channel.cloned().into();
    v["open"] = is_open(n).into();
    for spec in v["action_specs"].as_array_mut().into_iter().flatten() {
        let id = spec["id"].as_str().unwrap_or_default().to_string();
        spec["biometric"] = is_biometric(n, &id).into();
    }
    v
}

/// On the owner's desk: not answered, not cancelled, not queued, and a plain message stops counting once seen.
fn is_open(n: &Notification) -> bool {
    match n.status.as_str() {
        "cancelled" | "responded" | "queued" => false,
        _ if n.profile == "message" && n.action_specs().is_empty() => n.seen_at.is_none(),
        _ => true,
    }
}

/// Newest first; drops a plain notification once a newer one on its channel shares its collapse key.
fn current(rows: Vec<Notification>) -> Vec<Notification> {
    let mut newest = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|n| match &n.collapse_key {
            Some(k) if n.action_specs().is_empty() => newest.insert((n.channel_id, k.clone())),
            _ => true,
        })
        .collect()
}

async fn recent(st: &AppState, limit: i64) -> sqlx::Result<Vec<Notification>> {
    crate::select_notification!("status != 'cancelled' ORDER BY id DESC LIMIT ?", limit)
        .fetch_all(&st.db)
        .await
}

/// What the app icon badge shows: the items on Home.
pub async fn open_count(st: &AppState) -> sqlx::Result<usize> {
    Ok(current(recent(st, 500).await?)
        .iter()
        .filter(|n| is_open(n))
        .count())
}

pub async fn badge(State(st): State<AppState>, _: Human) -> AppResult<Json<Value>> {
    Ok(Json(json!({ "open": open_count(&st).await? })))
}

#[derive(Deserialize)]
pub struct FeedQuery {
    all: Option<bool>,
    limit: Option<i64>,
}

pub async fn feed(
    State(st): State<AppState>,
    _: Human,
    Query(q): Query<FeedQuery>,
) -> AppResult<Json<Value>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let rows = recent(&st, limit).await?;
    let names = channel_names(&st).await?;
    let list: Vec<Value> = if q.all.unwrap_or(false) {
        rows
    } else {
        current(rows).into_iter().filter(is_open).collect()
    }
    .iter()
    .map(|n| app_json(n, names.get(&n.channel_id)))
    .collect();
    Ok(Json(json!({ "notifications": list })))
}

async fn load(st: &AppState, id: i64) -> AppResult<Notification> {
    load_by_id(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such notification"))
}

pub async fn notification(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let n = load(&st, id).await?;
    let names = channel_names(&st).await?;
    Ok(Json(app_json(&n, names.get(&n.channel_id))))
}

pub async fn seen(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let at = now();
    let channel = sqlx::query_scalar!(
        "UPDATE notifications SET seen_at = ? WHERE id = ? AND seen_at IS NULL RETURNING channel_id",
        at,
        id
    )
    .fetch_optional(&st.db)
    .await?;
    if let Some(channel) = channel {
        st.notify_event(id, EventKind::Seen);
        st.changed(channel);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delivered(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let at = now();
    let channel = sqlx::query_scalar!(
        "UPDATE notifications SET delivered_at = ? WHERE id = ? AND delivered_at IS NULL RETURNING channel_id",
        at,
        id
    )
    .fetch_optional(&st.db)
    .await?;
    if let Some(channel) = channel {
        st.notify_event(id, EventKind::Delivered);
        st.changed(channel);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct GateStart {
    action_id: String,
}

pub async fn gate(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
    Json(body): Json<GateStart>,
) -> AppResult<Json<Value>> {
    let wa = owner::webauthn_or_404(&st)?;
    let n = load(&st, id).await?;
    if !is_biometric(&n, &body.action_id) {
        return Err(AppError::bad_request("this action needs no Face ID"));
    }
    let keys: Vec<Passkey> = owner::passkeys(&st.db)
        .await?
        .into_iter()
        .map(|(_, pk)| pk)
        .collect();
    let (options, state) = wa
        .start_passkey_authentication(&keys)
        .map_err(|e| AppError::Internal(e.into()))?;
    let challenge_id = owner::store_challenge(
        &st.db,
        Challenge {
            kind: "gate",
            enrollment_code_hash: None,
            notification_id: Some(n.id),
            action_id: Some(body.action_id),
        },
        &state,
    )
    .await?;
    Ok(Json(
        json!({ "challenge_id": challenge_id, "options": options }),
    ))
}

pub async fn channels_list(State(st): State<AppState>, _: Human) -> AppResult<Json<Value>> {
    let ids = sqlx::query_scalar!(r#"SELECT id as "id!" FROM channels ORDER BY id"#)
        .fetch_all(&st.db)
        .await?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(ch) = channels::load(&st.db, id).await? {
            let mut v = ch.brief();
            v["claim"] = ch.claim();
            v["listening"] = st.presence.listening(id).into();
            v["last_seen_at"] = st.presence.last_seen_at(id).into();
            v["typing_until"] = st.typing_until(id).into();
            v["last"] = sqlx::query!(
                "SELECT title, created_at FROM notifications WHERE channel_id = ? AND status != 'cancelled'
                 ORDER BY id DESC LIMIT 1",
                id
            )
            .fetch_optional(&st.db)
            .await?
            .map(|r| json!({ "title": r.title, "created_at": r.created_at }))
            .unwrap_or(Value::Null);
            out.push(v);
        }
    }
    Ok(Json(json!({ "channels": out })))
}

pub async fn channel_thread(
    State(st): State<AppState>,
    _: Human,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let ch = channels::load(&st.db, id)
        .await?
        .ok_or_else(|| AppError::not_found("no such channel"))?;
    let notifications: Vec<Value> = crate::select_notification!(
        "channel_id = ? AND status != 'cancelled' ORDER BY id DESC LIMIT 100",
        id
    )
    .fetch_all(&st.db)
    .await?
    .iter()
    .rev()
    .map(|n| app_json(n, Some(&ch.name)))
    .collect();
    let mut channel = ch.full();
    channel["claim"] = ch.claim();
    channel["listening"] = st.presence.listening(id).into();
    channel["last_seen_at"] = st.presence.last_seen_at(id).into();
    channel["typing_until"] = st.typing_until(id).into();
    let rows = sqlx::query!(
        r#"SELECT id as "id!", kind, body, text, correlation_id, created_at, delivered_at, processed_at,
                  acked_by_label, handler_summary
           FROM messages WHERE channel_id = ? ORDER BY id DESC LIMIT 100"#,
        id
    )
    .fetch_all(&st.db)
    .await?;
    let list: Vec<Value> = rows
        .into_iter()
        .rev()
        .map(|m| {
            json!({
                "id": m.id, "kind": m.kind, "body": m.body, "text": m.text,
                "correlation_id": m.correlation_id, "created_at": m.created_at,
                "delivered_at": m.delivered_at, "processed_at": m.processed_at,
                "acked_by_label": m.acked_by_label, "handler_summary": m.handler_summary,
            })
        })
        .collect();
    Ok(Json(
        json!({ "channel": channel, "notifications": notifications, "messages": list }),
    ))
}

/// Server-sent "channel N changed" signals; the PWA re-fetches whatever it shows.
pub async fn events(
    State(st): State<AppState>,
    _: Human,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = st.hub.changes.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let data = match rx.recv().await {
            Ok(channel_id) => json!({ "channel_id": channel_id }),
            // Missed some: tell the client to refresh everything.
            Err(broadcast::error::RecvError::Lagged(_)) => json!({ "channel_id": null }),
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((
            Ok(Event::default().event("changed").data(data.to_string())),
            rx,
        ))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

#[derive(Deserialize)]
pub struct Viewing {
    channel_id: Option<i64>,
    #[serde(default)]
    home: bool,
}

/// The PWA reports which channel is on screen, or Home, so pushes it already shows can be skipped.
pub async fn viewing(
    State(st): State<AppState>,
    _: Human,
    Json(body): Json<Viewing>,
) -> StatusCode {
    let view = match body.channel_id {
        Some(c) => Some(OwnerView::Channel(c)),
        None if body.home => Some(OwnerView::Home),
        None => None,
    };
    *st.owner_view.lock().unwrap() = view.map(|v| (v, std::time::Instant::now()));
    StatusCode::NO_CONTENT
}

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::time::{Instant, sleep_until};

use crate::server::AppState;
use crate::server::auth::{ChannelAuth, Identity};
use crate::server::error::{AppError, AppResult};
use crate::server::routes::notifications::LONG_POLL_MAX_SECS;
use crate::util::{now, now_plus, parse_ts};

const DEFAULT_LEASE_SECS: i64 = 600;

struct Message {
    id: i64,
    channel_id: i64,
    kind: String,
    body: String,
    thread_id: Option<String>,
    correlation_id: Option<String>,
    reference: Option<String>,
    action_id: Option<String>,
    text: Option<String>,
    created_at: String,
    delivered_at: Option<String>,
    delivery_expires_at: Option<String>,
    lease_fingerprint: Option<String>,
    lease_label: Option<String>,
    processed_at: Option<String>,
    acked_by_label: Option<String>,
    handler_summary: Option<String>,
}

impl Message {
    fn lease_live(&self) -> bool {
        self.processed_at.is_none()
            && self
                .delivery_expires_at
                .as_deref()
                .and_then(parse_ts)
                .is_some_and(|t| t > chrono::Utc::now())
    }

    fn json(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "channel_id": self.channel_id,
            "kind": self.kind,
            "body": self.body,
            "thread_id": self.thread_id,
            "created_at": self.created_at,
            "delivered_at": self.delivered_at,
            "processed_at": self.processed_at,
            "delivery_expires_at": self.delivery_expires_at,
            "consumed_at": self.processed_at,
            "has_events": false,
        });
        if self.kind == "system" {
            v["ref"] = self
                .reference
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or(Value::Null);
        }
        if self.kind == "notification_reply" {
            v["correlation_id"] = self.correlation_id.clone().into();
            v["action_id"] = self.action_id.clone().into();
            v["text"] = self.text.clone().into();
            v["ref"] = self
                .reference
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or(Value::Null);
        }
        if self.lease_live() {
            v["being_handled_by"] =
                json!({ "label": self.lease_label, "fingerprint": self.lease_fingerprint });
        }
        if self.processed_at.is_some() {
            v["acked_by_label"] = self.acked_by_label.clone().into();
            v["handler_summary"] = self.handler_summary.clone().into();
            v["handled_state"] = if self.handler_summary.is_some() {
                "handled"
            } else {
                "drained"
            }
            .into();
        }
        v
    }
}

macro_rules! select_messages {
    ($where:literal, $($arg:expr),*) => {
        sqlx::query_as!(
            Message,
            r#"SELECT id as "id!", channel_id, kind, body, thread_id, correlation_id, ref as reference, action_id, text,
                      created_at, delivered_at, delivery_expires_at, lease_fingerprint, lease_label, processed_at,
                      acked_by_label, handler_summary
               FROM messages WHERE "# + $where,
            $($arg),*
        )
    };
}

pub async fn has_pending_composer(db: &SqlitePool, channel_id: i64) -> sqlx::Result<bool> {
    let at = now();
    let found = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM messages WHERE channel_id = ? AND kind = 'message' AND processed_at IS NULL
                  AND (delivery_expires_at IS NULL OR delivery_expires_at <= ?)) as "found!: bool""#,
        channel_id,
        at
    )
    .fetch_one(db)
    .await?;
    Ok(found)
}

#[derive(Deserialize)]
pub struct ConsumeQuery {
    wait: Option<u64>,
    all: Option<bool>,
    lease: Option<i64>,
    since: Option<i64>,
    history: Option<bool>,
    before: Option<i64>,
    continuity: Option<bool>,
}

async fn deliverable(
    db: &SqlitePool,
    channel_id: i64,
    all: bool,
    since: i64,
) -> sqlx::Result<Vec<Message>> {
    let at = now();
    select_messages!(
        "channel_id = ? AND processed_at IS NULL AND (? = 1 OR kind = 'message') AND id > ?
         AND (delivery_expires_at IS NULL OR delivery_expires_at <= ?) ORDER BY id ASC LIMIT 100",
        channel_id,
        all,
        since,
        at
    )
    .fetch_all(db)
    .await
}

pub async fn consume(
    State(st): State<AppState>,
    auth: ChannelAuth,
    ident: Identity,
    Query(q): Query<ConsumeQuery>,
) -> AppResult<Json<Value>> {
    let all = q.all.unwrap_or(false);

    if q.history.unwrap_or(false) {
        let before = q.before.unwrap_or(i64::MAX);
        let rows = select_messages!(
            "channel_id = ? AND (? = 1 OR kind = 'message') AND id < ? ORDER BY id DESC LIMIT 200",
            auth.channel_id,
            all,
            before
        )
        .fetch_all(&st.db)
        .await?;
        let list: Vec<Value> = rows.iter().map(Message::json).collect();
        return Ok(Json(
            json!({ "messages": list, "stale_from_prior_claim": false }),
        ));
    }

    let wait = q.wait.unwrap_or(0).min(LONG_POLL_MAX_SECS);
    let deadline = Instant::now() + Duration::from_secs(wait);
    st.touch_consumer(
        auth.channel_id,
        &ident.who(),
        if wait > 0 { "longpoll" } else { "poll" },
    );
    let _hold = (wait > 0).then(|| st.hold_consumer(auth.channel_id, &ident.who(), "longpoll"));
    let mut rx = st.hub.messages.subscribe();
    let since = q.since.unwrap_or(0);
    let mut rows = deliverable(&st.db, auth.channel_id, all, since).await?;
    while rows.is_empty() && Instant::now() < deadline {
        tokio::select! {
            _ = sleep_until(deadline) => break,
            Ok(ch) = rx.recv() => { if ch != auth.channel_id { continue; } }
        }
        rows = deliverable(&st.db, auth.channel_id, all, since).await?;
    }

    let lease = q.lease.unwrap_or(DEFAULT_LEASE_SECS).clamp(60, 3600);
    let at = now();
    let expires = now_plus(lease);
    for m in &mut rows {
        sqlx::query!(
            "UPDATE messages SET delivered_at = COALESCE(delivered_at, ?), delivery_expires_at = ?,
                    lease_fingerprint = ?, lease_label = ? WHERE id = ?",
            at,
            expires,
            ident.fingerprint,
            ident.label,
            m.id
        )
        .execute(&st.db)
        .await?;
        m.delivered_at.get_or_insert_with(|| at.clone());
        m.delivery_expires_at = Some(expires.clone());
        m.lease_fingerprint = ident.fingerprint.clone();
        m.lease_label = ident.label.clone();
    }
    let reply_ids: Vec<&str> = rows
        .iter()
        .filter_map(|m| m.correlation_id.as_deref())
        .collect();
    for cid in reply_ids {
        sqlx::query!(
            "UPDATE notifications SET reply_delivered_at = COALESCE(reply_delivered_at, ?)
             WHERE channel_id = ? AND correlation_id = ?",
            at,
            auth.channel_id,
            cid
        )
        .execute(&st.db)
        .await?;
    }

    if !rows.is_empty() {
        st.changed(auth.channel_id);
    }
    let list: Vec<Value> = rows.iter().map(Message::json).collect();
    let mut body = json!({ "messages": list, "stale_from_prior_claim": false });
    if q.continuity.unwrap_or(false) {
        body["continuity_contexts"] = json!([]);
    }
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct AckBody {
    up_to: Option<i64>,
    ids: Option<Vec<i64>>,
    state: Option<String>,
    summary: Option<String>,
}

pub async fn ack(
    State(st): State<AppState>,
    auth: ChannelAuth,
    ident: Identity,
    Json(body): Json<AckBody>,
) -> AppResult<Json<Value>> {
    let renew = match body.state.as_deref() {
        None | Some("processed") => false,
        Some("delivered") => true,
        Some(s) => {
            return Err(AppError::contract(
                "invalid_state",
                format!("state must be processed or delivered, got {s:?}"),
            ));
        }
    };
    let explicit = body.ids.is_some();
    let candidates = match (&body.ids, body.up_to) {
        (Some(ids), _) => {
            let mut rows = Vec::new();
            for id in ids {
                if let Some(m) = select_messages!("channel_id = ? AND id = ?", auth.channel_id, id)
                    .fetch_optional(&st.db)
                    .await?
                {
                    rows.push(m);
                }
            }
            rows
        }
        (None, Some(up_to)) => {
            select_messages!(
                "channel_id = ? AND id <= ? ORDER BY id",
                auth.channel_id,
                up_to
            )
            .fetch_all(&st.db)
            .await?
        }
        (None, None) => return Err(AppError::bad_request("ack needs up_to or ids")),
    };

    let at = now();
    let summary = body
        .summary
        .as_deref()
        .map(|s| s.chars().take(1000).collect::<String>());
    let label = ident.attribution();
    let (mut done, mut skipped) = (0i64, 0i64);
    for m in candidates.iter().filter(|m| m.processed_at.is_none()) {
        let foreign_lease = m.lease_live()
            && m.lease_fingerprint.is_some()
            && m.lease_fingerprint != ident.fingerprint;
        let never_served = m.delivered_at.is_none();
        if foreign_lease || (never_served && (renew || !explicit)) {
            skipped += 1;
            continue;
        }
        if renew {
            let expires = now_plus(DEFAULT_LEASE_SECS);
            sqlx::query!(
                "UPDATE messages SET delivery_expires_at = ? WHERE id = ?",
                expires,
                m.id
            )
            .execute(&st.db)
            .await?;
        } else {
            sqlx::query!(
                "UPDATE messages SET processed_at = ?, delivered_at = COALESCE(delivered_at, ?), acked_by_label = ?,
                        handler_summary = ? WHERE id = ?",
                at,
                at,
                label,
                summary,
                m.id
            )
            .execute(&st.db)
            .await?;
        }
        done += 1;
    }

    if done > 0 && !renew {
        st.changed(auth.channel_id);
    }
    let mut out = if renew {
        json!({ "renewed": done })
    } else {
        json!({ "acked": done })
    };
    if skipped > 0 {
        out["skipped"] = skipped.into();
    }
    Ok(Json(out))
}

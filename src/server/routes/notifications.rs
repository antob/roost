use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::time::{Instant, sleep_until};

use crate::server::AppState;
use crate::server::auth::ChannelAuth;
use crate::server::error::{AppError, AppResult};
use crate::server::routes::messages::has_pending_composer;
use crate::util::now;

pub const LONG_POLL_MAX_SECS: u64 = 25;

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: i64,
    pub channel_id: i64,
    pub correlation_id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    pub body_markdown: Option<String>,
    pub template_kind: String,
    pub profile: String,
    pub urgency: String,
    pub actions: String,
    pub custom_actions: String,
    pub url: Option<String>,
    pub copy: Option<String>,
    pub thread_id: Option<String>,
    pub collapse_key: Option<String>,
    pub action_data: Option<String>,
    pub reply_to: Option<String>,
    pub mirror_reply: bool,
    pub sent_note: Option<String>,
    pub sent_by_label: Option<String>,
    pub event_at: Option<String>,
    pub deliver_at: Option<String>,
    pub status: String,
    pub delivered_at: Option<String>,
    pub seen_at: Option<String>,
    pub responded_at: Option<String>,
    pub chosen_action: Option<String>,
    pub reply_delivered_at: Option<String>,
    pub reply_message_id: Option<i64>,
    pub created_at: String,
    pub next_fire_at: Option<String>,
    pub fired_at: Option<String>,
    pub escalation_state: Option<String>,
    pub escalation_fires_at: Option<String>,
    pub escalate_after_secs: Option<i64>,
    pub after_cid: Option<String>,
    pub remind_at: Option<String>,
}

#[macro_export]
macro_rules! select_notification {
    ($where:literal, $($arg:expr),*) => {
        sqlx::query_as!(
            Notification,
            r#"SELECT id as "id!", channel_id, correlation_id, title, subtitle, body, body_markdown, template_kind,
                      profile, urgency, actions, custom_actions, url, copy, thread_id, collapse_key, action_data,
                      reply_to, mirror_reply as "mirror_reply: bool", sent_note, sent_by_label, event_at, deliver_at,
                      status, delivered_at, seen_at, responded_at, chosen_action, reply_delivered_at, reply_message_id,
                      created_at, next_fire_at, fired_at, escalation_state, escalation_fires_at, escalate_after_secs,
                      after_cid, remind_at
               FROM notifications WHERE "# + $where,
            $($arg),*
        )
    };
}

pub async fn load_by_cid(
    db: &SqlitePool,
    channel_id: i64,
    cid: &str,
) -> sqlx::Result<Option<Notification>> {
    select_notification!("channel_id = ? AND correlation_id = ?", channel_id, cid)
        .fetch_optional(db)
        .await
}

pub async fn load_by_id(db: &SqlitePool, id: i64) -> sqlx::Result<Option<Notification>> {
    select_notification!("id = ?", id).fetch_optional(db).await
}

fn parse_json(raw: &Option<String>) -> Value {
    raw.as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}

impl Notification {
    pub fn actions(&self) -> Vec<String> {
        serde_json::from_str(&self.actions).unwrap_or_default()
    }

    pub fn custom_actions(&self) -> Vec<Value> {
        serde_json::from_str(&self.custom_actions).unwrap_or_default()
    }

    pub fn chosen(&self) -> Value {
        parse_json(&self.chosen_action)
    }

    /// Settled for a decision queue: answered for good, or cancelled.
    pub fn is_settled(&self) -> bool {
        self.status == "cancelled" || self.is_terminal()
    }

    /// Terminal = answered with anything but a snooze.
    pub fn is_terminal(&self) -> bool {
        self.chosen()
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k != "snoozed")
    }

    pub fn action_specs(&self) -> Vec<Value> {
        let mut specs: Vec<Value> = self
            .actions()
            .iter()
            .filter_map(|id| builtin(id))
            .map(|(id, label, terminal)| json!({ "id": id, "label": label, "terminal": terminal, "custom": false }))
            .collect();
        for c in self.custom_actions() {
            let mut c = c.clone();
            c["custom"] = true.into();
            if c.get("terminal").is_none() {
                c["terminal"] = true.into();
            }
            specs.push(c);
        }
        specs
    }

    pub fn requires_action(&self) -> bool {
        let actions = self.actions();
        !self.custom_actions().is_empty() || actions.iter().any(|a| a != "done")
    }

    pub fn render_mode(&self) -> &'static str {
        // iOS web push carries no buttons: anything actionable is answered on the detail screen.
        if self.action_specs().is_empty() {
            "plain"
        } else {
            "detail_only"
        }
    }

    pub fn escalation(&self) -> Value {
        match &self.escalation_state {
            None => Value::Null,
            Some(state) => json!({
                "state": state,
                "after_minutes": self.escalate_after_secs.map(|s| (s + 59) / 60),
                "fires_at": self.escalation_fires_at,
            }),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "correlation_id": self.correlation_id,
            "status": self.status,
            "title": self.title,
            "subtitle": self.subtitle,
            "body": self.body,
            "body_markdown": self.body_markdown,
            "template_kind": self.template_kind,
            "profile": self.profile,
            "urgency": self.urgency,
            "actions": self.actions(),
            "action_specs": self.action_specs(),
            "render_mode": self.render_mode(),
            "banner_actions": false,
            "requires_action": self.requires_action(),
            "url": self.url,
            "copy": self.copy,
            "thread_id": self.thread_id,
            "collapse_key": self.collapse_key,
            "action_data": parse_json(&self.action_data),
            "sent_note": self.sent_note,
            "sent_by_label": self.sent_by_label,
            "event_at": self.event_at,
            "remind_at": self.remind_at,
            "after": self.after_cid,
            "deliver_at": self.deliver_at,
            "delivered_at": self.delivered_at,
            "seen_at": self.seen_at,
            "escalation": self.escalation(),
            "created_at": self.created_at,
        })
    }

    pub fn poll_json(&self) -> Value {
        let mut v = self.json();
        let chosen = self.chosen();
        v["responded"] = (!chosen.is_null()).into();
        if self.deliver_at.is_some() && self.fired_at.is_none() && self.is_terminal() {
            v["early_resolution"] = true.into();
        }
        v["chosen_action"] = chosen;
        v["reply_receipt"] = match self.reply_message_id {
            Some(mid) if self.responded_at.is_some() => {
                json!({ "delivered_at": self.reply_delivered_at, "processed_at": null, "message_id": mid })
            }
            _ => Value::Null,
        };
        v
    }
}

/// (id, label, terminal) for the built-in action catalog.
pub fn builtin(id: &str) -> Option<(&'static str, &'static str, bool)> {
    const CATALOG: &[(&str, &str, bool)] = &[
        ("snooze", "Snooze", false),
        ("done", "Done ✓", true),
        ("reschedule", "Reschedule", false),
        ("mute", "Mute", true),
        ("reply", "Reply", true),
        ("yes", "Yes", true),
        ("no", "No", true),
        ("approve", "Approve", true),
        ("reject", "Reject", true),
        ("accept", "Accept", true),
        ("decline", "Decline", true),
        ("later", "Later", true),
    ];
    CATALOG.iter().copied().find(|(cid, _, _)| *cid == id)
}

#[derive(Deserialize)]
pub struct PollQuery {
    wait: Option<u64>,
    wake_on_message: Option<bool>,
}

pub async fn show(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Path(cid): Path<String>,
    Query(q): Query<PollQuery>,
) -> AppResult<Json<Value>> {
    let wake = q.wake_on_message.unwrap_or(false);
    let deadline =
        Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(LONG_POLL_MAX_SECS));
    let mut notif_rx = st.hub.notifications.subscribe();
    let mut msg_rx = st.hub.messages.subscribe();
    let not_found = || AppError::not_found(format!("no notification with correlation_id {cid}"));

    let first = load_by_cid(&st.db, auth.channel_id, &cid)
        .await?
        .ok_or_else(not_found)?;
    let started_at = first.responded_at.clone();
    let mut current = first;
    let pending = loop {
        // A snoozed row holds for the NEXT event rather than re-returning the old snooze.
        let answered = current.is_settled() || current.responded_at != started_at;
        let pending = wake && has_pending_composer(&st.db, auth.channel_id).await?;
        if answered || pending || Instant::now() >= deadline {
            break pending;
        }
        tokio::select! {
            _ = sleep_until(deadline) => {}
            Ok(ev) = notif_rx.recv() => { if ev.id != current.id { continue; } }
            Ok(ch) = msg_rx.recv(), if wake => { if ch != auth.channel_id { continue; } }
        }
        current = load_by_id(&st.db, current.id)
            .await?
            .ok_or_else(not_found)?;
    };

    if current.responded_at.is_some() && current.reply_delivered_at.is_none() {
        let at = now();
        sqlx::query!(
            "UPDATE notifications SET reply_delivered_at = ? WHERE id = ?",
            at,
            current.id
        )
        .execute(&st.db)
        .await?;
        current.reply_delivered_at = Some(at);
    }
    let mut body = current.poll_json();
    if pending {
        body["messages_pending"] = true.into();
    }
    Ok(Json(body))
}

pub async fn cancel(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Path(cid): Path<String>,
) -> AppResult<Json<Value>> {
    let n = load_by_cid(&st.db, auth.channel_id, &cid)
        .await?
        .ok_or_else(|| AppError::not_found(format!("no notification with correlation_id {cid}")))?;
    match n.status.as_str() {
        "cancelled" => {}
        "scheduled" | "snoozed" | "queued" => {
            sqlx::query!(
                "UPDATE notifications SET status = 'cancelled', next_fire_at = NULL WHERE id = ?",
                n.id
            )
            .execute(&st.db)
            .await?;
            st.notify_event(n.id, crate::server::EventKind::Answer);
            st.changed(n.channel_id);
            crate::server::scheduler::release_queue(&st, n.channel_id, &n.correlation_id).await?;
        }
        _ => {
            return Err(AppError::conflict(
                "too late to cancel: the notification was already delivered or answered",
            ));
        }
    }
    Ok(Json(json!({ "cancelled": true, "correlation_id": cid })))
}

#[derive(Deserialize)]
pub struct IndexQuery {
    pending: Option<bool>,
    limit: Option<i64>,
}

pub async fn index(
    State(st): State<AppState>,
    auth: ChannelAuth,
    Query(q): Query<IndexQuery>,
) -> AppResult<Json<Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let pending = q.pending.unwrap_or(false);
    let rows = select_notification!(
        "channel_id = ? AND (? = 0 OR (delivered_at IS NOT NULL AND responded_at IS NULL)) ORDER BY id DESC LIMIT ?",
        auth.channel_id,
        pending,
        limit
    )
    .fetch_all(&st.db)
    .await?;
    let list: Vec<Value> = rows.iter().map(Notification::poll_json).collect();
    Ok(Json(json!({ "notifications": list })))
}

pub async fn summary(State(st): State<AppState>, auth: ChannelAuth) -> AppResult<Json<Value>> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) as "total!: i64",
                  COALESCE(SUM(responded_at IS NOT NULL), 0) as "answered!: i64",
                  COALESCE(SUM(responded_at IS NULL AND status NOT IN ('cancelled')), 0) as "open!: i64"
           FROM notifications WHERE channel_id = ?"#,
        auth.channel_id
    )
    .fetch_one(&st.db)
    .await?;
    Ok(Json(
        json!({ "total": row.total, "answered": row.answered, "open": row.open }),
    ))
}

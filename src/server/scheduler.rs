//! Fires time-based transitions (deliver_at, snooze re-fire, the urgent escalation fuse, event
//! reminders) and releases decision queues.

use std::time::Duration;

use chrono::Utc;

use crate::server::AppState;
use crate::server::push::{self, Push};
use crate::server::routes::Notification;
use crate::util::{now, now_plus, parse_ts};

const MAX_IDLE: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(10);

pub fn spawn(st: AppState) {
    tokio::spawn(async move {
        loop {
            let failed = match tick(&st).await {
                Ok(failed) => failed,
                Err(e) => {
                    tracing::warn!("scheduler tick failed: {e:#}");
                    true
                }
            };
            let mut wait = match next_due(&st).await {
                Ok(Some(at)) => (at - Utc::now()).to_std().unwrap_or(Duration::ZERO),
                Ok(None) => MAX_IDLE,
                Err(e) => {
                    tracing::warn!("scheduler lookup failed: {e:#}");
                    MAX_IDLE
                }
            };
            if failed {
                // A row that failed is still due; don't retry it in a tight loop.
                wait = wait.max(RETRY_AFTER);
            }
            tokio::select! {
                _ = tokio::time::sleep(wait.min(MAX_IDLE)) => {}
                _ = st.hub.schedule.notified() => {}
            }
        }
    });
}

async fn next_due(st: &AppState) -> sqlx::Result<Option<chrono::DateTime<Utc>>> {
    let at = sqlx::query_scalar!(
        r#"SELECT MIN(next_fire_at) as "at: String" FROM notifications WHERE next_fire_at IS NOT NULL"#
    )
    .fetch_one(&st.db)
    .await?;
    Ok(at.as_deref().and_then(parse_ts))
}

/// Fires every due row, skipping past ones that fail. True if any failed.
async fn tick(st: &AppState) -> anyhow::Result<bool> {
    let at = now();
    let due = crate::select_notification!(
        "next_fire_at IS NOT NULL AND next_fire_at <= ? ORDER BY next_fire_at",
        at
    )
    .fetch_all(&st.db)
    .await?;
    let mut failed = false;
    for n in due {
        let id = n.id;
        if let Err(e) = fire(st, n).await {
            tracing::warn!(notification = id, "scheduler fire failed: {e:#}");
            failed = true;
        }
    }
    Ok(failed)
}

async fn fire(st: &AppState, n: Notification) -> anyhow::Result<()> {
    // Each transition first claims the row on its old next_fire_at, so a concurrent answer or cancel wins.
    match (n.status.as_str(), n.escalation_state.as_deref()) {
        ("pending" | "responded", _) if n.remind_at.is_some() && n.next_fire_at == n.remind_at => {
            let claimed = sqlx::query!(
                "UPDATE notifications SET next_fire_at = NULL WHERE id = ? AND next_fire_at = ?",
                n.id,
                n.next_fire_at
            )
            .execute(&st.db)
            .await?
            .rows_affected();
            if claimed == 1 {
                push::send(st, &n, Push::Reminder).await;
            }
        }
        (status @ ("scheduled" | "snoozed"), _) => {
            let claimed = sqlx::query!(
                "UPDATE notifications SET status = 'pending', next_fire_at = NULL
                 WHERE id = ? AND status = ? AND next_fire_at = ?",
                n.id,
                status,
                n.next_fire_at
            )
            .execute(&st.db)
            .await?
            .rows_affected();
            if claimed == 1 {
                let kind = if status == "scheduled" {
                    Push::Initial
                } else {
                    Push::Refire
                };
                deliver(st, &n, kind).await?;
            }
        }
        ("pending", Some("armed")) => {
            let claimed = sqlx::query!(
                "UPDATE notifications SET escalation_state = 'fired_not_pushed', next_fire_at = NULL
                 WHERE id = ? AND status = 'pending' AND escalation_state = 'armed' AND next_fire_at = ?",
                n.id,
                n.next_fire_at
            )
            .execute(&st.db)
            .await?
            .rows_affected();
            if claimed == 1 {
                st.changed(n.channel_id);
            }
            if claimed == 1 && push::send(st, &n, Push::Escalation).await > 0 {
                sqlx::query!(
                    "UPDATE notifications SET escalation_state = 'fired' WHERE id = ?",
                    n.id
                )
                .execute(&st.db)
                .await?;
            }
        }
        _ => {
            sqlx::query!(
                "UPDATE notifications SET next_fire_at = NULL WHERE id = ? AND next_fire_at = ?",
                n.id,
                n.next_fire_at
            )
            .execute(&st.db)
            .await?;
        }
    }
    Ok(())
}

/// Sends a notification that just became `pending` and arms its escalation fuse.
pub async fn deliver(st: &AppState, n: &Notification, kind: Push) -> sqlx::Result<()> {
    let at = now();
    let arm = matches!(
        n.escalation_state.as_deref(),
        Some("pending" | "armed" | "defused" | "fired" | "fired_not_pushed")
    );
    let fires_at = arm.then(|| now_plus(n.escalate_after_secs.unwrap_or(300)));
    let next_fire_at = fires_at.clone().or_else(|| {
        n.remind_at
            .clone()
            .filter(|r| parse_ts(r).is_some_and(|t| t > Utc::now()))
    });
    sqlx::query!(
        "UPDATE notifications SET fired_at = ?,
                escalation_state = CASE WHEN ? THEN 'armed' ELSE escalation_state END,
                escalation_fires_at = COALESCE(?, escalation_fires_at),
                next_fire_at = ?
         WHERE id = ?",
        at,
        arm,
        fires_at,
        next_fire_at,
        n.id
    )
    .execute(&st.db)
    .await?;
    // The owner already has this channel open and sees it arrive live: a banner on top would be noise.
    let in_app = st.owner_viewing(n.channel_id);
    if in_app || push::send(st, n, kind).await > 0 {
        sqlx::query!(
            "UPDATE notifications SET delivered_at = COALESCE(delivered_at, ?) WHERE id = ?",
            at,
            n.id
        )
        .execute(&st.db)
        .await?;
        if in_app {
            st.notify_event(n.id, crate::server::EventKind::Delivered);
        }
    }
    if next_fire_at.is_some() {
        st.hub.schedule.notify_one();
    }
    st.changed(n.channel_id);
    Ok(())
}

/// Lets go of the notifications queued (`after`) behind `cid`, now that it is answered or cancelled.
pub async fn release_queue(st: &AppState, channel_id: i64, cid: &str) -> sqlx::Result<()> {
    let queued = crate::select_notification!(
        "channel_id = ? AND after_cid = ? AND status = 'queued' ORDER BY id",
        channel_id,
        cid
    )
    .fetch_all(&st.db)
    .await?;
    for n in queued {
        let later = n
            .deliver_at
            .as_deref()
            .and_then(parse_ts)
            .filter(|t| *t > Utc::now())
            .map(|_| n.deliver_at.clone());
        let (status, next_fire_at) = match later {
            Some(at) => ("scheduled", at),
            None => ("pending", None),
        };
        let claimed = sqlx::query!(
            "UPDATE notifications SET status = ?, next_fire_at = ? WHERE id = ? AND status = 'queued'",
            status,
            next_fire_at,
            n.id
        )
        .execute(&st.db)
        .await?
        .rows_affected();
        if claimed != 1 {
            continue;
        }
        if status == "pending" {
            deliver(st, &n, Push::Initial).await?;
        } else {
            st.hub.schedule.notify_one();
            st.changed(channel_id);
        }
        st.notify_event(n.id, crate::server::EventKind::Delivered);
    }
    Ok(())
}

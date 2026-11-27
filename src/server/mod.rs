pub mod auth;
pub mod channels;
pub mod error;
pub mod owner;
pub mod presence;
pub mod push;
pub mod routes;
pub mod scheduler;
pub mod web;
pub mod webhook;

use crate::util;

use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tokio::sync::broadcast;

/// Bounds every outbound push and webhook, so a hung endpoint can't stall sends or the scheduler.
pub const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub base_url: String,
    pub admin_token: Option<String>,
    pub callback_secret: Option<String>,
    pub timezone: String,
    /// PEM of the VAPID private key; generated and stored in the database when absent.
    pub vapid_private_pem: Option<String>,
    /// The VAPID `sub` claim (a mailto: or https: URL).
    pub vapid_subject: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// Answered, snoozed or cancelled.
    Answer,
    /// The service worker received the push.
    Delivered,
    /// The owner opened it.
    Seen,
}

#[derive(Clone, Copy, Debug)]
pub struct NotifEvent {
    pub id: i64,
    pub kind: EventKind,
}

pub struct Hub {
    pub notifications: broadcast::Sender<NotifEvent>,
    /// Channel ids that got a new queue row.
    pub messages: broadcast::Sender<i64>,
    /// Channel ids whose PWA-visible state changed (drives `/api/human/events`).
    pub changes: broadcast::Sender<i64>,
    /// Poked when a notification's next_fire_at moved earlier.
    pub schedule: tokio::sync::Notify,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            notifications: broadcast::channel(256).0,
            messages: broadcast::channel(256).0,
            changes: broadcast::channel(256).0,
            schedule: tokio::sync::Notify::new(),
        }
    }
}

pub struct Inner {
    pub db: SqlitePool,
    pub config: Config,
    pub hub: Hub,
    pub http: reqwest::Client,
    pub webauthn: Option<webauthn_rs::Webauthn>,
    pub vapid: push::Vapid,
    pub presence: presence::Presence,
    /// The channel whose conversation or item the owner has open in the PWA, and when that was last confirmed.
    pub owner_view: std::sync::Mutex<Option<(i64, std::time::Instant)>>,
    /// Until when each channel's agent shows the typing dots.
    pub typing: std::sync::Mutex<std::collections::HashMap<i64, chrono::DateTime<chrono::Utc>>>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub async fn new(db: SqlitePool, config: Config) -> anyhow::Result<Self> {
        let webauthn = owner::webauthn(&config.base_url)
            .inspect_err(|e| tracing::warn!("passkeys disabled: {e:#}"))
            .ok();
        let subject = config
            .vapid_subject
            .clone()
            .unwrap_or_else(|| config.base_url.clone());
        let vapid = push::Vapid::load(&db, config.vapid_private_pem.clone(), subject).await?;
        Ok(Self(Arc::new(Inner {
            db,
            config,
            hub: Hub::default(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(HTTP_TIMEOUT)
                .build()?,
            webauthn,
            vapid,
            presence: presence::Presence::default(),
            owner_view: std::sync::Mutex::new(None),
            typing: std::sync::Mutex::new(Default::default()),
        })))
    }

    /// True while the owner is looking at this channel in the PWA (confirmed within the last 12 s).
    pub fn owner_viewing(&self, channel_id: i64) -> bool {
        matches!(*self.owner_view.lock().unwrap(), Some((c, at)) if c == channel_id && at.elapsed() < std::time::Duration::from_secs(12))
    }

    pub fn start_typing(&self, channel_id: i64, secs: i64) -> String {
        let until = chrono::Utc::now() + chrono::Duration::seconds(secs);
        self.typing.lock().unwrap().insert(channel_id, until);
        util::ts(until)
    }

    pub fn stop_typing(&self, channel_id: i64) -> bool {
        self.typing.lock().unwrap().remove(&channel_id).is_some()
    }

    pub fn typing_until(&self, channel_id: i64) -> Option<String> {
        let mut map = self.typing.lock().unwrap();
        match map.get(&channel_id) {
            Some(until) if *until > chrono::Utc::now() => Some(util::ts(*until)),
            Some(_) => {
                map.remove(&channel_id);
                None
            }
            None => None,
        }
    }

    pub fn changed(&self, channel_id: i64) {
        let _ = self.hub.changes.send(channel_id);
    }

    pub fn notify_event(&self, id: i64, kind: EventKind) {
        let _ = self.hub.notifications.send(NotifEvent { id, kind });
    }
}

pub async fn connect(path: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening database {path}"))?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub fn router(state: AppState) -> axum::Router {
    routes::router(state)
}

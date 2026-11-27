//! Who is listening on each channel, as the CLI's `whoami` and the PWA see it.
//!
//! A consumer is live while it holds a `?wait=` long-poll, and for 60 s after any queue read, so a
//! crashed loop never fakes presence.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::server::AppState;
use crate::util::now;

const DECAY: Duration = Duration::from_secs(60);

struct Consumer {
    label: Option<String>,
    kind: Option<String>,
    transport: &'static str,
    holds: u32,
    last_seen: Instant,
    last_seen_at: String,
    last_read: Option<Instant>,
}

impl Consumer {
    /// Held right now, or read the queue within the decay window; a dropped socket alone grants no grace.
    fn live(&self) -> bool {
        self.holds > 0 || self.last_read.is_some_and(|t| t.elapsed() < DECAY)
    }
}

#[derive(Default)]
pub struct Presence {
    channels: Mutex<HashMap<i64, HashMap<String, Consumer>>>,
}

pub struct Who<'a> {
    pub fingerprint: Option<&'a str>,
    pub label: Option<&'a str>,
    pub kind: Option<&'a str>,
}

impl Presence {
    /// Records activity; returns true when the channel went from nobody listening to someone.
    fn record(
        &self,
        channel_id: i64,
        who: &Who,
        transport: &'static str,
        hold: i32,
        read: bool,
    ) -> bool {
        let mut channels = self.channels.lock().unwrap();
        let consumers = channels.entry(channel_id).or_default();
        let was_live = consumers.values().any(Consumer::live);
        let key = who.fingerprint.unwrap_or("anonymous").to_string();
        let c = consumers.entry(key).or_insert_with(|| Consumer {
            label: None,
            kind: None,
            transport,
            holds: 0,
            last_seen: Instant::now(),
            last_seen_at: now(),
            last_read: None,
        });
        if who.label.is_some() {
            c.label = who.label.map(String::from);
        }
        if who.kind.is_some() {
            c.kind = who.kind.map(String::from);
        }
        c.transport = transport;
        c.holds = c.holds.saturating_add_signed(hold);
        c.last_seen = Instant::now();
        c.last_seen_at = now();
        if read {
            c.last_read = Some(Instant::now());
        }
        !was_live && consumers.values().any(Consumer::live)
    }

    pub fn forget(&self, channel_id: i64) {
        self.channels.lock().unwrap().remove(&channel_id);
    }

    pub fn listening(&self, channel_id: i64) -> bool {
        self.channels
            .lock()
            .unwrap()
            .get(&channel_id)
            .is_some_and(|cs| cs.values().any(Consumer::live))
    }

    pub fn last_seen_at(&self, channel_id: i64) -> Option<String> {
        self.channels
            .lock()
            .unwrap()
            .get(&channel_id)?
            .values()
            .map(|c| c.last_seen_at.clone())
            .max()
    }

    /// The `consumers` array of whoami.
    pub fn consumers(&self, channel_id: i64) -> Vec<Value> {
        let channels = self.channels.lock().unwrap();
        let Some(cs) = channels.get(&channel_id) else {
            return vec![];
        };
        cs.iter()
            .filter(|(_, c)| c.last_seen.elapsed() < Duration::from_secs(24 * 3600))
            .map(|(fp, c)| {
                json!({
                    "fingerprint": (fp != "anonymous").then_some(fp),
                    "label": c.label,
                    "kind": c.kind,
                    "transport": c.transport,
                    "live": c.live(),
                    "listening": c.live(),
                    "last_seen_at": c.last_seen_at,
                })
            })
            .collect()
    }
}

impl AppState {
    /// A queue read: keeps the consumer live for the decay window.
    pub fn touch_consumer(&self, channel_id: i64, who: &Who, transport: &'static str) {
        if self.presence.record(channel_id, who, transport, 0, true) {
            self.changed(channel_id);
        }
        self.recheck_after_decay(channel_id);
    }

    /// Holds the consumer live until the guard drops (a long-poll).
    pub fn hold_consumer(&self, channel_id: i64, who: &Who, transport: &'static str) -> Hold {
        if self.presence.record(channel_id, who, transport, 1, false) {
            self.changed(channel_id);
        }
        Hold {
            st: self.clone(),
            channel_id,
            fingerprint: who.fingerprint.map(String::from),
            transport,
        }
    }

    /// Tells the PWA to look again once the decay window has passed, so "offline" shows up on its own.
    fn recheck_after_decay(&self, channel_id: i64) {
        let st = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DECAY + Duration::from_secs(1)).await;
            if !st.presence.listening(channel_id) {
                st.changed(channel_id);
            }
        });
    }
}

pub struct Hold {
    st: AppState,
    channel_id: i64,
    fingerprint: Option<String>,
    transport: &'static str,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let who = Who {
            fingerprint: self.fingerprint.as_deref(),
            label: None,
            kind: None,
        };
        self.st
            .presence
            .record(self.channel_id, &who, self.transport, -1, false);
        self.st.recheck_after_decay(self.channel_id);
    }
}

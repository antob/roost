use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::server::AppState;
use crate::server::routes::Notification;

const ATTEMPTS: u32 = 8;

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

pub fn payload(n: &Notification) -> Value {
    let chosen = n.chosen();
    let mut body = json!({
        "type": "roost.event",
        "event": chosen["kind"],
        "action_id": chosen["action_id"],
        "label": chosen["label"],
        "text": chosen["text"],
        "correlation_id": n.correlation_id,
        "notification_id": n.id,
        "title": n.title,
        "payload": { "action_id": chosen["action_id"], "label": chosen["label"] },
        "occurred_at": chosen["at"],
    });
    if let Some(d) = &n.deliver_at {
        body["deliver_at"] = d.clone().into();
    }
    if let Some(d) = &n.delivered_at {
        body["delivered_at"] = d.clone().into();
    }
    body
}

/// POST the answer to `reply_to`, retrying with exponential backoff in the background.
pub fn dispatch(st: &AppState, n: &Notification) {
    let Some(url) = n.reply_to.clone() else {
        return;
    };
    let Some(secret) = st.config.callback_secret.clone() else {
        tracing::warn!(correlation_id = %n.correlation_id, "reply_to set but no callback secret configured; webhook skipped");
        return;
    };
    let body = payload(n).to_string();
    let signature = sign(&secret, body.as_bytes());
    let http = st.http.clone();
    let cid = n.correlation_id.clone();
    tokio::spawn(async move {
        for attempt in 0..ATTEMPTS {
            let res = http
                .post(&url)
                .header("content-type", "application/json")
                .header("x-roost-signature", &signature)
                .timeout(Duration::from_secs(15))
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => return,
                Ok(r) => tracing::warn!(%cid, status = %r.status(), attempt, "webhook rejected"),
                Err(e) => tracing::warn!(%cid, error = %e, attempt, "webhook failed"),
            }
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt + 1).min(300))).await;
        }
        tracing::error!(%cid, "webhook gave up after {ATTEMPTS} attempts");
    });
}

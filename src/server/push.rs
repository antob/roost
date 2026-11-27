use serde_json::json;
use sqlx::SqlitePool;
use web_push::request_builder::build_request;
use web_push::{
    ContentEncoding, PartialVapidSignatureBuilder, SubscriptionInfo, Urgency,
    VapidSignatureBuilder, WebPushMessageBuilder,
};

use crate::server::AppState;
use crate::server::routes::Notification;
use crate::util::{now, parse_when, sha256_hex, url_safe_b64};

#[derive(Debug, Clone, Copy)]
pub enum Push {
    Initial,
    Refire,
    Escalation,
    /// An event's "starting soon" push, `lead_minutes` before `event_at`.
    Reminder,
}

pub struct Vapid {
    pem: String,
    subject: String,
    pub public_key: String,
}

impl Vapid {
    /// Uses `pem` if given, else the key stored in settings (generated on first run).
    pub async fn load(
        db: &SqlitePool,
        pem: Option<String>,
        subject: String,
    ) -> anyhow::Result<Self> {
        let pem = match pem {
            Some(p) => p,
            None => {
                crate::server::owner::setting_or_init(db, "vapid_private_pem", generate_pem).await?
            }
        };
        let partial: PartialVapidSignatureBuilder =
            VapidSignatureBuilder::from_pem_no_sub(pem.as_bytes())?;
        Ok(Self {
            public_key: url_safe_b64(&partial.get_public_key()),
            pem,
            subject,
        })
    }
}

fn generate_pem() -> anyhow::Result<String> {
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let key = EcKey::generate(&group)?;
    Ok(String::from_utf8(key.private_key_to_pem()?)?)
}

pub async fn device_count(db: &SqlitePool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM push_subscriptions"#)
        .fetch_one(db)
        .await
}

/// The Web Push `Topic` for a collapse key: a newer push with the same topic replaces an undelivered one.
pub fn topic(n: &Notification) -> Option<String> {
    let key = n.collapse_key.as_deref()?;
    Some(sha256_hex(format!("{}:{key}", n.channel_id).as_bytes())[..32].to_string())
}

fn starts_in(n: &Notification) -> String {
    let mins = n
        .event_at
        .as_deref()
        .and_then(parse_when)
        .map(|t| (t - chrono::Utc::now()).num_seconds())
        .map(|s| (s + 30) / 60);
    match mins {
        Some(m) if m >= 1 => format!("In {m} min: {}", n.title),
        _ => format!("Starting now: {}", n.title),
    }
}

pub fn payload(
    n: &Notification,
    channel: &str,
    push: Push,
    badge: Option<usize>,
) -> serde_json::Value {
    let title = match push {
        Push::Initial => n.title.clone(),
        Push::Refire => format!("⏰ {}", n.title),
        Push::Escalation => format!("🚨 Still waiting: {}", n.title),
        Push::Reminder => format!("⏰ {}", starts_in(n)),
    };
    let body: String = n
        .body
        .as_deref()
        .or(n.subtitle.as_deref())
        .unwrap_or("")
        .chars()
        .take(240)
        .collect();
    // A collapse key shares one banner slot; a reminder gets its own so it alerts again.
    let tag = match (push, topic(n)) {
        (Push::Reminder, _) => format!("r-{}", n.id),
        (_, Some(t)) => format!("c-{t}"),
        _ => format!("n-{}", n.id),
    };
    json!({
        "id": n.id,
        "title": title,
        "body": body,
        "channel": channel,
        "profile": n.profile,
        "requires_action": n.requires_action(),
        "sticky": n.profile == "urgent" && n.urgency != "normal",
        "url": format!("/n/{}", n.id),
        "tag": tag,
        "badge": badge,
    })
}

/// Sends the notification to every registered device and returns how many accepted it.
pub async fn send(st: &AppState, n: &Notification, push: Push) -> usize {
    let channel = sqlx::query_scalar!("SELECT name FROM channels WHERE id = ?", n.channel_id)
        .fetch_optional(&st.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let urgency = match (n.profile.as_str(), push) {
        (_, Push::Escalation | Push::Reminder) => Urgency::High,
        _ if n.profile == "message" || n.urgency == "normal" => Urgency::Normal,
        _ => Urgency::High,
    };
    let badge = crate::server::routes::open_count(st)
        .await
        .inspect_err(|e| tracing::warn!("counting open items failed: {e}"))
        .ok();
    let body = payload(n, &channel, push, badge).to_string();
    broadcast(st, body.as_bytes(), urgency, topic(n).as_deref()).await
}

pub async fn broadcast(st: &AppState, body: &[u8], urgency: Urgency, topic: Option<&str>) -> usize {
    let subs =
        match sqlx::query!(r#"SELECT id as "id!", endpoint, p256dh, auth FROM push_subscriptions"#)
            .fetch_all(&st.db)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("loading push subscriptions failed: {e}");
                return 0;
            }
        };
    let sends = subs.into_iter().map(|s| async move {
        let info = SubscriptionInfo::new(s.endpoint.clone(), s.p256dh, s.auth);
        match deliver_one(st, &info, body, urgency, topic).await {
            Ok(()) => {
                let at = now();
                let _ = sqlx::query!(
                    "UPDATE push_subscriptions SET last_success_at = ?, failures = 0 WHERE id = ?",
                    at,
                    s.id
                )
                .execute(&st.db)
                .await;
                true
            }
            Err(Failure::Gone) => {
                tracing::info!(endpoint = %s.endpoint, "push subscription gone; removing it");
                let _ = sqlx::query!("DELETE FROM push_subscriptions WHERE id = ?", s.id)
                    .execute(&st.db)
                    .await;
                false
            }
            Err(Failure::Other(e)) => {
                tracing::warn!(endpoint = %s.endpoint, "push failed: {e:#}");
                let at = now();
                let _ = sqlx::query!(
                    "UPDATE push_subscriptions SET last_failure_at = ?, failures = failures + 1 WHERE id = ?",
                    at,
                    s.id
                )
                .execute(&st.db)
                .await;
                false
            }
        }
    });
    futures_util::future::join_all(sends)
        .await
        .into_iter()
        .filter(|ok| *ok)
        .count()
}

enum Failure {
    Gone,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure::Other(e.into())
    }
}

async fn deliver_one(
    st: &AppState,
    info: &SubscriptionInfo,
    body: &[u8],
    urgency: Urgency,
    topic: Option<&str>,
) -> Result<(), Failure> {
    let mut sig = VapidSignatureBuilder::from_pem(st.vapid.pem.as_bytes(), info)?;
    sig.add_claim("sub", st.vapid.subject.as_str());
    let mut msg = WebPushMessageBuilder::new(info);
    msg.set_payload(ContentEncoding::Aes128Gcm, body);
    msg.set_ttl(24 * 3600);
    msg.set_urgency(urgency);
    if let Some(t) = topic {
        msg.set_topic(t.to_string());
    }
    msg.set_vapid_signature(sig.build()?);
    let req = build_request::<Vec<u8>>(msg.build()?);

    let mut out = st.http.post(req.uri().to_string());
    for (k, v) in req.headers() {
        out = out.header(k.as_str(), v.as_bytes());
    }
    let res = out.body(req.into_body()).send().await?;
    match res.status().as_u16() {
        200..=299 => Ok(()),
        404 | 410 => Err(Failure::Gone),
        code => {
            let text = res.text().await.unwrap_or_default();
            Err(Failure::Other(anyhow::anyhow!(
                "push service answered {code}: {text}"
            )))
        }
    }
}

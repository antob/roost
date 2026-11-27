use std::net::IpAddr;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use serde_json::{Map, Value, json};

use crate::server::AppState;
use crate::server::auth::{ChannelAuth, Identity};
use crate::server::channels;
use crate::server::error::{AppError, AppResult};
use crate::server::push::Push;
use crate::server::routes::notifications::{builtin, load_by_cid};
use crate::server::scheduler;
use crate::util::{now, parse_when, ts};

const PROFILES: &[&str] = &["message", "important", "urgent", "event", "live"];
const RESERVED_ACTION_IDS: &[&str] = &["dismiss", "acknowledge"];

fn canonical_kind(raw: &str) -> Option<&'static str> {
    Some(match raw {
        "message" | "fyi" | "default" => "message",
        "important" | "report" | "ask" => "important",
        "urgent" | "alert" | "escalating" => "urgent",
        "event" => "event",
        "live" | "tracking" => "live",
        _ => return None,
    })
}

fn urgency_rank(u: &str) -> u8 {
    match u {
        "normal" => 0,
        "persistent" => 1,
        _ => 2,
    }
}

fn profile_urgency(profile: &str) -> &'static str {
    if profile == "message" {
        "normal"
    } else {
        "persistent"
    }
}

fn str_field<'a>(body: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn truncated(body: &Map<String, Value>, key: &str, max: usize) -> Option<String> {
    str_field(body, key).map(|s| s.chars().take(max).collect())
}

fn validate_custom_actions(raw: Option<&Value>) -> AppResult<Vec<Value>> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let invalid = |msg: String| AppError::contract("invalid_custom_actions", msg);
    let list = raw
        .as_array()
        .ok_or_else(|| invalid("custom_actions must be an array".into()))?;
    if list.len() > 8 {
        return Err(invalid("custom_actions: at most 8 actions".into()));
    }
    if raw.to_string().len() > 2048 {
        return Err(invalid("custom_actions: over 2 KB".into()));
    }
    let mut seen = Vec::new();
    for item in list {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let valid_id = !id.is_empty()
            && id.len() <= 40
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid_id {
            return Err(invalid(format!(
                "custom_actions: invalid id {id:?} (a-z0-9_, ≤40)"
            )));
        }
        if builtin(id).is_some() || RESERVED_ACTION_IDS.contains(&id) {
            return Err(invalid(format!(
                "custom_actions: id {id:?} collides with a built-in"
            )));
        }
        if seen.contains(&id) {
            return Err(invalid(format!("custom_actions: duplicate id {id:?}")));
        }
        seen.push(id);
        if item
            .get("label")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(invalid(format!(
                "custom_actions: {id:?} is missing a label"
            )));
        }
    }
    Ok(list.clone())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

fn validate_reply_to(raw: &str) -> AppResult<()> {
    let invalid = |why: &str| AppError::contract("invalid_reply_to", format!("reply_to {why}"));
    let url = reqwest::Url::parse(raw).map_err(|_| invalid("is not a valid URL"))?;
    if url.scheme() != "https" {
        return Err(invalid("must be https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not embed credentials"));
    }
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            if d == "localhost"
                || d.ends_with(".localhost")
                || d.ends_with(".local")
                || d.ends_with(".internal")
            {
                return Err(invalid("must resolve to a public host"));
            }
        }
        Some(url::Host::Ipv4(ip)) if is_private_ip(ip.into()) => {
            return Err(invalid("must resolve to a public host"));
        }
        Some(url::Host::Ipv6(ip)) if is_private_ip(ip.into()) => {
            return Err(invalid("must resolve to a public host"));
        }
        Some(_) => {}
        None => return Err(invalid("has no host")),
    }
    Ok(())
}

/// When to push the "starting soon" reminder: `lead_minutes` (default 15) before the event.
fn event_reminder(event_at: &str, lead: Option<&Value>) -> AppResult<Option<String>> {
    let lead = match lead.filter(|v| !v.is_null()) {
        None => 15,
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .filter(|m| (0..=7 * 24 * 60).contains(m))
            .ok_or_else(|| {
                AppError::contract(
                    "invalid_lead_minutes",
                    "lead_minutes must be a whole number of minutes, 0..10080",
                )
            })?,
    };
    // Without an offset the instant is ambiguous: keep the event, skip the reminder.
    let Some(at) = parse_when(event_at) else {
        return Ok(None);
    };
    let remind = at - chrono::Duration::minutes(lead);
    Ok((remind > chrono::Utc::now() + chrono::Duration::seconds(5)).then(|| ts(remind)))
}

/// The re-alert delay for `escalate: true`, the floor of the channel setting.
const ESCALATE_SOON_SECS: i64 = 60;

pub async fn notify(
    State(st): State<AppState>,
    auth: ChannelAuth,
    ident: Identity,
    body: Result<Json<Map<String, Value>>, JsonRejection>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let Json(body) = body.map_err(|e| AppError::bad_request(format!("malformed JSON: {e}")))?;
    let channel = channels::load(&st.db, auth.channel_id)
        .await?
        .ok_or_else(AppError::unauthorized)?;

    let title = str_field(&body, "title")
        .ok_or_else(|| AppError::bad_request("param is missing or the value is empty: title"))?
        .to_string();

    let kind_input = str_field(&body, "template_kind");
    let template_kind = match kind_input {
        Some(k) => canonical_kind(k).ok_or_else(|| {
            AppError::contract_with(
                "unknown_template_kind",
                format!("unknown template_kind {k:?}"),
                json!({ "valid": PROFILES }),
            )
        })?,
        None => "message",
    };
    let requested_profile = match str_field(&body, "profile") {
        Some(p) => Some(canonical_kind(p).ok_or_else(|| {
            AppError::contract_with(
                "unknown_profile",
                format!("unknown profile {p:?}"),
                json!({ "valid": PROFILES }),
            )
        })?),
        None => kind_input.map(|_| template_kind),
    };
    let profile = requested_profile
        .unwrap_or_else(|| canonical_kind(&channel.default_profile).unwrap_or("message"));
    if profile == "live" {
        return Err(AppError::contract_with(
            "tracking_not_notify",
            "the live profile is a Live Activity, not a notification",
            json!({ "endpoint": "POST /api/v1/live_activities" }),
        ));
    }
    let event_at = str_field(&body, "event_at").map(String::from);
    if profile == "event" && event_at.is_none() {
        return Err(AppError::contract(
            "event_at_required",
            "profile event requires event_at (ISO8601)",
        ));
    }
    let remind_at = match &event_at {
        Some(at) if profile == "event" => event_reminder(at, body.get("lead_minutes"))?,
        _ => None,
    };

    let wanted_urgency = match str_field(&body, "urgency") {
        Some(u @ ("normal" | "persistent" | "alarm")) => u.to_string(),
        Some(u) => {
            return Err(AppError::contract(
                "unknown_urgency",
                format!("unknown urgency {u:?}"),
            ));
        }
        None => profile_urgency(profile).to_string(),
    };
    let (urgency, degrade_reason) =
        if urgency_rank(&wanted_urgency) > urgency_rank(&channel.max_urgency) {
            (channel.max_urgency.clone(), Some("channel_max_urgency"))
        } else if wanted_urgency == "alarm" {
            // A web push can never ring an AlarmKit alarm.
            ("persistent".to_string(), Some("no_alarm_capable_device"))
        } else {
            (wanted_urgency, None)
        };

    let actions: Vec<String> = body
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|id| builtin(id).is_some())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let custom_actions = validate_custom_actions(body.get("custom_actions"))?;

    let correlation_id = match str_field(&body, "correlation_id") {
        Some(c) if c.chars().count() > 128 => {
            return Err(AppError::contract(
                "correlation_id_too_long",
                "correlation_id is an ID handle: at most 128 chars",
            ));
        }
        Some(c) => c.to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let reply_to = str_field(&body, "reply_to").map(String::from);
    if let Some(r) = &reply_to {
        validate_reply_to(r)?;
    }
    let after_cid = truncated(&body, "after", 128);
    let waits_behind = match &after_cid {
        Some(a) if *a == correlation_id => {
            return Err(AppError::contract(
                "invalid_after",
                "after names this notification itself",
            ));
        }
        Some(a) => !load_by_cid(&st.db, channel.id, a)
            .await?
            .ok_or_else(|| {
                AppError::contract(
                    "unknown_after",
                    format!("after: no notification with correlation_id {a:?} on this channel"),
                )
            })?
            .is_settled(),
        None => false,
    };
    let deliver_at = match str_field(&body, "deliver_at") {
        Some(d) => Some(parse_when(d).ok_or_else(|| {
            AppError::contract(
                "invalid_deliver_at",
                "deliver_at must be ISO8601 with an offset",
            )
        })?),
        None => None,
    };
    let status = match deliver_at {
        _ if waits_behind => "queued",
        Some(t) if t > chrono::Utc::now() => "scheduled",
        _ => "pending",
    };
    let deliver_at = deliver_at.map(ts);
    let next_fire_at = (status == "scheduled")
        .then(|| deliver_at.clone())
        .flatten();
    let escalation_state = match (profile, channel.max_urgency.as_str()) {
        ("urgent", "alarm") => Some("pending"),
        ("urgent", _) => Some("disabled_by_channel"),
        _ => None,
    };
    let escalate = body.get("escalate").and_then(Value::as_bool) == Some(true);
    let escalate_after_secs = escalation_state.map(|_| {
        if escalate {
            ESCALATE_SOON_SECS.min(channel.escalate_after_secs)
        } else {
            channel.escalate_after_secs
        }
    });

    let subtitle = truncated(&body, "subtitle", 1000);
    let text = truncated(&body, "body", 4000);
    let body_markdown = str_field(&body, "body_markdown").map(String::from);
    let url = truncated(&body, "url", 1024);
    let copy = truncated(&body, "copy", 512);
    let thread_id = truncated(&body, "thread_id", 128);
    let collapse_key = truncated(&body, "collapse_key", 256);
    let sent_note = truncated(&body, "sent_note", 1000);
    let action_data = body
        .get("action_data")
        .filter(|v| !v.is_null())
        .map(Value::to_string);
    let mirror_reply = body
        .get("mirror_reply")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let actions_json = serde_json::to_string(&actions).unwrap_or_else(|_| "[]".into());
    let custom_json = serde_json::to_string(&custom_actions).unwrap_or_else(|_| "[]".into());
    let sent_by = ident.attribution();
    let created_at = now();

    let existing = load_by_cid(&st.db, channel.id, &correlation_id).await?;
    let updated = match &existing {
        Some(n) if matches!(n.status.as_str(), "scheduled" | "queued") => {
            sqlx::query!(
                "UPDATE notifications SET title = ?, subtitle = ?, body = ?, body_markdown = ?, template_kind = ?,
                        profile = ?, urgency = ?, actions = ?, custom_actions = ?, url = ?, copy = ?, thread_id = ?,
                        collapse_key = ?, action_data = ?, reply_to = ?, mirror_reply = ?, sent_note = ?,
                        event_at = ?, deliver_at = ?, status = ?, next_fire_at = ?, escalation_state = ?,
                        escalate_after_secs = ?, after_cid = ?, remind_at = ? WHERE id = ?",
                title, subtitle, text, body_markdown, template_kind, profile, urgency, actions_json, custom_json,
                url, copy, thread_id, collapse_key, action_data, reply_to, mirror_reply, sent_note, event_at,
                deliver_at, status, next_fire_at, escalation_state, escalate_after_secs, after_cid, remind_at, n.id
            )
            .execute(&st.db)
            .await?;
            true
        }
        Some(_) => false,
        None => {
            sqlx::query!(
                "INSERT INTO notifications (channel_id, correlation_id, title, subtitle, body, body_markdown,
                        template_kind, profile, urgency, actions, custom_actions, url, copy, thread_id, collapse_key,
                        action_data, reply_to, mirror_reply, sent_note, sent_by_label, event_at, deliver_at, status,
                        created_at, next_fire_at, escalation_state, escalate_after_secs, after_cid, remind_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                channel.id, correlation_id, title, subtitle, text, body_markdown, template_kind, profile, urgency,
                actions_json, custom_json, url, copy, thread_id, collapse_key, action_data, reply_to, mirror_reply,
                sent_note, sent_by, event_at, deliver_at, status, created_at, next_fire_at, escalation_state,
                escalate_after_secs, after_cid, remind_at
            )
            .execute(&st.db)
            .await?;
            false
        }
    };

    let reload = async || {
        load_by_cid(&st.db, channel.id, &correlation_id)
            .await?
            .ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("notification vanished after insert"))
            })
    };
    let mut n = reload().await?;
    let wrote = updated || existing.is_none();
    if wrote && n.status == "pending" {
        scheduler::deliver(&st, &n, Push::Initial).await?;
        n = reload().await?;
    } else if wrote && n.status == "scheduled" {
        st.hub.schedule.notify_one();
    }
    if wrote {
        st.stop_typing(channel.id);
        st.changed(channel.id);
    }
    let mut out = n.json();
    out["registered_devices"] = crate::server::push::device_count(&st.db).await?.into();
    out["requested_profile"] = requested_profile.map(Value::from).unwrap_or(Value::Null);
    out["degraded"] = degrade_reason.is_some().into();
    out["degrade_reason"] = degrade_reason.map(Value::from).unwrap_or(Value::Null);
    out["mirror_reply"] = n.mirror_reply.into();
    let listening = st.presence.listening(channel.id);
    out["listening"] = listening.into();
    out["nobody_listening"] = (!listening).into();
    if updated {
        out["updated"] = true.into();
    }
    Ok((StatusCode::CREATED, Json(out)))
}

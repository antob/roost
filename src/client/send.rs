//! Sends (`message`, `important`, `urgent`, `event`), the blocking shortcuts (`ask`, `approval`,
//! `approve`, `hello`), and `wait`, `cancel`, `inbox`, `catchup`.

use std::io::Read;

use clap::Args;
use reqwest::Method;
use serde_json::{Map, Value, json};

use super::http::Api;
use super::wait::{Outcome, Wait, print_outcome, timed_out};
use super::{Exit, Res, lenient_int, print_json};

#[derive(Args)]
pub struct SendArgs {
    /// The headline (required).
    #[arg(long)]
    title: Option<String>,
    /// Plain-text body.
    #[arg(long)]
    body: Option<String>,
    /// Markdown body.
    #[arg(long, short = 'm')]
    body_markdown: Option<String>,
    /// Read the markdown body from a file, or `-` for stdin.
    #[arg(long)]
    body_markdown_file: Option<String>,
    #[arg(long)]
    subtitle: Option<String>,
    /// Content/action pattern, e.g. approval, onboarding.
    #[arg(long)]
    template: Option<String>,
    /// Delivery profile.
    #[arg(long)]
    profile: Option<String>,
    /// When the thing happens (RFC 3339), for `event`.
    #[arg(long)]
    event_at: Option<String>,
    /// Minutes before --event-at for the "starting soon" push (default 15).
    #[arg(long)]
    lead_minutes: Option<String>,
    /// normal | persistent | alarm (prefer the send type).
    #[arg(long)]
    urgency: Option<String>,
    /// A link the app opens.
    #[arg(long)]
    url: Option<String>,
    /// A tap-to-copy value shown on the item.
    #[arg(long)]
    copy: Option<String>,
    /// Buttons: a comma list (yes,no · approve,reject · done · reply …) or a JSON array of {"id","label"}.
    #[arg(long)]
    actions: Option<String>,
    /// A custom button, id:label[:destructive][:confirm][:biometric][:terminal]; repeatable.
    #[arg(long = "custom-action")]
    custom_action: Vec<String>,
    /// Add one Face ID confirm button.
    #[arg(long)]
    gated: bool,
    /// Hold the notification until this time (RFC 3339).
    #[arg(long)]
    deliver_at: Option<String>,
    /// POST the answer to this URL, signed.
    #[arg(long)]
    reply_to: Option<String>,
    /// Your own id for this notification; reusing it updates a scheduled one.
    #[arg(long)]
    correlation_id: Option<String>,
    /// Group follow-ups into one thread.
    #[arg(long)]
    thread: Option<String>,
    /// Queue this behind an earlier notification: it reaches the human once that one is answered or cancelled.
    #[arg(long)]
    after: Option<String>,
    /// A newer send with the same key replaces this one on the phone (progress updates).
    #[arg(long)]
    collapse_key: Option<String>,
    /// Why this was sent (shown to the human as metadata).
    #[arg(long)]
    note: Option<String>,
    /// key=value, a raw /notify field (value parsed as JSON when it parses); repeatable.
    #[arg(long)]
    param: Vec<String>,
    /// Block until the human answers.
    #[arg(long)]
    wait: bool,
    /// Seconds to wait for the answer.
    #[arg(long, value_parser = lenient_int)]
    timeout: Option<i64>,
    /// Seconds between polls when the server can't hold a request.
    #[arg(long, value_parser = lenient_int)]
    interval: Option<i64>,
}

#[derive(Args)]
pub struct UrgentArgs {
    #[command(flatten)]
    pub send: SendArgs,
    /// Re-alert after 1 minute if unanswered, instead of the channel's delay.
    #[arg(long)]
    pub escalate: bool,
}

#[derive(Args)]
pub struct ApproveArgs {
    /// The question to approve.
    question: Option<String>,
    #[arg(long)]
    allow_label: Option<String>,
    #[arg(long)]
    deny_label: Option<String>,
    #[command(flatten)]
    send: SendArgs,
}

#[derive(Args)]
pub struct WaitArgs {
    correlation_id: String,
    #[arg(long, value_parser = lenient_int, default_value = "300")]
    timeout: i64,
    #[arg(long, value_parser = lenient_int, default_value = "30")]
    interval: i64,
}

#[derive(Args)]
pub struct InboxArgs {
    /// Only notifications still waiting for an answer.
    #[arg(long)]
    pending: bool,
    /// Counts instead of rows.
    #[arg(long)]
    summary: bool,
    #[arg(long)]
    limit: Option<String>,
}

#[derive(Args)]
pub struct CatchupArgs {
    /// At most this many messages, newest first.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    limit: Option<u32>,
    /// Only messages older than this id.
    #[arg(long)]
    before: Option<u64>,
    /// Only messages newer than this id.
    #[arg(long)]
    since: Option<u64>,
    /// One line per message: id · kind · body · state.
    #[arg(long)]
    digest: bool,
}

const CUSTOM_ID: fn(&str) -> bool = |id| {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
};

fn custom_from_spec(spec: &str) -> Res<Value> {
    let mut parts = spec.split(':');
    let id = parts.next().unwrap_or_default();
    if !CUSTOM_ID(id) {
        return Err(Exit::usage(format!(
            "roost: --custom-action id {id:?} is invalid: lowercase letters, digits and _ only, at most 40"
        )));
    }
    let label = parts.next().unwrap_or(id);
    let mut ca = json!({ "id": id, "label": label });
    for flag in parts {
        match flag {
            "destructive" => ca["style"] = "destructive".into(),
            "confirm" | "biometric" | "terminal" => ca[flag] = true.into(),
            _ => {}
        }
    }
    Ok(ca)
}

fn custom_from_json(i: usize, item: &Value) -> Res<Value> {
    let bad = |what: &str| Exit::usage(format!("roost: --actions[{i}] {what}"));
    let obj = item
        .as_object()
        .ok_or_else(|| bad("must be an object with \"id\" and \"label\""))?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| CUSTOM_ID(id));
    let id = id.ok_or_else(|| bad(".id is invalid: lowercase letters, digits and _ only"))?;
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .filter(|l| !l.trim().is_empty())
        .ok_or_else(|| bad(".label is required"))?;
    let mut ca = json!({ "id": id, "label": label });
    for k in ["sf_symbol", "style"] {
        if let Some(v) = obj.get(k) {
            ca[k] = v.clone();
        }
    }
    if obj
        .get("destructive")
        .is_some_and(|v| v == &Value::Bool(true))
    {
        ca["style"] = "destructive".into();
    }
    for k in ["confirm", "biometric", "terminal"] {
        if let Some(v) = obj.get(k) {
            ca[k] = Value::Bool(v.as_bool().unwrap_or(false));
        }
    }
    Ok(ca)
}

impl SendArgs {
    fn answerable(&self) -> bool {
        self.actions.is_some() || !self.custom_action.is_empty() || self.template.is_some()
    }

    /// The /notify body; `extra` fields override the flags and `--param` overrides everything.
    fn body(&self, extra: Value) -> Res<Value> {
        let title = self
            .title
            .as_deref()
            .ok_or_else(|| Exit::usage("roost: --title is required"))?;
        let mut b = Map::new();
        b.insert("title".into(), title.into());
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                b.insert(k.into(), v.clone().into());
            }
        };
        put("body", &self.body);
        put("subtitle", &self.subtitle);
        put("template", &self.template);
        put("profile", &self.profile);
        put("event_at", &self.event_at);
        put("urgency", &self.urgency);
        put("url", &self.url);
        put("copy", &self.copy);
        put("deliver_at", &self.deliver_at);
        put("reply_to", &self.reply_to);
        put("correlation_id", &self.correlation_id);
        put("thread_id", &self.thread);
        put("after", &self.after);
        put("sent_note", &self.note);
        put("collapse_key", &self.collapse_key);
        if let Some(f) = &self.body_markdown_file {
            let mut md = String::new();
            let read = if f == "-" {
                std::io::stdin().read_to_string(&mut md).map(|_| ())
            } else {
                std::fs::read_to_string(f).map(|s| md = s)
            };
            read.map_err(|e| Exit::usage(format!("roost: --body-markdown-file {f}: {e}")))?;
            b.insert("body_markdown".into(), md.into());
        } else if let Some(md) = &self.body_markdown {
            b.insert("body_markdown".into(), md.clone().into());
        }
        if let Some(lm) = &self.lead_minutes {
            b.insert("lead_minutes".into(), json!(lenient_int(lm).ok()));
        }

        let mut custom = Vec::new();
        if let Some(actions) = &self.actions {
            let t = actions.trim();
            if t.starts_with('[') {
                let arr: Value = serde_json::from_str(t).map_err(|e| {
                    Exit::usage(format!(
                        "roost: --actions looks like JSON but didn't parse ({e}). Use a JSON array of \
                         {{\"id\",\"label\"}} objects, or the short form yes,no"
                    ))
                })?;
                let arr = arr.as_array().ok_or_else(|| {
                    Exit::usage(
                        "roost: --actions JSON must be an array of {\"id\",\"label\"} objects",
                    )
                })?;
                for (i, item) in arr.iter().enumerate() {
                    custom.push(custom_from_json(i, item)?);
                }
            } else {
                let list: Vec<&str> = t.split(',').filter(|a| !a.is_empty()).collect();
                const DECISIONS: [&str; 7] = [
                    "yes", "no", "approve", "reject", "accept", "decline", "later",
                ];
                let clash: Vec<&str> = list
                    .iter()
                    .copied()
                    .filter(|a| DECISIONS.contains(a))
                    .collect();
                if list.contains(&"reply") && !clash.is_empty() {
                    return Err(Exit::usage(format!(
                        "roost: --actions can't combine a decision button ({}) with reply: the human taps the \
                         button and you never get the text. Use --actions reply alone for a typed answer.",
                        clash.join(",")
                    )));
                }
                b.insert("actions".into(), json!(list));
            }
        }
        for spec in &self.custom_action {
            custom.push(custom_from_spec(spec)?);
        }
        if self.gated && !custom.iter().any(|c| c["biometric"] == true) {
            custom.push(
                json!({ "id": "confirm_action", "label": "Confirm", "style": "destructive",
                                "confirm": true, "biometric": true, "terminal": true }),
            );
        }
        if !custom.is_empty() {
            b.insert("custom_actions".into(), custom.into());
        }
        if let Value::Object(extra) = extra {
            b.extend(extra);
        }
        for pair in &self.param {
            let Some((k, raw)) = pair.split_once('=').filter(|(k, _)| !k.is_empty()) else {
                return Err(Exit::usage(format!(
                    "roost: --param expects KEY=VALUE, got {pair:?}"
                )));
            };
            let v = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
            b.insert(k.into(), v);
        }
        Ok(Value::Object(b))
    }
}

/// POSTs /notify and narrates the response; exit 2 on failure.
async fn notify(api: &Api, body: &Value) -> Res<(Value, String)> {
    let r = api
        .call("send", Method::POST, "/notify", Some(body))
        .await?;
    if r.status == 401 || r.status == 403 {
        return Err(super::http::key_rejected());
    }
    if !r.ok() {
        println!("{}", r.raw());
        return Err(Exit::error(format!(
            "roost: send failed ({}): {}",
            r.status,
            r.raw()
        )));
    }
    let info = &r.json;
    if info["updated"] == true {
        eprintln!(
            "roost: updated the scheduled notification (same correlation_id; nothing fires twice)"
        );
    }
    if info["registered_devices"] == 0 {
        eprintln!(
            "roost: no devices have notifications on; nobody will be alerted (it still shows in the app)"
        );
    }
    if matches!(
        info["escalation"]["state"].as_str(),
        Some("pending" | "armed")
    ) {
        let after = info["escalation"]["after_minutes"]
            .as_i64()
            .map_or("on delivery".into(), |m| format!("{m} min after delivery"));
        eprintln!("roost: re-alerts if unanswered {after} (answering or snoozing defuses it)");
    }
    if let Some(t) = info["thread_id"].as_str() {
        eprintln!("roost: thread={t}; send follow-ups with the same --thread");
    }
    if info["status"] == "queued" {
        eprintln!(
            "roost: queued behind {}; it reaches the human once that one is answered or cancelled",
            info["after"].as_str().unwrap_or("an earlier notification")
        );
    }
    if let Some(at) = info["remind_at"].as_str() {
        eprintln!("roost: a \"starting soon\" push follows at {at}");
    } else if info["profile"] == "event" {
        eprintln!(
            "roost: no reminder push (the reminder time has passed, or --event-at has no UTC offset)"
        );
    }
    Ok((r.json.clone(), r.raw().to_string()))
}

/// Mints the correlation id before the send so a lost response can still be waited on.
fn new_cid(a: &mut SendArgs) -> String {
    a.correlation_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

pub enum Kind {
    Plain,
    Ask,
    Approval,
}

pub async fn typed(api: &Api, kind: &str, mut a: SendArgs, extra: Value, mode: Kind) -> Res {
    if a.title.is_none() {
        return Err(Exit::usage("roost: --title is required"));
    }
    if kind == "event" {
        let at = a.event_at.as_deref().ok_or_else(|| {
            Exit::usage("roost: event needs --event-at, e.g. --event-at 2026-06-26T14:00+02:00")
        })?;
        if !valid_datetime(at) {
            return Err(Exit::usage(format!(
                "roost: --event-at {at:?} is not an ISO 8601 datetime, e.g. 2026-06-26T14:00+02:00"
            )));
        }
    }
    let mut extra = extra;
    match mode {
        Kind::Ask if !a.answerable() => {
            return Err(Exit::usage(
                "roost: ask needs buttons: --actions yes,no (or approve,reject, or reply) or --custom-action id:label",
            ));
        }
        Kind::Approval if !a.answerable() => {
            extra["custom_actions"] = json!([
                { "id": "grant", "label": "Approve", "biometric": true, "terminal": true },
                { "id": "deny", "label": "Reject", "style": "destructive", "terminal": true },
            ]);
        }
        _ => {}
    }
    extra["template_kind"] = kind.into();
    let wait = a.wait || !matches!(mode, Kind::Plain);
    if !wait {
        let (info, raw) = notify(api, &a.body(extra)?).await?;
        println!("{raw}");
        if let Some(cid) = info["correlation_id"].as_str() {
            eprintln!("roost: correlation_id={cid} (roost wait {cid} blocks for the answer)");
        }
        if info["nobody_listening"] == true {
            eprintln!(
                "roost: nobody is listening on this channel; an answer or reply waits until you run `roost online` (or `roost wait <cid>`)"
            );
        }
        return Ok(0);
    }
    let cid = new_cid(&mut a);
    let body = a.body(extra)?;
    eprintln!("roost: correlation_id={cid}");
    let (info, _) = notify(api, &body).await?;
    let timeout = a.timeout.map(|t| t.max(0) as u64).unwrap_or_else(|| {
        info["suggested_ask_timeout"]
            .as_u64()
            .unwrap_or(if info["requires_action"] == true {
                3600
            } else {
                600
            })
    });
    eprintln!("roost: sent; waiting up to {timeout}s on {cid}");
    let w = Wait {
        api,
        cid: &cid,
        timeout,
        interval: a.interval.unwrap_or(30).max(1) as u64,
        wake_on_message: true,
    };
    let (outcome, healthy) = w.run().await?;
    print_outcome(&cid, outcome, timeout, healthy)
}

/// Face ID approval: exit 0 only on an explicit allow; deny, timeout or failure is exit 1.
pub async fn approve(api: &Api, mut a: ApproveArgs) -> Res {
    let Some(question) = a.question.take().or_else(|| a.send.title.take()) else {
        return Err(Exit::usage(
            "roost: usage: roost approve \"<question>\" [--body TEXT] [--timeout N] [--allow-label L] [--deny-label L]",
        ));
    };
    a.send.title = Some(question);
    let allow = a.allow_label.as_deref().unwrap_or("Allow");
    let deny = a.deny_label.as_deref().unwrap_or("Deny");
    let timeout = a.send.timeout.unwrap_or(300).max(0) as u64;
    let cid = new_cid(&mut a.send);
    let body = a.send.body(json!({
        "template_kind": "important",
        "mirror_reply": false,
        "custom_actions": [
            { "id": "allow", "label": allow, "confirm": true, "biometric": true, "terminal": true },
            { "id": "deny", "label": deny, "style": "destructive", "terminal": true },
        ],
    }))?;
    eprintln!("roost: correlation_id={cid}");
    if let Err(e) = notify(api, &body).await {
        if let Some(m) = e.msg {
            eprintln!("{m}");
        }
        return Err(Exit::usage(
            "roost: could not send the approval; denied (nothing was approved)",
        ));
    }
    eprintln!("roost: approval sent; waiting up to {timeout}s on {cid} (only \"{allow}\" exits 0)");
    let wait = Wait {
        api,
        cid: &cid,
        timeout,
        interval: a.send.interval.unwrap_or(30).max(1) as u64,
        wake_on_message: false,
    };
    let interrupted = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let outcome = tokio::select! {
        r = wait.run() => r?.0,
        () = interrupted => {
            return Err(Exit::usage("roost: interrupted before an answer; denied (nothing was approved)"));
        }
    };
    match outcome {
        Outcome::Answer(chosen) => {
            print_json(&chosen);
            if chosen["action_id"] == "allow" {
                eprintln!("roost: allowed; the human approved with Face ID");
                Ok(0)
            } else {
                eprintln!(
                    "roost: denied; the human chose \"{}\"",
                    chosen["action_id"].as_str().unwrap_or("?")
                );
                Ok(1)
            }
        }
        Outcome::Cancelled => {
            println!(
                "{}",
                json!({ "decision": "deny", "reason": "cancelled", "correlation_id": cid })
            );
            eprintln!("roost: the approval was cancelled; denied (an approval fails closed)");
            Ok(1)
        }
        _ => {
            println!(
                "{}",
                json!({ "decision": "deny", "reason": "timeout", "correlation_id": cid })
            );
            eprintln!("roost: no answer before the timeout; denied (an approval fails closed)");
            Ok(1)
        }
    }
}

/// The first handshake after setup: a Done button the human taps to prove the round trip.
pub async fn hello(api: &Api, mut a: SendArgs) -> Res {
    a.template = Some("onboarding".into());
    a.title.get_or_insert_with(|| "Your agent is ready".into());
    a.body.get_or_insert_with(|| {
        "Tap Done to confirm you received me; that proves the round trip works.".into()
    });
    let timeout = a.timeout.unwrap_or(120).max(0) as u64;
    let cid = new_cid(&mut a);
    let body = a.body(json!({}))?;
    eprintln!("roost: correlation_id={cid}");
    notify(api, &body).await?;
    eprintln!("roost: hello sent; waiting up to {timeout}s for your human to tap Done on {cid}");
    let wait = Wait {
        api,
        cid: &cid,
        timeout,
        interval: a.interval.unwrap_or(30).max(1) as u64,
        wake_on_message: false,
    };
    let (outcome, healthy) = wait.run().await?;
    match outcome {
        Outcome::Answer(chosen) => {
            print_json(&chosen);
            Ok(0)
        }
        Outcome::Cancelled => Err(super::wait::cancelled(&cid)),
        _ => {
            eprintln!(
                "roost: the handshake stays open; `roost listen --all` collects the tap whenever it comes."
            );
            Err(timed_out(&cid, timeout, healthy))
        }
    }
}

pub async fn wait(api: &Api, a: WaitArgs) -> Res {
    let timeout = a.timeout.max(0) as u64;
    let w = Wait {
        api,
        cid: &a.correlation_id,
        timeout,
        interval: a.interval.max(1) as u64,
        wake_on_message: true,
    };
    let (outcome, healthy) = w.run().await?;
    print_outcome(&a.correlation_id, outcome, timeout, healthy)
}

pub async fn cancel(api: &Api, cid: &str) -> Res {
    let path = format!(
        "/notifications/{}",
        percent_encoding::utf8_percent_encode(cid, percent_encoding::NON_ALPHANUMERIC)
    );
    let r = api.call("cancel", Method::DELETE, &path, None).await?;
    println!("{}", r.raw());
    if r.ok() {
        eprintln!("roost: cancelled {cid}; nothing will fire");
        return Ok(0);
    }
    let why = if r.status == 409 {
        "too late, it already reached the phone"
    } else {
        "unknown correlation_id?"
    };
    Err(Exit::error(format!(
        "roost: cancel failed ({}): {why}",
        r.status
    )))
}

pub async fn inbox(api: &Api, a: InboxArgs) -> Res {
    let mut q: Vec<String> = Vec::new();
    let path = if a.summary {
        "/inbox/summary"
    } else {
        if a.pending {
            q.push("pending=true".into());
        }
        if let Some(l) = &a.limit {
            q.push(format!(
                "limit={}",
                percent_encoding::utf8_percent_encode(l, percent_encoding::NON_ALPHANUMERIC)
            ));
        }
        "/notifications"
    };
    let r = api
        .expect(
            "inbox",
            Method::GET,
            &format!("{path}?{}", q.join("&")),
            None,
        )
        .await?;
    println!("{}", r.raw());
    let d = &r.json;
    if a.summary {
        eprintln!(
            "roost: {} sent, {} answered, {} open",
            d["total"], d["answered"], d["open"]
        );
    } else {
        let rows = d["notifications"].as_array().map_or(&[][..], Vec::as_slice);
        let pending = rows
            .iter()
            .filter(|r| {
                r["requires_action"] == true
                    && matches!(r["status"].as_str(), Some("pending" | "snoozed"))
            })
            .count();
        eprintln!(
            "roost: {} notification(s), {pending} waiting for an answer",
            rows.len()
        );
    }
    Ok(0)
}

/// A read-only look at the conversation, newest first: nothing is leased or acked.
pub async fn catchup(api: &Api, a: CatchupArgs) -> Res {
    let mut q = vec!["history=true".to_string(), "all=true".into()];
    if let Some(l) = a.limit {
        q.push(format!("limit={l}"));
    }
    if let Some(b) = a.before {
        q.push(format!("before={b}"));
    }
    if let Some(s) = a.since {
        q.push(format!("since={s}"));
    }
    let r = api
        .expect(
            "catchup",
            Method::GET,
            &format!("/messages?{}", q.join("&")),
            None,
        )
        .await?;
    let mut rows: Vec<Value> = r.json["messages"].as_array().cloned().unwrap_or_default();
    rows.sort_by_key(|m| std::cmp::Reverse(m["id"].as_i64().unwrap_or(0)));
    let newest = rows.first().and_then(|m| m["id"].as_i64());
    if let Some(s) = a.since {
        rows.retain(|m| m["id"].as_u64().unwrap_or(0) > s);
    }
    if let Some(l) = a.limit {
        rows.truncate(l as usize);
    }
    if a.digest {
        for m in &rows {
            let body: String = m["body"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(60)
                .collect();
            let state = if !m["processed_at"].is_null() {
                "handled"
            } else if !m["delivered_at"].is_null() {
                "delivered"
            } else {
                "new"
            };
            println!(
                "{} · {} · {body} · {state}",
                m["id"],
                m["kind"].as_str().unwrap_or("message")
            );
        }
    } else {
        print_json(&json!({ "messages": rows }));
    }
    eprintln!(
        "roost: catchup: {} message(s), read-only (not leased, not acked)",
        rows.len()
    );
    if let Some(id) = newest {
        eprintln!(
            "roost: newest is id {id}; next time `roost catchup --digest --since {id}` shows only what's new"
        );
    }
    Ok(0)
}

fn valid_datetime(s: &str) -> bool {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    DateTime::parse_from_rfc3339(s).is_ok()
        || DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M%:z").is_ok()
        || DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M%z").is_ok()
        || NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M").is_ok()
        || NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S").is_ok()
        || NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

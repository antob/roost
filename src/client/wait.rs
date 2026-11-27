//! Blocking on one notification's answer with the server's long-poll.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{Value, json};

use super::http::{Api, key_rejected};
use super::{Exit, Res, print_json};

pub enum Outcome {
    Answer(Value),
    /// The human wrote in the conversation while we waited.
    Composer(Value),
    Timeout,
    /// Cancelled before an answer: none will come.
    Cancelled,
}

pub struct Wait<'a> {
    pub api: &'a Api,
    pub cid: &'a str,
    pub timeout: u64,
    pub interval: u64,
    /// Return early when the human writes in the conversation instead of answering.
    pub wake_on_message: bool,
}

impl Wait<'_> {
    pub async fn run(&self) -> Res<(Outcome, bool)> {
        let deadline = Instant::now() + Duration::from_secs(self.timeout);
        let path = |wait: u64| {
            let mut q = vec![];
            if wait > 0 {
                q.push(format!("wait={wait}"));
            }
            if self.wake_on_message {
                q.push("wake_on_message=true".into());
            }
            let cid =
                percent_encoding::utf8_percent_encode(self.cid, percent_encoding::NON_ALPHANUMERIC);
            format!("/notifications/{cid}?{}", q.join("&"))
        };
        let mut healthy = false;
        let mut failures: u32 = 0;
        let mut told = (false, false, None::<String>);
        loop {
            let left = deadline.saturating_duration_since(Instant::now()).as_secs();
            let wait = left.min(25);
            let asked = Instant::now();
            let res = self
                .api
                .send(
                    Method::GET,
                    &path(wait),
                    None,
                    Duration::from_secs(wait + 10),
                )
                .await;
            let mut quick_retry = true;
            match res {
                Ok(r) if r.status == 200 => {
                    healthy = true;
                    failures = 0;
                    let d = &r.json;
                    if d["status"] == "cancelled" {
                        return Ok((Outcome::Cancelled, healthy));
                    } else if d["responded"] == true {
                        let chosen = d["chosen_action"].clone();
                        if d["messages_pending"] == true {
                            eprintln!(
                                "roost: the human also wrote in the conversation; read it with `roost listen` (or `roost catchup`, read-only)."
                            );
                        }
                        if chosen["kind"] == "snoozed" {
                            let until = chosen["snooze_until"].as_str().map(String::from);
                            if told.2 != until {
                                eprintln!(
                                    "roost: snoozed until {}; it fires again then, still waiting",
                                    until.as_deref().unwrap_or("later")
                                );
                                told.2 = until;
                            }
                        } else {
                            return Ok((Outcome::Answer(chosen), healthy));
                        }
                    } else if self.wake_on_message && d["messages_pending"] == true {
                        if let Some(batch) = drain_conversation(self.api).await {
                            return Ok((Outcome::Composer(batch), healthy));
                        }
                    } else if !told.0 && d["escalation"]["state"] == "fired" {
                        told.0 = true;
                        eprintln!(
                            "roost: the urgent re-alert fired and there is still no answer; keep waiting or back off"
                        );
                    }
                }
                Ok(r) if r.status == 404 => {
                    healthy = true;
                    if !told.1 {
                        told.1 = true;
                        eprintln!("roost: no notification with correlation_id {}", self.cid);
                    }
                    quick_retry = false;
                }
                Ok(r) if r.status == 401 || r.status == 403 => return Err(key_rejected()),
                Ok(r) => {
                    failures += 1;
                    eprintln!("roost: poll error {}", r.status);
                    quick_retry = false;
                }
                Err(_) => {
                    failures += 1;
                    quick_retry = false;
                }
            }
            if Instant::now() >= deadline {
                return Ok((Outcome::Timeout, healthy));
            }
            if asked.elapsed() < Duration::from_secs(2) {
                let pace = if quick_retry {
                    1
                } else {
                    (1u64 << failures.min(5)).min(self.interval.max(1))
                };
                let left = deadline.saturating_duration_since(Instant::now());
                tokio::time::sleep(Duration::from_secs(pace).min(left)).await;
            }
        }
    }
}

/// Takes the waiting conversation messages (leased, not acked) as a `human_message` batch.
pub async fn drain_conversation(api: &Api) -> Option<Value> {
    let r = api
        .send(
            Method::GET,
            "/messages?continuity=true",
            None,
            super::http::TIMEOUT,
        )
        .await
        .ok()?;
    let msgs = r.json["messages"].as_array().filter(|m| !m.is_empty())?;
    Some(json!({ "messages": msgs, "continuity_contexts": r.json["continuity_contexts"] }))
}

/// Prints the answer, or the conversation batch that interrupted the wait; exit 0 either way.
pub fn print_outcome(cid: &str, outcome: Outcome, timeout: u64, healthy: bool) -> Res {
    match outcome {
        Outcome::Answer(chosen) => {
            print_json(&chosen);
            Ok(0)
        }
        Outcome::Composer(batch) => {
            print_human_message(cid, &batch);
            Ok(0)
        }
        Outcome::Timeout => Err(timed_out(cid, timeout, healthy)),
        Outcome::Cancelled => Err(cancelled(cid)),
    }
}

pub fn cancelled(cid: &str) -> Exit {
    Exit::error(format!(
        "roost: {cid} was cancelled before an answer; none will come"
    ))
}

pub fn print_human_message(cid: &str, batch: &Value) {
    for ctx in batch["continuity_contexts"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let mut line = json!({ "type": "continuity_context" });
        if let (Some(l), Some(c)) = (line.as_object_mut(), ctx.as_object()) {
            l.extend(c.clone());
        }
        println!("{line}");
    }
    let msgs = &batch["messages"];
    let up_to = msgs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_i64())
        .max()
        .unwrap_or(0);
    print_json(&json!({
        "kind": "human_message",
        "note": format!(
            "The human wrote in the conversation while you waited; handle it first. Notification {cid} is still \
             unanswered: resume with `roost wait {cid}` afterwards."
        ),
        "pending_notification": cid,
        "messages": msgs,
    }));
    eprintln!(
        "roost: {} message(s) delivered, not done. Ack after handling them: `roost ack --up-to {up_to} --summary \"…\"` \
         (un-acked messages are served again after ~10 min).",
        msgs.as_array().map_or(0, Vec::len)
    );
}

/// Exit 3 when the channel worked and the human just hasn't answered; 4 when it never worked.
pub fn timed_out(cid: &str, timeout: u64, healthy: bool) -> Exit {
    if healthy {
        eprintln!(
            "roost: no answer on {cid} within {timeout}s. It stays open: `roost wait {cid}` resumes, \
             or the answer arrives through `roost listen --all`."
        );
        Exit::code(3)
    } else {
        eprintln!(
            "roost: no answer on {cid} within {timeout}s, and the server never answered a poll; the channel looks broken. Run `roost doctor`."
        );
        Exit::code(4)
    }
}

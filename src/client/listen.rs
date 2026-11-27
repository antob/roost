//! `listen` / `online` (the conversation queue, with an optional handler per batch), `ack`,
//! `selftest`, `presence` and the Claude Code SessionStart hook.

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{SignalKind, signal};

use super::config::{self, Config};
use super::http::{Api, TIMEOUT, key_rejected};
use super::{Exit, Res, lenient_int, print_json};

pub const WATCH: &str = "roost online --follow --ndjson --timeout 0";

#[derive(Args)]
pub struct ListenArgs {
    /// Give up after this many seconds (0 with --follow: never).
    #[arg(long, value_parser = lenient_int, default_value = "600")]
    timeout: i64,
    /// Seconds between polls when the server can't hold a request.
    #[arg(long, value_parser = lenient_int, default_value = "5")]
    interval: i64,
    /// Also answers to your notifications, selftests and other system rows, not only the human's messages.
    #[arg(long)]
    pub all: bool,
    /// Keep listening after a batch, until --timeout.
    #[arg(long)]
    follow: bool,
    /// One compact JSON object per line, each batch closed by {"type":"batch_end"}.
    #[arg(long)]
    ndjson: bool,
    /// Ack each batch as soon as it is printed instead of after the work.
    #[arg(long, conflicts_with = "exec")]
    ack_on_read: bool,
    /// Run this shell command once for the batch (JSON on stdin and in $ROOST_BATCH_FILE); exit 0 acks it.
    #[arg(long, conflicts_with_all = ["follow", "ndjson"])]
    exec: Option<String>,
    /// Kill the --exec handler after this many seconds.
    #[arg(long, value_parser = lenient_int, default_value = "1800")]
    handler_timeout: i64,
}

#[derive(Args)]
pub struct AckArgs {
    /// Ack every message you were served up to and including this id.
    #[arg(long, conflicts_with = "ids", required_unless_present = "ids")]
    up_to: Option<String>,
    /// Ack exactly these ids (comma-separated).
    #[arg(long)]
    ids: Option<String>,
    /// Keep the lease instead of acking: "still working on it".
    #[arg(long)]
    renew: bool,
    /// What you did, shown to the human next to the ✓✓.
    #[arg(long)]
    summary: Option<String>,
}

#[derive(Subcommand)]
pub enum HookCommand {
    /// claude: every Claude Code session here opens with `roost presence`. pi: the extension that does
    /// that and adds the `roost_watch` tool (.pi/extensions/roost.ts).
    Install {
        #[arg(long, value_enum, default_value = "claude")]
        target: HookTarget,
        /// For every session of this user: $CLAUDE_CONFIG_DIR/settings.json (default ~/.claude) or
        /// $PI_CODING_AGENT_DIR/extensions (default ~/.pi/agent). Sessions without a roost key stay quiet.
        #[arg(long)]
        global: bool,
    },
    /// Remove it.
    Uninstall {
        #[arg(long, value_enum, default_value = "claude")]
        target: HookTarget,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum HookTarget {
    Claude,
    Pi,
}

const PI_EXTENSION: &str = include_str!("pi/roost.ts");
const PI_MARKER: &str = "roost-extension";
const PI_CONFIG_LINE: &str = r#"const ROOST: { command: string; env: Record<string, string>; presence: string[] } = { command: "roost", env: {}, presence: ["presence"] }; // roost-config"#;

impl ListenArgs {
    pub fn kind(&self) -> &'static str {
        if self.follow && self.timeout == 0 {
            "watch"
        } else {
            "listen"
        }
    }
}

fn id_list(flag: &str, s: &str) -> Res<Vec<i64>> {
    s.split(',')
        .map(|p| {
            let p = p.trim();
            p.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| p.parse().ok())
                .flatten()
                .ok_or_else(|| {
                    Exit::usage(format!(
                        "roost: {flag} {p:?} is not a numeric message id (use the id from listen output, not a correlation_id)"
                    ))
                })
        })
        .collect()
}

/// One consumer per channel per machine: a lockfile keyed by the channel key.
struct Lock(PathBuf);

fn proc_start(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19).map(String::from)
}

impl Lock {
    fn acquire(cfg: &Config, token: &str, kind: &str) -> Res<Self> {
        let hash = crate::util::sha256_hex(token.as_bytes());
        let path = cfg.base_dir.join(format!("consumer-{}.lock", &hash[..16]));
        let me = std::process::id() as i32;
        let body = json!({
            "pid": me, "proc_started_at": proc_start(me), "started_at": crate::util::now(),
            "label": cfg.label(), "kind": kind,
        });
        config::make_private_dir(&cfg.base_dir).map_err(|e| {
            Exit::error(format!(
                "roost: can't create {}: {e}",
                cfg.base_dir.display()
            ))
        })?;
        for _ in 0..2 {
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            match created {
                Ok(mut f) => {
                    let _ = f.write_all(body.to_string().as_bytes());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder: Value = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default();
                    let pid = holder["pid"].as_i64().unwrap_or(0) as i32;
                    let alive = pid > 0
                        && kill(Pid::from_raw(pid), None).is_ok()
                        && holder["proc_started_at"].as_str().map(String::from) == proc_start(pid);
                    if alive {
                        return Err(Exit::error(format!(
                            "roost: this channel already has a consumer on this machine (pid {pid}, {} \"{}\", since {}). \
                             One channel, one consumer: read with `roost catchup` (read-only), or stop that process. \
                             If you are sure nothing is running, remove {}",
                            holder["kind"].as_str().unwrap_or("listen"),
                            holder["label"].as_str().unwrap_or("?"),
                            holder["started_at"].as_str().unwrap_or("?"),
                            path.display()
                        )));
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => {
                    return Err(Exit::error(format!(
                        "roost: can't write {}: {e}",
                        path.display()
                    )));
                }
            }
        }
        Err(Exit::error(format!(
            "roost: couldn't take {}",
            path.display()
        )))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn listen(cfg: &Config, api: &Api, a: ListenArgs) -> Res {
    let forever = a.follow && a.timeout == 0;
    if forever
        && std::env::var_os("CLAUDECODE").is_none()
        && std::env::var("ROOST_EVENT_STREAM").as_deref() != Ok("1")
    {
        return Err(Exit::usage(format!(
            "roost: `--follow --timeout 0` is the session-length watch for a harness that wakes you on this \
             process's output (Claude Code: Monitor({{command:'{WATCH}', persistent:true}})). Outside one, nothing \
             would read it: run one round in the foreground instead (`roost online`), handle it, relaunch. In Pi, use \
             the roost_watch tool (`roost hook install --target pi` adds it). A harness that does stream stdout events \
             can set ROOST_EVENT_STREAM=1."
        )));
    }
    let token = cfg.token.as_deref().unwrap_or_default();
    let _lock = Lock::acquire(cfg, token, a.kind())?;
    let child: std::sync::Arc<std::sync::Mutex<Option<i32>>> = Default::default();

    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let stop = |sig: &str, code: i32, child: Option<i32>| {
        eprintln!("roost: {sig}: stopping; nothing in flight is acked (the lease re-serves it).");
        if let Some(pid) = child {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
        }
        code
    };
    tokio::select! {
        r = rounds(api, &a, child.clone()) => r,
        _ = sigint.recv() => Ok(stop("SIGINT", 130, *child.lock().unwrap())),
        _ = sigterm.recv() => Ok(stop("SIGTERM", 143, *child.lock().unwrap())),
        () = orphan_watchdog() => Ok(0),
    }
}

/// Exits once the process that started us is gone, so an orphan never holds the queue.
async fn orphan_watchdog() {
    let parent = nix::unistd::getppid();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if nix::unistd::getppid() != parent {
            eprintln!("roost: the process that started this listener is gone; exiting.");
            return;
        }
    }
}

async fn rounds(
    api: &Api,
    a: &ListenArgs,
    child: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
) -> Res {
    let forever = a.follow && a.timeout == 0;
    let deadline =
        (!forever).then(|| Instant::now() + Duration::from_secs(a.timeout.max(0) as u64));
    let left = || {
        deadline.map_or(u64::MAX, |d| {
            d.saturating_duration_since(Instant::now()).as_secs()
        })
    };
    let mut healthy = false;
    let mut got_any = false;
    let mut failures: u32 = 0;
    if a.follow && !forever {
        eprintln!(
            "roost: --follow keeps listening until --timeout; a turn-based agent should use one-shot rounds instead."
        );
    }
    loop {
        let wait = left().min(25);
        let mut path = "/messages?continuity=true".to_string();
        if a.all {
            path.push_str("&all=true");
        }
        if wait > 0 {
            path.push_str(&format!("&wait={wait}"));
        }
        let asked = Instant::now();
        match api
            .send(Method::GET, &path, None, Duration::from_secs(wait + 10))
            .await
        {
            Ok(r) if r.status == 200 => {
                healthy = true;
                failures = 0;
                let msgs = r.json["messages"].as_array().cloned().unwrap_or_default();
                if a.exec.is_none() {
                    for ctx in r.json["continuity_contexts"]
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
                }
                if !msgs.is_empty() {
                    if let Some(cmd) = &a.exec {
                        let batch = json!({ "messages": msgs });
                        return exec_round(api, cmd, batch, a.handler_timeout.max(1) as u64, child)
                            .await;
                    }
                    print_batch(api, a, &msgs).await;
                    got_any = true;
                    if !a.follow {
                        return Ok(0);
                    }
                }
            }
            Ok(r) if r.status == 401 || r.status == 403 => return Err(key_rejected()),
            Ok(r) => {
                failures += 1;
                if r.status < 500 {
                    eprintln!("roost: listen error {}: {}", r.status, r.raw());
                }
            }
            Err(_) => failures += 1,
        }
        if left() == 0 {
            if a.follow && got_any {
                eprintln!("roost: --follow window ended; batches were delivered");
                return Ok(0);
            }
            let relaunch = if a.follow {
                ""
            } else {
                " Relaunch the listener now: listen → handle → ack → relaunch is what keeps you reachable."
            };
            if healthy {
                eprintln!(
                    "roost: no message from the human within {}s.{relaunch}",
                    a.timeout
                );
                return Err(Exit::code(3));
            }
            eprintln!(
                "roost: no message within {}s, and the server never answered; the channel looks broken. Run `roost doctor`.",
                a.timeout
            );
            return Err(Exit::code(4));
        }
        if asked.elapsed() < Duration::from_secs(2) {
            let pace = if failures == 0 {
                a.interval.max(1) as u64
            } else {
                (1u64 << failures.min(5))
                    .max(a.interval.max(1) as u64)
                    .min(60)
            };
            tokio::time::sleep(Duration::from_secs(pace.min(left().max(1)))).await;
        }
    }
}

async fn print_batch(api: &Api, a: &ListenArgs, msgs: &[Value]) {
    let ids: Vec<i64> = msgs.iter().filter_map(|m| m["id"].as_i64()).collect();
    let up_to = ids.iter().copied().max().unwrap_or(0);
    if a.ndjson {
        for m in msgs {
            let mut line = json!({ "type": m["kind"].as_str().unwrap_or("message") });
            if let (Some(l), Some(row)) = (line.as_object_mut(), m.as_object()) {
                l.extend(row.clone());
            }
            println!("{line}");
        }
        let mut end = json!({ "type": "batch_end", "count": msgs.len() });
        if !ids.is_empty() {
            end["max_ackable_id"] = up_to.into();
        }
        println!("{end}");
    } else {
        print_json(&Value::Array(msgs.to_vec()));
    }
    let _ = std::io::stdout().flush();
    for m in msgs {
        if m["kind"] == "notification_reply" {
            let said = m["text"]
                .as_str()
                .map(|t| format!(": {}", t.chars().take(120).collect::<String>()));
            eprintln!(
                "roost: answer to your notification {} (\"{}\"): {}{}",
                m["ref"]["correlation_id"].as_str().unwrap_or("?"),
                m["ref"]["title"].as_str().unwrap_or("?"),
                m["action_id"]
                    .as_str()
                    .or(m["ref"]["event_kind"].as_str())
                    .unwrap_or("?"),
                said.unwrap_or_default()
            );
        }
    }
    if a.ack_on_read {
        match api
            .send(
                Method::POST,
                "/messages/ack",
                Some(&json!({ "up_to": up_to })),
                TIMEOUT,
            )
            .await
        {
            Ok(r) if r.ok() => eprintln!(
                "roost: {} message(s) acked on read; answer with a send (reuse thread_id when present)",
                r.json["acked"].as_i64().unwrap_or(0)
            ),
            Ok(r) => eprintln!(
                "roost: WARNING: ack failed ({}); these messages will be served again",
                r.status
            ),
            Err(e) => {
                eprintln!("roost: WARNING: ack failed ({e}); these messages will be served again")
            }
        }
    } else {
        eprintln!(
            "roost: {} message(s) delivered, not done. Ack after handling them: `roost ack --up-to {up_to} --summary \"<what you did>\"` \
             (un-acked messages are served again after ~10 min).",
            msgs.len()
        );
    }
}

/// Runs the handler once for the batch; its exit code decides whether the batch is acked.
async fn exec_round(
    api: &Api,
    cmd: &str,
    batch: Value,
    timeout_s: u64,
    slot: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
) -> Res {
    let ids: Vec<i64> = batch["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_i64())
        .collect();
    eprintln!(
        "roost: listen: batch of {} message(s) → handler (its exit code decides the ack): {cmd}",
        ids.len()
    );
    let file = std::env::temp_dir().join(format!(
        "roost-batch-{}-{}.json",
        std::process::id(),
        crate::util::random_token(6)
    ));
    let batch_text = format!("{batch}\n");
    let file_ok = config::write_private(&file, &batch_text).is_ok();
    let started = Instant::now();
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    if file_ok {
        command.env("ROOST_BATCH_FILE", &file);
    }
    let dots = auto_typing();
    if dots {
        set_typing(api, AUTO_TYPING_SECS).await;
    }
    let result = match command.spawn() {
        Ok(mut child) => {
            let pid = child.id().map(|p| p as i32);
            *slot.lock().unwrap() = pid;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(batch_text.as_bytes()).await;
            }
            let r = supervise(api, &mut child, pid, &ids, timeout_s, dots).await;
            *slot.lock().unwrap() = None;
            Ok(r)
        }
        Err(e) => Err(e.to_string()),
    };
    if file_ok {
        let _ = std::fs::remove_file(&file);
    }
    if dots {
        set_typing(api, 0).await;
    }
    let secs = started.elapsed().as_secs();
    let machine = |mid_line: bool, v: Value| {
        let mut out = std::io::stdout();
        if mid_line {
            let _ = out.write_all(b"\n");
        }
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    };
    let ran = match result {
        Ok(r) => r,
        Err(e) => {
            machine(
                false,
                json!({ "type": "handler_failed", "exit": null, "reason": "spawn_error", "ids": ids }),
            );
            eprintln!(
                "roost: listen: the handler couldn't run ({e}); nothing acked, the batch is served again after the lease."
            );
            return Err(Exit::code(2));
        }
    };
    let code = ran.status.and_then(|s| s.code());
    if code == Some(0) && !ran.timed_out {
        let mut body = json!({ "ids": ids });
        if let Some(s) = &ran.summary {
            body["summary"] = s.clone().into();
        }
        match api
            .send(Method::POST, "/messages/ack", Some(&body), TIMEOUT)
            .await
        {
            Ok(r) if r.ok() => eprintln!(
                "roost: listen: acked {} message(s){}",
                r.json["acked"].as_i64().unwrap_or(0),
                ran.summary
                    .as_deref()
                    .map(|s| format!(" · summary: {s}"))
                    .unwrap_or_default()
            ),
            _ => {
                machine(ran.mid_line, json!({ "type": "ack_failed", "ids": ids }));
                eprintln!(
                    "roost: listen: the handler exited 0 but the ack didn't land; the batch is served again after the lease. \
                     Ack by hand if it must not repeat: `roost ack --ids {} --summary \"…\"`",
                    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
                );
                return Err(Exit::code(2));
            }
        }
        if ran.summary.is_none() {
            eprintln!(
                "roost: listen: the handler printed no `roost-summary: <what you did>` line, so the ack carries no note."
            );
        }
        return Ok(0);
    }
    let signal = ran.status.and_then(|s| s.signal());
    let (reason, why) = if ran.timed_out {
        (
            "timeout",
            format!("timed out (--handler-timeout {timeout_s}s)"),
        )
    } else if let Some(sig) = signal {
        ("signal", format!("killed by signal {sig}"))
    } else {
        (
            "exit",
            format!("exit {}", code.map_or("?".into(), |c| c.to_string())),
        )
    };
    let mut line = json!({ "type": "handler_failed", "exit": code, "reason": reason, "ids": ids });
    if let Some(sig) = signal {
        line["signal"] = sig.into();
    }
    machine(ran.mid_line, line);
    eprintln!(
        "roost: listen: handler {why} after {secs}s; nothing acked, the batch is served again after the lease."
    );
    Err(Exit::code(2))
}

/// Raises (or with 0 clears) the typing dots; best effort, a failure is never fatal.
async fn set_typing(api: &Api, secs: i64) {
    let body = json!({ "seconds": secs });
    let _ = api
        .send(Method::POST, "/typing", Some(&body), Duration::from_secs(5))
        .await;
}

/// `--exec` raises the dots while the handler runs unless ROOST_NO_AUTO_TYPING=1.
fn auto_typing() -> bool {
    !std::env::var("ROOST_NO_AUTO_TYPING").is_ok_and(|v| v == "1")
}

const AUTO_TYPING_SECS: i64 = 90;

pub async fn typing(api: &Api, arg: Option<&str>) -> Res {
    let secs = match arg.map(str::trim) {
        None => 60,
        Some("off" | "stop" | "0") => 0,
        Some(s) => match lenient_int(s) {
            Ok(n) if n > 0 => n,
            _ => {
                return Err(Exit::usage(format!(
                    "roost: typing takes seconds (3–300) or `off`, not {s:?}"
                )));
            }
        },
    };
    let r = api
        .call(
            "typing",
            Method::POST,
            "/typing",
            Some(&json!({ "seconds": secs })),
        )
        .await?;
    if r.status == 404 {
        return Err(Exit::error(
            "roost: this server has no typing indicator (404); update roost on the server",
        ));
    }
    if r.status == 401 || r.status == 403 {
        return Err(key_rejected());
    }
    if !r.ok() {
        return Err(Exit::error(format!(
            "roost: typing failed ({}): {}",
            r.status,
            r.raw()
        )));
    }
    println!("{}", r.raw());
    if r.json["typing"] == true {
        if r.json["clamped"] == true {
            eprintln!(
                "roost: the server clamped that to {}s (3–300)",
                r.json["seconds"]
            );
        }
        eprintln!(
            "roost: the human sees you typing for {}s, or until your next send; run it again to extend",
            r.json["seconds"]
        );
    } else {
        eprintln!("roost: typing dots cleared");
    }
    Ok(0)
}

struct Ran {
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    summary: Option<String>,
    mid_line: bool,
}

/// Tees the handler's stdout, keeps the lease alive, and enforces the timeout.
async fn supervise(
    api: &Api,
    child: &mut tokio::process::Child,
    pid: Option<i32>,
    ids: &[i64],
    timeout_s: u64,
    dots: bool,
) -> Ran {
    let mut out = child.stdout.take();
    let mut buf = vec![0u8; 8192];
    let mut line_tail = Vec::new();
    let mut ran = Ran {
        status: None,
        timed_out: false,
        summary: None,
        mid_line: false,
    };
    let mut eof = out.is_none();
    let mut renew = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let kill_at = tokio::time::sleep(Duration::from_secs(timeout_s));
    tokio::pin!(kill_at);
    let mut hard_kill: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let group = |sig: Signal| {
        if let Some(p) = pid {
            let _ = kill(Pid::from_raw(-p), sig);
        }
    };
    let take_marker = |line: &[u8], summary: &mut Option<String>| {
        let line = String::from_utf8_lossy(line);
        if let Some(rest) = line.trim().strip_prefix("roost-summary:") {
            *summary = Some(rest.trim().chars().take(1000).collect());
        }
    };
    loop {
        if ran.status.is_some() && eof {
            break;
        }
        let read = async {
            match out.as_mut() {
                Some(o) if !eof => o.read(&mut buf).await,
                _ => std::future::pending().await,
            }
        };
        let hard = async {
            match hard_kill.as_mut() {
                Some(s) => s.await,
                None => std::future::pending().await,
            }
        };
        let graced = async {
            match grace.as_mut() {
                Some(s) => s.await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            n = read => match n {
                Ok(0) | Err(_) => eof = true,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(chunk);
                    let _ = stdout.flush();
                    ran.mid_line = chunk.last() != Some(&b'\n');
                    for &b in chunk {
                        if b == b'\n' {
                            take_marker(&line_tail, &mut ran.summary);
                            line_tail.clear();
                        } else if line_tail.len() < 2048 {
                            line_tail.push(b);
                        }
                    }
                }
            },
            st = child.wait(), if ran.status.is_none() => {
                ran.status = st.ok();
                grace = Some(Box::pin(tokio::time::sleep(Duration::from_secs(2))));
            }
            _ = renew.tick(), if !ids.is_empty() => {
                if dots && ran.status.is_none() {
                    set_typing(api, AUTO_TYPING_SECS).await;
                }
                let body = json!({ "ids": ids, "state": "delivered" });
                if let Ok(r) = api.send(Method::POST, "/messages/ack", Some(&body), TIMEOUT).await
                    && !r.ok() {
                    eprintln!("roost: listen: lease renewal failed ({}); the batch may be served again early", r.status);
                }
            }
            () = &mut kill_at, if !ran.timed_out => {
                ran.timed_out = true;
                eprintln!("roost: listen: handler exceeded --handler-timeout ({timeout_s}s); SIGTERM to its process group (SIGKILL in 5s)");
                group(Signal::SIGTERM);
                hard_kill = Some(Box::pin(tokio::time::sleep(Duration::from_secs(5))));
            }
            () = hard => { group(Signal::SIGKILL); hard_kill = None; }
            () = graced => break,
        }
    }
    if !line_tail.is_empty() {
        take_marker(&line_tail, &mut ran.summary);
    }
    ran
}

pub async fn ack(api: &Api, a: AckArgs) -> Res {
    let mut body = json!({});
    if let Some(u) = &a.up_to {
        body["up_to"] = id_list("--up-to", u)?[0].into();
    } else if let Some(ids) = &a.ids {
        body["ids"] = json!(id_list("--ids", ids)?);
    }
    if a.renew {
        body["state"] = "delivered".into();
    }
    if let Some(s) = &a.summary {
        let s = s.trim();
        if s.is_empty() {
            return Err(Exit::usage(
                "roost: ack --summary needs text, e.g. --summary \"restarted the worker\"",
            ));
        }
        body["summary"] = s.chars().take(1000).collect::<String>().into();
    }
    let r = api
        .expect("ack", Method::POST, "/messages/ack", Some(&body))
        .await?;
    println!("{}", r.raw());
    let acked = r.json["acked"].as_i64().unwrap_or(0);
    if a.renew {
        eprintln!(
            "roost: lease renewed on {} message(s); ack when done",
            r.json["renewed"].as_i64().unwrap_or(0)
        );
        return Ok(0);
    }
    if acked == 0 {
        eprintln!(
            "roost: 0 acked: those messages were already handled, or aren't yours. `roost catchup --digest` shows the queue."
        );
    } else if body["summary"].is_string() {
        eprintln!("roost: {acked} message(s) handled; the human sees ✓✓ with your summary");
    } else {
        eprintln!(
            "roost: {acked} message(s) acked without --summary; next time say what you did, the human sees it."
        );
    }
    if let Some(n) = r.json["skipped"].as_i64().filter(|n| *n > 0) {
        eprintln!(
            "roost: {n} older message(s) were skipped (not served to you yet); they stay queued"
        );
    }
    let presence = api
        .send(Method::GET, "/whoami", None, Duration::from_secs(6))
        .await
        .ok()
        .and_then(|w| w.json["listening_state"].as_str().map(str::to_uppercase));
    eprintln!(
        "roost: relaunch your listener for the next round, or keep the watch up ({WATCH} under a monitor).{}",
        presence
            .map(|p| format!(" Presence right now: {p}."))
            .unwrap_or_default()
    );
    Ok(0)
}

/// Proves the loop: a nonce goes into the queue and something other than this command must ack it.
pub async fn selftest(api: &Api, window: i64) -> Res {
    let window = window.clamp(5, 600);
    let r = api
        .expect(
            "selftest",
            Method::POST,
            "/selftest",
            Some(&json!({ "window_seconds": window })),
        )
        .await?;
    let id = r.json["id"].clone();
    let granted = r.json["window_seconds"].as_i64().unwrap_or(window);
    eprintln!(
        "roost: selftest {id} sent; watching read-only for up to {granted}s. It passes only if your listener picks it up and acks it."
    );
    let deadline = Instant::now() + Duration::from_secs(granted as u64);
    let verdict = loop {
        let verdict = match api
            .send(Method::GET, &format!("/selftest/{id}"), None, TIMEOUT)
            .await
        {
            Ok(r) if r.status == 200 => Some(r.json),
            _ => None,
        };
        let status = verdict
            .as_ref()
            .and_then(|v| v["status"].as_str())
            .unwrap_or("pending");
        if status != "pending" || Instant::now() >= deadline {
            break verdict;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let status = verdict.as_ref().and_then(|v| v["status"].as_str());
    match status {
        Some("passed") => {
            eprintln!("roost: ✅ selftest passed: a listener picked the nonce up and acked it.");
            println!(
                "{}",
                json!({ "status": "passed", "id": id, "window_seconds": granted })
            );
            Ok(0)
        }
        None => {
            eprintln!(
                "roost: selftest inconclusive: its verdict couldn't be read. Check `roost doctor` and re-run."
            );
            println!(
                "{}",
                json!({ "status": "unknown", "id": id, "reason": "verdict_unreadable" })
            );
            Err(Exit::code(2))
        }
        Some(s) => {
            let live = api
                .send(Method::GET, "/whoami", None, TIMEOUT)
                .await
                .ok()
                .and_then(|w| {
                    w.json["consumers"]
                        .as_array()
                        .map(|c| c.iter().filter(|c| c["live"] == true).count())
                });
            let cause = match live {
                Some(0) => format!(
                    "nothing is listening on this channel. Start the watch (`{WATCH}` under your harness's monitor) or one `roost listen --all` round, then re-run."
                ),
                Some(n) => format!(
                    "{n} listener(s) are live but none acked the nonce: one that reads without acking, or a handler slower than --window."
                ),
                None => "the nonce wasn't acked inside the window.".into(),
            };
            eprintln!("roost: ❌ selftest failed: {cause}");
            println!(
                "{}",
                json!({ "status": s, "id": id, "consumers_live": live })
            );
            Err(Exit::code(2))
        }
    }
}

/// One line for a session's opening context: is anyone listening here? Always exit 0.
pub async fn presence(cfg: &Config, if_configured: bool) -> Res {
    if if_configured && cfg.token.is_none() {
        return Ok(0);
    }
    let who = match super::connect(cfg) {
        Ok(api) => api
            .send(Method::GET, "/whoami", None, TIMEOUT)
            .await
            .ok()
            .filter(|r| r.status == 200),
        Err(_) => None,
    };
    let Some(w) = who else {
        println!(
            "roost: channel state unknown from here (no key in this environment, or the server is unreachable); run `roost doctor`."
        );
        return Ok(0);
    };
    let d = &w.json;
    let name = d["channel"]["name"].as_str().unwrap_or("?");
    let live: Vec<String> = d["consumers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["live"] == true)
        .map(|c| {
            let label = c["label"].as_str().unwrap_or("a listener");
            c["kind"]
                .as_str()
                .map_or(label.to_string(), |k| format!("{label} [{k}]"))
        })
        .collect();
    if d["listening_state"] == "listening" {
        println!(
            "roost (channel \"{name}\"): listening: {} holds the queue. Read with `roost catchup --digest`; don't start a second listener.",
            if live.is_empty() {
                "a listener".into()
            } else {
                live.join(", ")
            }
        );
    } else if super::in_pi() {
        println!(
            "roost (channel \"{name}\"): OFFLINE: nobody is listening, so your human cannot reach you. Start the watch now \
             with the roost_watch tool (action \"start\"), then answer with `roost message` and `roost ack`."
        );
    } else {
        println!(
            "roost (channel \"{name}\"): OFFLINE: nobody is listening, so your human cannot reach you. Start the watch now: \
             Monitor({{command:'{WATCH}', description:'roost: messages from my human', persistent:true}}), then answer \
             with `roost message` and `roost ack`."
        );
    }
    Ok(0)
}

fn settings_path(cfg: &Config, global: bool) -> PathBuf {
    if global {
        return super::claude_dir().join("settings.json");
    }
    cfg.project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
        .join(".claude/settings.json")
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"].as_array().into_iter().flatten().any(|h| {
        h["command"]
            .as_str()
            .is_some_and(|c| c.contains("roost") && c.contains(" presence"))
    })
}

fn sh_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-=:".contains(&b))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The roost a session should run: `roost` when it's on PATH, else this binary, plus the env that picks this
/// identity. A global install leaves ROOST_AGENT out, so each project keeps its own identity.
fn session_roost(global: bool) -> (String, Vec<(&'static str, String)>) {
    let vars: &[&'static str] = if global {
        &["XDG_CONFIG_HOME"]
    } else {
        &["ROOST_AGENT", "XDG_CONFIG_HOME"]
    };
    let env = vars
        .iter()
        .copied()
        .filter_map(|var| {
            std::env::var(var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (var, v))
        })
        .collect();
    let on_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("roost").is_file()));
    let exe = if on_path {
        "roost".to_string()
    } else {
        std::env::current_exe().map_or("roost".into(), |p| p.to_string_lossy().into_owned())
    };
    (exe, env)
}

fn presence_args(global: bool) -> &'static [&'static str] {
    if global {
        &["presence", "--if-configured"]
    } else {
        &["presence"]
    }
}

fn hook_command(global: bool) -> String {
    let (exe, env) = session_roost(global);
    let mut parts: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", sh_quote(v)))
        .collect();
    parts.push(format!(
        "{} {}",
        sh_quote(&exe),
        presence_args(global).join(" ")
    ));
    parts.join(" ")
}

fn pi_extension_path(cfg: &Config, global: bool) -> PathBuf {
    if global {
        return super::pi_agent_dir().join("extensions/roost.ts");
    }
    cfg.project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
        .join(".pi/extensions/roost.ts")
}

/// roost's hook installed in the user's Claude Code settings, if any.
pub fn global_hook(cfg: &Config) -> Option<PathBuf> {
    let file = settings_path(cfg, true);
    read_settings(&file)
        .is_ok_and(|s| {
            s["hooks"]["SessionStart"]
                .as_array()
                .is_some_and(|l| l.iter().any(is_ours))
        })
        .then_some(file)
}

/// roost's extension installed in the user's Pi config, if any.
pub fn global_pi_extension(cfg: &Config) -> Option<PathBuf> {
    let file = pi_extension_path(cfg, true);
    is_our_pi_extension(&file).then_some(file)
}

fn is_our_pi_extension(file: &std::path::Path) -> bool {
    std::fs::read_to_string(file).is_ok_and(|s| s.contains(PI_MARKER))
}

pub fn install_pi_extension(cfg: &Config, global: bool) -> Res<Value> {
    let file = pi_extension_path(cfg, global);
    let (exe, env) = session_roost(global);
    let env: serde_json::Map<String, Value> = env
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::from(v)))
        .collect();
    let config = json!({ "command": exe, "env": env, "presence": presence_args(global) });
    let text = PI_EXTENSION.replacen(
        PI_CONFIG_LINE,
        &format!(
            "const ROOST: {{ command: string; env: Record<string, string>; presence: string[] }} = {config}; // roost-config"
        ),
        1,
    );
    let other = pi_extension_path(cfg, !global);
    if is_our_pi_extension(&other) {
        eprintln!(
            "roost: WARNING: {} also exists, and Pi would load both (two roost_watch tools). Remove one: roost hook uninstall --target pi{}",
            other.display(),
            if global { "" } else { " --global" }
        );
    }
    let fail =
        |e: std::io::Error| Exit::error(format!("roost: can't write {}: {e}", file.display()));
    let old = std::fs::read_to_string(&file).ok();
    let changed = old.as_deref() != Some(text.as_str());
    if let Some(old) = &old
        && !old.contains(PI_MARKER)
    {
        let bak = file.with_extension("ts.bak");
        std::fs::rename(&file, &bak).map_err(fail)?;
        eprintln!(
            "roost: kept your previous {} as {}",
            file.display(),
            bak.display()
        );
    }
    if changed {
        file.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(fail)?;
        std::fs::write(&file, text).map_err(fail)?;
    }
    Ok(
        json!({ "ok": true, "file": file, "target": "pi", "global": global, "command": exe, "changed": changed }),
    )
}

fn read_settings(file: &std::path::Path) -> Res<Value> {
    match std::fs::read_to_string(file) {
        Ok(s) => {
            let v: Value = serde_json::from_str(&s).map_err(|e| {
                Exit::error(format!(
                    "roost: {} is not valid JSON ({e}); fix it by hand first",
                    file.display()
                ))
            })?;
            if v.is_object() {
                Ok(v)
            } else {
                Err(Exit::error(format!(
                    "roost: {} is not a JSON object",
                    file.display()
                )))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(Exit::error(format!(
            "roost: can't read {}: {e}",
            file.display()
        ))),
    }
}

fn write_settings(file: &std::path::Path, v: &Value) -> Res<()> {
    let text = format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default());
    file.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(file, text))
        .map_err(|e| Exit::error(format!("roost: can't write {}: {e}", file.display())))
}

pub fn install_hook(cfg: &Config, global: bool) -> Res<Value> {
    let file = settings_path(cfg, global);
    let other = settings_path(cfg, !global);
    if other != file
        && read_settings(&other).is_ok_and(|s| {
            s["hooks"]["SessionStart"]
                .as_array()
                .is_some_and(|l| l.iter().any(is_ours))
        })
    {
        eprintln!(
            "roost: WARNING: {} also runs `roost presence`, so sessions would open with it twice. Remove one: roost hook uninstall{}",
            other.display(),
            if global { "" } else { " --global" }
        );
    }
    let mut settings = read_settings(&file)?;
    if !settings["hooks"].is_object() {
        settings["hooks"] = json!({});
    }
    let mut list = settings["hooks"]["SessionStart"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let command = hook_command(global);
    let entry = json!({ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 20 }] });
    let changed = match list.iter().position(is_ours) {
        Some(i) if list[i] == entry => false,
        Some(i) => {
            list[i] = entry;
            true
        }
        None => {
            list.push(entry);
            true
        }
    };
    settings["hooks"]["SessionStart"] = list.into();
    if changed {
        write_settings(&file, &settings)?;
    }
    Ok(
        json!({ "ok": true, "file": file, "hook": "SessionStart", "global": global, "command": command, "changed": changed }),
    )
}

pub fn hook(cfg: &Config, cmd: HookCommand) -> Res {
    let out = match cmd {
        HookCommand::Install {
            target: HookTarget::Claude,
            global,
        } => install_hook(cfg, global)?,
        HookCommand::Install {
            target: HookTarget::Pi,
            global,
        } => install_pi_extension(cfg, global)?,
        HookCommand::Uninstall {
            target: HookTarget::Pi,
            global,
        } => {
            let file = pi_extension_path(cfg, global);
            let ours = is_our_pi_extension(&file);
            if ours {
                std::fs::remove_file(&file).map_err(|e| {
                    Exit::error(format!("roost: can't remove {}: {e}", file.display()))
                })?;
            }
            json!({ "ok": true, "file": file, "removed": ours })
        }
        HookCommand::Uninstall {
            target: HookTarget::Claude,
            global,
        } => {
            let file = settings_path(cfg, global);
            let mut settings = read_settings(&file)?;
            let list = settings["hooks"]["SessionStart"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let kept: Vec<Value> = list.iter().filter(|e| !is_ours(e)).cloned().collect();
            let removed = kept.len() != list.len();
            if removed {
                if kept.is_empty() {
                    if let Some(h) = settings["hooks"].as_object_mut() {
                        h.remove("SessionStart");
                    }
                } else {
                    settings["hooks"]["SessionStart"] = kept.into();
                }
                write_settings(&file, &settings)?;
            }
            json!({ "ok": true, "file": file, "removed": removed })
        }
    };
    print_json(&out);
    Ok(0)
}

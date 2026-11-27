//! The agent CLI: `roost setup`, `roost ask`, `roost online`, … against a roost server.
//!
//! stdout carries machine output only; every narration line goes to stderr. Exit codes:
//! 0 ok, 1 usage, 2 error, 3 timed out waiting, 4 timed out with the channel looking broken.

mod config;
mod http;
mod listen;
mod send;
mod setup;
mod skill;
mod wait;

use std::io::Write;

use clap::Subcommand;
use reqwest::Method;

use config::Config;
use http::Api;

#[derive(Subcommand)]
pub enum Command {
    /// Claim a channel with the code from the roost app and store its key.
    Setup(setup::SetupArgs),
    /// Print the channel this key speaks for, its devices and who is listening.
    Whoami,
    /// Check the key, the server and the channel; prints one JSON line.
    Doctor {
        /// Only print warnings and errors.
        #[arg(long)]
        quiet: bool,
    },
    /// A notification the human reads when they get to it (no alert sound by default).
    #[command(alias = "fyi")]
    Message(send::SendArgs),
    /// A notification worth an alert: results, decisions, anything they should see soon.
    #[command(alias = "report")]
    Important(send::SendArgs),
    /// Interrupts the human, and re-alerts while it stays unanswered.
    #[command(alias = "alert")]
    Urgent(send::UrgentArgs),
    /// Something happening at a time (--event-at).
    Event(send::SendArgs),
    /// Send a question with buttons and block until the human answers.
    Ask(send::SendArgs),
    /// Ask for an approval (Approve with Face ID / Reject) and block for the answer.
    Approval(send::SendArgs),
    /// Gate a step on the human: exit 0 only if they tap Allow with Face ID; anything else is exit 1.
    Approve(send::ApproveArgs),
    /// The first handshake: the human taps Done to prove the round trip.
    Hello(send::SendArgs),
    /// Block until a sent notification is answered.
    Wait(send::WaitArgs),
    /// Cancel a scheduled notification before it fires.
    Cancel { correlation_id: String },
    /// List sent notifications and their answers.
    Inbox(send::InboxArgs),
    /// Read the conversation without consuming it.
    Catchup(send::CatchupArgs),
    /// Wait for the human's next messages, print them, and exit (or keep going with --follow).
    Listen(listen::ListenArgs),
    /// `listen --all`: the human's messages plus answers to your notifications. The watch is `roost online --follow --ndjson --timeout 0`.
    Online(listen::ListenArgs),
    /// Mark messages handled once the work is done.
    Ack(listen::AckArgs),
    /// Show the human the typing dots while you work on a reply (default 60 s, `off` clears them).
    Typing {
        /// Seconds (3–300), or `off`.
        seconds: Option<String>,
    },
    /// Prove a listener is really picking up messages (exit 2 if nothing acks within --window).
    Selftest {
        /// Seconds to wait for the round trip (5–600).
        #[arg(long, value_parser = lenient_int, default_value = "30")]
        window: i64,
    },
    /// One line saying whether anyone is listening on this channel (for a session's opening context).
    Presence {
        /// Say nothing when this environment has no roost key (what global hooks run).
        #[arg(long)]
        if_configured: bool,
    },
    /// Session integration: the Claude Code SessionStart hook, or the Pi extension with the `roost_watch` tool.
    Hook {
        #[command(subcommand)]
        command: listen::HookCommand,
    },
    /// The instructions agents read about roost (a Claude Code or Pi skill, or a section in AGENTS.md).
    Skill {
        #[command(subcommand)]
        command: skill::SkillCommand,
    },
}

pub struct Exit {
    pub code: i32,
    pub msg: Option<String>,
}

impl Exit {
    pub fn usage(msg: impl Into<String>) -> Self {
        Self {
            code: 1,
            msg: Some(msg.into()),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            code: 2,
            msg: Some(msg.into()),
        }
    }

    pub fn code(code: i32) -> Self {
        Self { code, msg: None }
    }
}

pub type Res<T = i32> = Result<T, Exit>;

/// Runs an agent command and returns the process exit code.
pub async fn run(cmd: Command) -> i32 {
    let result = match cmd {
        Command::Setup(args) => setup::setup(args).await,
        Command::Whoami => whoami().await,
        Command::Doctor { quiet } => setup::doctor_from_config(quiet).await,
        Command::Presence { if_configured } => {
            listen::presence(&Config::load(), if_configured).await
        }
        Command::Hook { command } => listen::hook(&Config::load(), command),
        Command::Skill { command } => skill::command(&Config::load(), command),
        Command::Listen(a) => listen_with(a).await,
        Command::Online(mut a) => {
            a.all = true;
            listen_with(a).await
        }
        cmd => match connect(&Config::load()) {
            Ok(api) => channel_command(&api, cmd).await,
            Err(e) => Err(e),
        },
    };
    let code = result.unwrap_or_else(|e| {
        if let Some(m) = e.msg {
            eprintln!("{m}");
        }
        e.code
    });
    let _ = std::io::stdout().flush();
    code
}

async fn channel_command(api: &Api, cmd: Command) -> Res {
    use send::Kind;
    let none = serde_json::json!({});
    match cmd {
        Command::Message(a) => send::typed(api, "message", a, none, Kind::Plain).await,
        Command::Important(a) => send::typed(api, "important", a, none, Kind::Plain).await,
        Command::Urgent(a) => {
            let extra = if a.escalate {
                serde_json::json!({ "escalate": true })
            } else {
                none
            };
            send::typed(api, "urgent", a.send, extra, Kind::Plain).await
        }
        Command::Event(a) => send::typed(api, "event", a, none, Kind::Plain).await,
        Command::Ask(a) => send::typed(api, "important", a, none, Kind::Ask).await,
        Command::Approval(a) => send::typed(api, "important", a, none, Kind::Approval).await,
        Command::Approve(a) => send::approve(api, a).await,
        Command::Hello(a) => send::hello(api, a).await,
        Command::Wait(a) => send::wait(api, a).await,
        Command::Cancel { correlation_id } => send::cancel(api, &correlation_id).await,
        Command::Inbox(a) => send::inbox(api, a).await,
        Command::Catchup(a) => send::catchup(api, a).await,
        Command::Ack(a) => listen::ack(api, a).await,
        Command::Selftest { window } => listen::selftest(api, window).await,
        Command::Typing { seconds } => listen::typing(api, seconds.as_deref()).await,
        _ => unreachable!("dispatched in run"),
    }
}

async fn listen_with(a: listen::ListenArgs) -> Res {
    let cfg = Config::load();
    let api = connect(&cfg)?.with_kind(a.kind());
    listen::listen(&cfg, &api, a).await
}

/// Numbers the way agents write them: `30`, `30s`, ` 30 ` all mean 30 (parseInt semantics).
pub fn lenient_int(s: &str) -> Result<i64, String> {
    let t = s.trim_start();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end]
        .parse::<i64>()
        .map(|n| sign * n)
        .map_err(|_| format!("{s:?} is not a number of seconds"))
}

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(Into::into).unwrap_or_default()
}

/// Claude Code's user config: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> std::path::PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home().join(".claude"), Into::into)
}

/// Pi's user config: `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn pi_agent_dir() -> std::path::PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home().join(".pi/agent"), Into::into)
}

/// True inside the Pi coding agent, which marks its child processes.
pub fn in_pi() -> bool {
    std::env::var("PI_CODING_AGENT").as_deref() == Ok("true")
        || std::env::var("AI_AGENT").as_deref() == Ok("pi")
}

pub fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// The configured identity and a client for it, or the no-identity exit.
fn connect(cfg: &Config) -> Res<Api> {
    let Some(token) = cfg.token.as_deref() else {
        return Err(Exit::usage(no_token_message(
            cfg,
            "roost: no identity here.",
        )));
    };
    let Some(url) = cfg.url.as_deref() else {
        return Err(Exit::usage(format!(
            "roost: a key is configured but no server URL. Set ROOST_URL, or add ROOST_URL=… to {}.",
            cfg.file().display()
        )));
    };
    Ok(Api::new(cfg, url, Some(token)))
}

fn no_token_message(cfg: &Config, prefix: &str) -> String {
    let with_env = |sub: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(cfg.base_dir.join(sub))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("env").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let mut hints = Vec::new();
    let agents = with_env("agents");
    if !agents.is_empty() {
        let ids: Vec<String> = agents.iter().map(|a| format!("ROOST_AGENT={a}")).collect();
        hints.push(format!(
            "this machine has per-agent identities; if one is yours, set it for every command: {}",
            ids.join(" · ")
        ));
    }
    let projects = with_env("projects").len();
    if projects > 0 && cfg.project_root.is_none() {
        hints.push(format!(
            "{projects} project-scoped identit(ies) exist; run roost from inside the project you set up"
        ));
    }
    let how = "ROOST_URL and ROOST_TOKEN, or run `roost setup --claim <code> --url <server>` with a claim \
               code from the roost app (channel → ⚙ → New claim code).";
    if hints.is_empty() {
        format!("{prefix} Set {how}")
    } else {
        format!("{prefix} {}. Otherwise, set {how}", hints.join("; "))
    }
}

async fn whoami() -> Res {
    let cfg = Config::load();
    let api = connect(&cfg)?;
    let r = api.expect("whoami", Method::GET, "/whoami", None).await?;
    print_json(&r.json);
    let d = &r.json;
    eprintln!(
        "roost: you are channel \"{}\" · {} device(s) · {}",
        d["channel"]["name"].as_str().unwrap_or("?"),
        d["devices"].as_i64().unwrap_or(0),
        d["listening_state"].as_str().unwrap_or("unknown")
    );
    Ok(0)
}

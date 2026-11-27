//! `roost setup --claim` and `roost doctor`.

use clap::Args;
use reqwest::Method;
use serde_json::json;

use super::config::{self, Config};
use super::http::Api;
use super::{Exit, Res, connect, no_token_message};

#[derive(Args)]
pub struct SetupArgs {
    /// The claim code from the roost app.
    #[arg(long, allow_hyphen_values = true)]
    claim: String,
    /// The roost server, e.g. https://roost.example.net. Defaults to ROOST_URL or the stored one.
    #[arg(long)]
    url: Option<String>,
    /// Store the key in the machine-wide file even inside a git project.
    #[arg(long)]
    global: bool,
    /// Overwrite a stored key that still works.
    #[arg(long)]
    force: bool,
    /// Print `export ROOST_URL=… ROOST_TOKEN=…` instead of storing the key. Not for agents: the key would land in their context.
    #[arg(long)]
    print: bool,
    /// How this agent runs: turn_based (default), persistent or external_daemon. Advisory, shown to the human.
    #[arg(long, value_parser = ["turn_based", "persistent", "external_daemon", "always_on"])]
    listen_mode: Option<String>,
    /// Don't install the Claude Code SessionStart hook or the Pi extension.
    #[arg(long)]
    no_hook: bool,
    /// Only print warnings and errors.
    #[arg(long)]
    quiet: bool,
}

pub async fn setup(a: SetupArgs) -> Res {
    let note = |m: &str| {
        if !a.quiet {
            eprintln!("{m}");
        }
    };
    let mut cfg = Config::load();
    // Decide the identity's home before claiming: the fingerprint that binds the claim hashes it.
    if cfg.agent.is_some() {
        if a.global {
            return Err(Exit::usage(
                "roost: --global conflicts with ROOST_AGENT; unset one (ROOST_AGENT is already its own scope).",
            ));
        }
    } else {
        cfg.dir = match (&cfg.project_dir, a.global) {
            (Some(p), false) => p.clone(),
            _ => cfg.base_dir.clone(),
        };
    }
    let target = cfg.read_file();
    let base = a
        .url
        .clone()
        .or_else(|| std::env::var("ROOST_URL").ok().filter(|u| !u.is_empty()))
        .or_else(|| target.get("ROOST_URL").cloned())
        .map(|u| u.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            Exit::usage("roost: which server? Pass --url https://your-roost (or set ROOST_URL).")
        })?;

    if !a.print && !a.force {
        if let Some(old) = target.get("ROOST_TOKEN") {
            let guard_base = target.get("ROOST_URL").unwrap_or(&base);
            let api = Api::new(&cfg, guard_base, Some(old));
            let owner = match api
                .send(Method::GET, "/whoami", None, super::http::TIMEOUT)
                .await
            {
                Ok(r) if r.status == 200 => r.json["channel"]["name"]
                    .as_str()
                    .map(|n| format!("channel \"{n}\"")),
                Ok(r) if r.status == 401 => None,
                Ok(r) => Some(format!("a channel (the server answered {})", r.status)),
                Err(_) => Some("a channel (its server is unreachable)".into()),
            };
            if let Some(owner) = owner {
                let how = if cfg.project_scoped() {
                    "To point this project at the new channel, re-run with --force. A second agent in the same \
                     directory should use ROOST_AGENT=<id> for this and every later command."
                } else {
                    "Run setup from inside your project's git checkout (each project gets its own key), or give \
                     this agent ROOST_AGENT=<id> for this and every later command. To replace the shared key \
                     anyway, re-run with --force."
                };
                return Err(Exit::error(format!(
                    "roost: {} already holds the key of {owner}; overwriting it would make anything reading \
                     that file send as the new channel. {how} The claim code was not used.",
                    cfg.file().display()
                )));
            }
        }
        // Prove the home is writable before the single-use code is spent.
        let probe = cfg.dir.join(format!(".write-probe-{}", std::process::id()));
        if let Err(e) = config::make_private_dir(&cfg.dir)
            .and_then(|()| config::write_private(&probe, "ok"))
            .and_then(|()| std::fs::remove_file(&probe))
        {
            return Err(Exit::error(format!(
                "roost: {} is not writable from here ({e}); the claim code was not used. In a sandbox that \
                 only allows writes inside the project, export XDG_CONFIG_HOME=\"$PWD/.roost\" (and gitignore \
                 it) for this and every later command.",
                cfg.dir.display()
            )));
        }
    }

    let api = Api::new(&cfg, &base, None);
    let r = api
        .call(
            "claim",
            Method::POST,
            "/claim",
            Some(&json!({ "code": a.claim })),
        )
        .await
        .map_err(|e| Exit::error(format!("{} (is {base} right?)", e.msg.unwrap_or_default())))?;
    if r.status == 404 {
        return Err(Exit::error(
            "roost: claim code unknown, expired (15 min) or already used; ask your human for a fresh one.",
        ));
    }
    let Some(key) = r.json["key"].as_str().filter(|_| r.ok()) else {
        return Err(Exit::error(format!(
            "roost: claim failed ({}): {}",
            r.status,
            r.raw()
        )));
    };
    let base = r.json["base_url"]
        .as_str()
        .unwrap_or(&base)
        .trim_end_matches('/')
        .to_string();
    let channel = r.json["channel"]["name"]
        .as_str()
        .unwrap_or("?")
        .to_string();
    let api = Api::new(&cfg, &base, Some(key));
    if let Some(id) = r.json["channel"]["id"].as_i64() {
        declare_contract(&api, id, a.listen_mode.as_deref()).await;
    }
    note(
        "roost: this claim rotated the channel key; any other install holding the old one now gets 401.",
    );

    if a.print {
        println!("export ROOST_URL={base}");
        println!("export ROOST_TOKEN={key}");
        eprintln!(
            "roost: channel \"{channel}\": nothing stored. Put both lines in this agent's launch environment."
        );
        super::skill::after_setup(&cfg, &note);
        return doctor(&api, "fresh claim (printed, not stored)", a.quiet).await;
    }

    let file = cfg.file();
    config::make_private_dir(&cfg.dir)
        .and_then(|()| {
            config::write_private(&file, &format!("ROOST_URL={base}\nROOST_TOKEN={key}\n"))
        })
        .map_err(|e| {
            Exit::error(format!(
                "roost: claimed channel \"{channel}\" but could not store its key in {} ({e}). \
                 Ask for a fresh code once that is fixed.",
                file.display()
            ))
        })?;
    let scope = match (&cfg.agent, &cfg.project_root) {
        (Some(id), _) => {
            format!("; agent \"{id}\": every later roost command needs ROOST_AGENT={id}")
        }
        (None, Some(root)) if cfg.project_scoped() => {
            format!("; scoped to this project ({})", root.display())
        }
        _ => {
            "; the machine-wide key: every agent here without its own sends as this channel".into()
        }
    };
    note(&format!(
        "roost: channel \"{channel}\" set up; key stored in {} (mode 600, never printed){scope}",
        file.display()
    ));
    let owned = api
        .call(
            "ownership",
            Method::POST,
            "/claim/ownership",
            Some(&json!({ "fingerprint": api.fingerprint(), "label": cfg.label() })),
        )
        .await;
    if let Ok(r) = owned
        && r.status == 200
    {
        note(&format!(
            "roost: ownership claimed as \"{}\" (generation {})",
            cfg.label(),
            r.json["claim"]["claim_generation"]
        ));
    }
    super::skill::after_setup(&cfg, &note);
    note("roost: next: `roost hello` sends a first handshake your human confirms with one tap.");
    let claude_global = super::listen::global_hook(&cfg);
    let pi_global = super::listen::global_pi_extension(&cfg);
    if let Some(f) = claude_global
        .as_ref()
        .filter(|_| std::env::var_os("CLAUDECODE").is_some())
    {
        note(&format!(
            "roost: SessionStart hook installed globally ({}); none added here",
            f.display()
        ));
    }
    if let Some(f) = pi_global.as_ref().filter(|_| super::in_pi()) {
        note(&format!(
            "roost: Pi extension installed globally ({}); none added here",
            f.display()
        ));
    }
    if std::env::var_os("CLAUDECODE").is_some() && !a.no_hook && claude_global.is_none() {
        match super::listen::install_hook(&cfg, false) {
            Ok(h) => note(&format!(
                "roost: SessionStart hook in {}: each Claude Code session opens with `roost presence` (remove: roost hook uninstall)",
                h["file"].as_str().unwrap_or("?")
            )),
            Err(e) => eprintln!(
                "roost: SessionStart hook not installed ({}); run `roost hook install` later",
                e.msg.unwrap_or_default()
            ),
        }
    }
    if super::in_pi() && !a.no_hook && pi_global.is_none() {
        match super::listen::install_pi_extension(&cfg, false) {
            Ok(h) => note(&format!(
                "roost: Pi extension in {}: sessions here open with `roost presence`, and the roost_watch tool keeps you reachable (trust the project in Pi once; remove: roost hook uninstall --target pi)",
                h["file"].as_str().unwrap_or("?")
            )),
            Err(e) => eprintln!(
                "roost: Pi extension not installed ({}); run `roost hook install --target pi` later",
                e.msg.unwrap_or_default()
            ),
        }
    }
    doctor(&api, &file.display().to_string(), a.quiet).await
}

async fn declare_contract(api: &Api, channel_id: i64, mode: Option<&str>) {
    let mode = mode.unwrap_or("turn_based");
    let contract = json!({ "listen_mode": mode, "keep_connection_alive": mode != "turn_based" });
    let r = api
        .call(
            "contract",
            Method::PATCH,
            &format!("/channels/{channel_id}"),
            Some(&json!({ "operating_contract": contract })),
        )
        .await;
    match r {
        Ok(r) if r.ok() => {
            eprintln!("roost: declared listen_mode={mode} (advisory; the human sees it)")
        }
        Ok(r) => eprintln!(
            "roost: note: couldn't declare the operating contract ({})",
            r.status
        ),
        Err(_) => eprintln!("roost: note: couldn't declare the operating contract (network)"),
    }
}

pub async fn doctor_from_config(quiet: bool) -> Res {
    let cfg = Config::load();
    if cfg.token.is_none() {
        return Err(Exit::error(no_token_message(&cfg, "roost doctor: no key.")));
    }
    let api = connect(&cfg)?;
    doctor(&api, &cfg.token_source(), quiet).await
}

/// Checks the key against the server and prints one JSON line; exit 0 healthy, 2 broken.
async fn doctor(api: &Api, source: &str, quiet: bool) -> Res {
    let note = |m: String| {
        if !quiet {
            eprintln!("{m}");
        }
    };
    let mut warnings: Vec<&str> = Vec::new();
    note(format!("roost doctor: key from {source}, never displayed"));
    note(format!("roost doctor: server {}", api.base));
    let r = api
        .send(Method::GET, "/whoami", None, super::http::TIMEOUT)
        .await
        .map_err(|e| Exit::error(format!("roost doctor: server unreachable: {e}")))?;
    if r.status == 401 || r.status == 403 {
        return Err(super::http::key_rejected());
    }
    if r.status != 200 || r.json["channel"].is_null() {
        println!("{}", json!({ "ok": false, "status": r.status }));
        return Err(Exit::error(format!(
            "roost doctor: unexpected {} from /whoami: {}",
            r.status,
            r.raw()
        )));
    }
    let d = &r.json;
    let devices = d["devices"].as_i64().unwrap_or(0);
    note(format!(
        "roost doctor: key valid · channel \"{}\" · {devices} device(s)",
        d["channel"]["name"].as_str().unwrap_or("?")
    ));
    if devices == 0 {
        eprintln!(
            "roost doctor: WARNING: no devices. Sends reach nobody until the human enables notifications in the roost app."
        );
        warnings.push("no_devices");
    }
    let history = api
        .send(
            Method::GET,
            "/messages?history=true",
            None,
            super::http::TIMEOUT,
        )
        .await;
    if let Ok(h) = history
        && h.status == 200
    {
        let pending = h.json["messages"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|m| m["processed_at"].is_null() && m["consumed_at"].is_null())
                    .count()
            })
            .unwrap_or(0);
        if pending > 0 {
            eprintln!(
                "roost doctor: WARNING: {pending} message(s) from the human are not acked yet. \
                 Read them with `roost listen` (or `roost catchup`, read-only)."
            );
            warnings.push("unacked_messages");
        }
    }
    println!(
        "{}",
        json!({
            "ok": true,
            "base_url": api.base,
            "channel": d["channel"]["name"],
            "devices": devices,
            "listening_state": d["listening_state"],
            "warnings": warnings.len(),
            "warning_kinds": warnings,
        })
    );
    Ok(0)
}

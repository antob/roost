//! Where the agent's key lives and who the agent says it is.
//!
//! Read precedence: `ROOST_AGENT` → `agents/<id>/env`; else, when no `ROOST_TOKEN` is in the
//! environment and this git project has one, `projects/<hash>/env`; else the shared `env`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::util::sha256_hex;

pub struct Config {
    pub base_dir: PathBuf,
    pub agent: Option<String>,
    pub project_root: Option<PathBuf>,
    pub project_dir: Option<PathBuf>,
    /// The identity's home: `env`, `fp-salt` and state live here.
    pub dir: PathBuf,
    pub env_token: bool,
    pub url: Option<String>,
    pub token: Option<String>,
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn base_dir() -> PathBuf {
    env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("roost")
}

/// The first ancestor holding a `.git` entry (a dir, or a file in a worktree); `$HOME` never counts.
fn project_root() -> Option<PathBuf> {
    let home = std::env::home_dir();
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return (Some(&dir) != home.as_ref()).then_some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn read_env_file(file: &Path) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(file) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            if line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            (!k.is_empty() && !v.is_empty()).then(|| (k.trim().to_string(), v.to_string()))
        })
        .collect()
}

impl Config {
    pub fn load() -> Self {
        let base_dir = base_dir();
        let agent = env_var("ROOST_AGENT").map(|a| {
            a.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .take(64)
                .collect::<String>()
        });
        let project_root = project_root();
        let project_dir = project_root.as_ref().map(|root| {
            let hash = sha256_hex(root.to_string_lossy().as_bytes());
            base_dir.join("projects").join(&hash[..16])
        });
        let env_token = env_var("ROOST_TOKEN").is_some();
        let dir = match (&agent, &project_dir) {
            (Some(a), _) => base_dir.join("agents").join(a),
            (None, Some(p)) if !env_token && p.join("env").exists() => p.clone(),
            _ => base_dir.clone(),
        };
        let mut cfg = Self {
            base_dir,
            agent,
            project_root,
            project_dir,
            dir,
            env_token,
            url: None,
            token: None,
        };
        let file = cfg.read_file();
        cfg.url = env_var("ROOST_URL")
            .or_else(|| file.get("ROOST_URL").cloned())
            .map(|u| u.trim_end_matches('/').to_string());
        cfg.token = env_var("ROOST_TOKEN").or_else(|| file.get("ROOST_TOKEN").cloned());
        cfg
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("env")
    }

    pub fn read_file(&self) -> HashMap<String, String> {
        read_env_file(&self.file())
    }

    pub fn project_scoped(&self) -> bool {
        self.agent.is_none() && self.project_dir.as_ref() == Some(&self.dir)
    }

    /// Where the key came from, for doctor's narration.
    pub fn token_source(&self) -> String {
        if self.env_token {
            return "ROOST_TOKEN env var".into();
        }
        let scope = match &self.agent {
            Some(a) => format!(" (ROOST_AGENT={a})"),
            None if self.project_scoped() => " (this project)".into(),
            None => " (shared)".into(),
        };
        format!("{}{scope}", self.file().display())
    }

    /// `fp_` + sha256(host|user|agent|env file|salt), stable per identity home and unguessable.
    pub fn fingerprint(&self) -> String {
        let material = [
            hostname(),
            username(),
            self.agent.clone().unwrap_or_default(),
            self.file().to_string_lossy().into_owned(),
            self.salt(),
        ]
        .join("|");
        format!("fp_{}", &sha256_hex(material.as_bytes())[..24])
    }

    fn salt(&self) -> String {
        let file = self.dir.join("fp-salt");
        if let Ok(s) = fs::read_to_string(&file) {
            return s.trim().to_string();
        }
        let minted = crate::util::random_token(32);
        let saved =
            make_private_dir(&self.dir).and_then(|()| write_private(&file, &format!("{minted}\n")));
        // An unsaved salt would change every run, so fall back to none.
        if saved.is_ok() { minted } else { String::new() }
    }

    pub fn label(&self) -> String {
        env_var("ROOST_LABEL")
            .or_else(|| self.agent.clone())
            .unwrap_or_else(hostname)
            .chars()
            .take(80)
            .collect()
    }

    pub fn encoded_label(&self) -> String {
        utf8_percent_encode(&self.label(), NON_ALPHANUMERIC).to_string()
    }
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn username() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_default()
}

pub fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

pub fn write_private(file: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, file)
}

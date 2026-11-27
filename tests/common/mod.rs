//! A live roost on a random port plus helpers to drive the roost agent CLI against it.
#![allow(dead_code)]

use std::process::Output;
use std::time::Duration;

use roost::server::{AppState, Config, channels, connect, router, scheduler};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tokio::process::Command;

pub const ADMIN: &str = "test-admin-token";

pub struct Roost {
    pub base: String,
    pub db: SqlitePool,
    pub tmp: TempDir,
    pub channel_id: i64,
    pub http: reqwest::Client,
}

impl Roost {
    pub async fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("work")).unwrap();
        let db = connect(tmp.path().join("roost.db").to_str().unwrap())
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Passkeys need a domain name, so use localhost rather than the bare IP.
        let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
        let config = Config {
            base_url: base.clone(),
            admin_token: Some(ADMIN.into()),
            callback_secret: Some("test-secret".into()),
            timezone: "Europe/Stockholm".into(),
            vapid_private_pem: None,
            vapid_subject: None,
        };
        let state = AppState::new(db.clone(), config).await.unwrap();
        scheduler::spawn(state.clone());
        let app = router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (channel_id, _) = channels::create(&db, "test channel").await.unwrap();
        Self {
            base,
            db,
            tmp,
            channel_id,
            http: reqwest::Client::new(),
        }
    }

    pub fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_roost"));
        cmd.args(args)
            .current_dir(self.tmp.path().join("work"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.tmp.path())
            .env("XDG_CONFIG_HOME", self.tmp.path().join(".config"))
            .kill_on_drop(true);
        // The Nix build sandbox has no /etc/ssl; reqwest needs a CA bundle to start.
        if let Some(certs) = std::env::var_os("SSL_CERT_FILE") {
            cmd.env("SSL_CERT_FILE", certs);
        }
        cmd
    }

    pub async fn run(&self, args: &[&str]) -> Output {
        let out = tokio::time::timeout(Duration::from_secs(60), self.cli(args).output())
            .await
            .expect("the CLI timed out")
            .unwrap();
        eprintln!(
            "$ roost {}\n[exit {:?}]\n--- stdout\n{}\n--- stderr\n{}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    pub async fn setup(&self) {
        let code = channels::mint_claim_code(&self.db, self.channel_id)
            .await
            .unwrap()
            .code;
        let out = self
            .run(&[
                "setup",
                "--claim",
                &code,
                "--url",
                &self.base,
                "--global",
                "--no-hook",
            ])
            .await;
        assert!(out.status.success(), "setup failed");
        let env =
            std::fs::read_to_string(self.tmp.path().join(".config/roost").join("env")).unwrap();
        assert!(
            env.contains("ROOST_TOKEN=rst_"),
            "env file has no key: {env}"
        );
        assert!(env.contains(&format!("ROOST_URL={}", self.base)));
        let skill = self.tmp.path().join("work/.claude/skills/roost/SKILL.md");
        assert!(
            skill.exists(),
            "setup wrote no skill at {}",
            skill.display()
        );
    }

    pub async fn wait_for_notification(&self, cid: &str) {
        for _ in 0..200 {
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE correlation_id = ?")
                    .bind(cid)
                    .fetch_one(&self.db)
                    .await
                    .unwrap();
            if n > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("notification {cid} never arrived");
    }

    pub async fn answer(&self, cid: &str, body: Value) -> Value {
        let res = self.try_answer(cid, body).await;
        assert!(
            res.status().is_success(),
            "respond failed: {}",
            res.status()
        );
        res.json().await.unwrap()
    }

    pub async fn try_answer(&self, cid: &str, body: Value) -> reqwest::Response {
        self.http
            .post(format!(
                "{}/api/human/channels/{}/notifications/{cid}/respond",
                self.base, self.channel_id
            ))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Polls `roost inbox` until `cid`'s row satisfies `done`, returning that row.
    pub async fn inbox_until(&self, cid: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        for _ in 0..20 {
            let out = self.run(&["inbox"]).await;
            assert!(out.status.success(), "inbox failed");
            if let Some(row) = stdout_json(&out)["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["correlation_id"] == cid)
            {
                if done(row) {
                    return row.clone();
                }
                last = row.clone();
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("{cid} never reached {what}; last row: {last:#}");
    }

    pub async fn human_says(&self, text: &str) -> i64 {
        let res = self
            .http
            .post(format!(
                "{}/api/human/channels/{}/messages",
                self.base, self.channel_id
            ))
            .bearer_auth(ADMIN)
            .json(&json!({ "body": text }))
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success());
        res.json::<Value>().await.unwrap()["id"].as_i64().unwrap()
    }
}

pub fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e})"))
}

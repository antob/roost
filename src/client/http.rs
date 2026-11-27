//! The channel-key HTTP client: bearer auth plus the advisory identity headers on every call.

use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use super::config::Config;
use super::{Exit, Res};

pub const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Api {
    pub base: String,
    token: Option<String>,
    fingerprint: String,
    label: String,
    kind: Option<&'static str>,
    http: reqwest::Client,
}

pub struct Resp {
    pub status: u16,
    pub text: String,
    pub json: Value,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as received, for commands that pass the server's answer straight through.
    pub fn raw(&self) -> &str {
        self.text.trim_end()
    }
}

impl Api {
    pub fn new(cfg: &Config, base: &str, token: Option<&str>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.map(String::from),
            fingerprint: cfg.fingerprint(),
            label: cfg.encoded_label(),
            kind: None,
            http: reqwest::Client::new(),
        }
    }

    /// What a consuming command is (`listen` or `watch`), so whoami can tell consumers apart.
    pub fn with_kind(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Resp, reqwest::Error> {
        let mut req = self
            .http
            .request(method, format!("{}/api/v1{path}", self.base))
            .timeout(timeout)
            .header("x-roost-fingerprint", &self.fingerprint)
            .header("x-roost-label", &self.label);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(k) = self.kind {
            req = req.header("x-roost-consumer-kind", k);
        }
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        let text = res.text().await?;
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Resp { status, text, json })
    }

    /// A call whose network failure ends the command with exit 2, naming `what` failed.
    pub async fn call(
        &self,
        what: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Res<Resp> {
        self.send(method, path, body, TIMEOUT)
            .await
            .map_err(|e| Exit::error(format!("roost: {what} failed (network): {e}")))
    }

    /// Like `call`, but any non-2xx status is also exit 2.
    pub async fn expect(
        &self,
        what: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Res<Resp> {
        let r = self.call(what, method, path, body).await?;
        if r.status == 401 || r.status == 403 {
            return Err(key_rejected());
        }
        if !r.ok() {
            return Err(Exit::error(format!(
                "roost: {what} failed ({}): {}",
                r.status,
                r.raw()
            )));
        }
        Ok(r)
    }
}

pub fn key_rejected() -> Exit {
    Exit::error(
        "roost: the server rejected this channel key (revoked, or the channel was deleted). \
         Ask your human for a fresh claim code and run `roost setup --claim <code>`.",
    )
}

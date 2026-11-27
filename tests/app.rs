//! The owner side: Web Push delivery to a fake push service, passkeys, and the Face ID gate.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{Roost, stdout_json};
use reqwest::header::{COOKIE, SET_COOKIE};
use serde_json::{Value, json};
use url::Url;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

type Received = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;

struct FakePushService {
    base: String,
    received: Received,
    keys: Box<dyn ece::LocalKeyPair>,
    auth: [u8; 16],
}

impl FakePushService {
    /// `/push/gone` answers 410 like an expired subscription, `/push/hang` never answers; anything else is accepted.
    async fn start() -> Self {
        let received: Received = Arc::default();
        let log = received.clone();
        let app = axum::Router::new().route(
            "/push/{id}",
            axum::routing::post(
                move |Path(id): Path<String>, headers: HeaderMap, body: axum::body::Bytes| {
                    let log = log.clone();
                    async move {
                        let gone = id == "gone";
                        if id == "hang" {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                        log.lock().unwrap().push((id, headers, body.to_vec()));
                        if gone {
                            StatusCode::GONE
                        } else {
                            StatusCode::CREATED
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (keys, auth) = ece::generate_keypair_and_auth_secret().unwrap();
        Self {
            base,
            received,
            keys,
            auth,
        }
    }

    async fn subscribe(&self, r: &Roost, id: &str) {
        let public = self.keys.raw_components().unwrap().public_key().to_vec();
        sqlx::query(
            "INSERT INTO push_subscriptions (endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, '2026-01-01T00:00:00Z')",
        )
        .bind(format!("{}/push/{id}", self.base))
        .bind(URL_SAFE_NO_PAD.encode(public))
        .bind(URL_SAFE_NO_PAD.encode(self.auth))
        .execute(&r.db)
        .await
        .unwrap();
    }

    fn decrypt(&self, body: &[u8]) -> Value {
        let plain = ece::decrypt(&self.keys.raw_components().unwrap(), &self.auth, body).unwrap();
        serde_json::from_slice(&plain).unwrap()
    }

    /// Waits for the n-th push to `id` and returns its headers and decrypted payload.
    async fn nth(&self, id: &str, n: usize) -> (HeaderMap, Value) {
        for _ in 0..100 {
            let hit = self
                .received
                .lock()
                .unwrap()
                .iter()
                .filter(|(i, _, _)| i == id)
                .nth(n)
                .map(|(_, h, b)| (h.clone(), b.clone()));
            if let Some((headers, body)) = hit {
                return (headers, self.decrypt(&body));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("push #{n} to {id} never arrived");
    }
}

#[tokio::test]
async fn a_hung_push_endpoint_does_not_hold_up_the_send() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "hang").await;
    push.subscribe(&r, "phone").await;
    r.setup().await;

    let started = std::time::Instant::now();
    let sent = r
        .run(&[
            "important",
            "--title",
            "Build done",
            "--correlation-id",
            "hang-1",
        ])
        .await;
    assert!(sent.status.success());
    assert!(
        started.elapsed() < roost::server::HTTP_TIMEOUT + Duration::from_secs(5),
        "the send waited {:?}",
        started.elapsed()
    );
    push.nth("phone", 0).await;
    let row = r
        .inbox_until("hang-1", "delivered", |n| !n["delivered_at"].is_null())
        .await;
    assert_eq!(row["status"], "pending");
}

#[tokio::test]
async fn push_reaches_a_subscribed_device() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    r.setup().await;

    let sent = r
        .run(&[
            "important",
            "--title",
            "Deploy",
            "--body",
            "v2 is out",
            "--correlation-id",
            "p-1",
        ])
        .await;
    assert!(sent.status.success());
    assert_eq!(stdout_json(&sent)["registered_devices"], 1);
    assert!(!String::from_utf8_lossy(&sent.stderr).contains("0 registered devices"));

    let (headers, payload) = push.nth("phone", 0).await;
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["urgency"], "high");
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("vapid t=")
    );
    assert_eq!(payload["title"], "Deploy");
    assert_eq!(payload["body"], "v2 is out");
    assert_eq!(payload["channel"], "test channel");
    assert!(payload["url"].as_str().unwrap().starts_with("/n/"));
}

#[tokio::test]
async fn escalation_re_pushes_and_reports_fired() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    sqlx::query("UPDATE channels SET escalate_after_secs = 1 WHERE id = ?")
        .bind(r.channel_id)
        .execute(&r.db)
        .await
        .unwrap();
    r.setup().await;

    assert!(
        r.run(&[
            "urgent",
            "--title",
            "Server down",
            "--correlation-id",
            "u-1"
        ])
        .await
        .status
        .success()
    );
    push.nth("phone", 0).await;
    let (_, again) = push.nth("phone", 1).await;
    assert_eq!(again["title"], "🚨 Still waiting: Server down");
    r.inbox_until("u-1", "fired", |n| n["escalation"]["state"] == "fired")
        .await;
}

#[tokio::test]
async fn gone_subscriptions_are_pruned() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "gone").await;
    r.setup().await;

    let sent = r.run(&["message", "--title", "Hello"]).await;
    push.nth("gone", 0).await;
    assert_eq!(
        stdout_json(&sent)["registered_devices"],
        0,
        "the 410 prunes it before the 201 is built"
    );
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions")
        .fetch_one(&r.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

struct Owner<'a> {
    r: &'a Roost,
    key: SoftPasskey,
    cookie: Option<String>,
}

impl<'a> Owner<'a> {
    fn new(r: &'a Roost) -> Self {
        Self {
            r,
            key: SoftPasskey::new(true),
            cookie: None,
        }
    }

    fn origin(&self) -> Url {
        Url::parse(&self.r.base).unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        let mut req = self
            .r
            .http
            .post(format!("{}/api/human{path}", self.r.base))
            .json(&body);
        if let Some(c) = &self.cookie {
            req = req.header(COOKIE, c);
        }
        req.send().await.unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        let mut req = self.r.http.get(format!("{}/api/human{path}", self.r.base));
        if let Some(c) = &self.cookie {
            req = req.header(COOKIE, c);
        }
        req.send().await.unwrap()
    }

    fn keep_cookie(&mut self, res: &reqwest::Response) {
        let raw = res.headers()[SET_COOKIE].to_str().unwrap();
        assert!(raw.contains("HttpOnly") && raw.contains("SameSite=Strict"));
        self.cookie = Some(raw.split(';').next().unwrap().to_string());
    }

    async fn enroll(&mut self, code: &str) -> StatusCode {
        let start = self.post("/enroll/start", json!({ "code": code })).await;
        if !start.status().is_success() {
            return start.status();
        }
        let start: Value = start.json().await.unwrap();
        let options: CreationChallengeResponse =
            serde_json::from_value(start["options"].clone()).unwrap();
        let credential = self.key.do_registration(self.origin(), options).unwrap();
        let res = self
            .post("/enroll/finish", json!({ "challenge_id": start["challenge_id"], "credential": credential, "label": "test" }))
            .await;
        if res.status().is_success() {
            self.keep_cookie(&res);
        }
        res.status()
    }

    async fn assertion(&mut self, start: Value) -> Value {
        let options: RequestChallengeResponse =
            serde_json::from_value(start["options"].clone()).unwrap();
        let credential = self.key.do_authentication(self.origin(), options).unwrap();
        json!({ "challenge_id": start["challenge_id"], "credential": credential })
    }
}

#[tokio::test]
async fn passkey_enroll_login_and_gated_approval() {
    let r = Roost::start().await;
    let mut owner = Owner::new(&r);

    assert_eq!(owner.get("/feed").await.status(), StatusCode::UNAUTHORIZED);
    let session: Value = owner.get("/session").await.json().await.unwrap();
    assert_eq!(session["authenticated"], false);
    assert_eq!(session["enrolled"], false);

    let (code, _) = roost::server::owner::mint_enrollment_code(&r.db)
        .await
        .unwrap();
    assert_eq!(owner.enroll(&code).await, StatusCode::OK);
    let session: Value = owner.get("/session").await.json().await.unwrap();
    assert_eq!(session["authenticated"], true);

    let mut other = Owner::new(&r);
    assert_eq!(
        other.enroll(&code).await,
        StatusCode::FORBIDDEN,
        "codes are single use"
    );

    owner.cookie = None;
    let start: Value = owner
        .post("/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let assertion = owner.assertion(start).await;
    let login = owner.post("/login/finish", assertion.clone()).await;
    assert_eq!(login.status(), StatusCode::OK);
    owner.keep_cookie(&login);
    assert_eq!(
        owner.post("/login/finish", assertion).await.status(),
        StatusCode::FORBIDDEN,
        "challenges are single use"
    );

    r.setup().await;
    let waiter = tokio::spawn(
        r.cli(&[
            "approve",
            "--title",
            "Deploy prod?",
            "--correlation-id",
            "g-1",
            "--timeout",
            "60",
        ])
        .output(),
    );
    r.wait_for_notification("g-1").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM notifications WHERE correlation_id = 'g-1'")
        .fetch_one(&r.db)
        .await
        .unwrap();

    let item: Value = owner
        .get(&format!("/notifications/{id}"))
        .await
        .json()
        .await
        .unwrap();
    let allow = item["action_specs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "allow")
        .unwrap();
    assert_eq!(allow["biometric"], true);
    let feed: Value = owner.get("/feed").await.json().await.unwrap();
    assert_eq!(feed["notifications"][0]["correlation_id"], "g-1");

    let ungated = owner
        .post(
            &format!("/notifications/{id}/respond"),
            json!({ "action_id": "allow" }),
        )
        .await;
    assert_eq!(
        ungated.status(),
        StatusCode::FORBIDDEN,
        "a session must pass Face ID"
    );

    let deny_gate = owner
        .post(
            &format!("/notifications/{id}/gate"),
            json!({ "action_id": "allow" }),
        )
        .await;
    let deny_gate = owner.assertion(deny_gate.json().await.unwrap()).await;
    let wrong_action = owner
        .post(
            &format!("/notifications/{id}/respond"),
            json!({ "action_id": "deny", "gate": deny_gate }),
        )
        .await;
    assert_eq!(
        wrong_action.status(),
        StatusCode::OK,
        "deny is not gated, so no gate is checked"
    );

    let out = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "deny exits 1");
}

#[tokio::test]
async fn gated_allow_succeeds_with_face_id() {
    let r = Roost::start().await;
    let mut owner = Owner::new(&r);
    let (code, _) = roost::server::owner::mint_enrollment_code(&r.db)
        .await
        .unwrap();
    assert_eq!(owner.enroll(&code).await, StatusCode::OK);
    r.setup().await;

    let waiter = tokio::spawn(
        r.cli(&[
            "approve",
            "--title",
            "Wire money?",
            "--correlation-id",
            "g-2",
            "--timeout",
            "60",
        ])
        .output(),
    );
    r.wait_for_notification("g-2").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM notifications WHERE correlation_id = 'g-2'")
        .fetch_one(&r.db)
        .await
        .unwrap();

    let start: Value = owner
        .post(
            &format!("/notifications/{id}/gate"),
            json!({ "action_id": "allow" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let gate = owner.assertion(start).await;
    let res = owner
        .post(
            &format!("/notifications/{id}/respond"),
            json!({ "action_id": "allow", "gate": gate }),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    let out = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(out.status.success(), "allow exits 0");
}

#[tokio::test]
async fn event_stream_signals_channel_changes() {
    let r = Roost::start().await;
    r.setup().await;
    let url = format!("{}/api/human/events", r.base);
    assert_eq!(
        r.http.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let mut stream = r
        .http
        .get(&url)
        .bearer_auth(common::ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    assert!(
        r.run(&["message", "--title", "Live?"])
            .await
            .status
            .success()
    );

    let event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let chunk = stream.chunk().await.unwrap().expect("stream ended");
            let text = String::from_utf8_lossy(&chunk).to_string();
            if text.contains("event: changed") {
                return text;
            }
        }
    })
    .await
    .expect("no change event");
    assert!(
        event.contains(&format!("\"channel_id\":{}", r.channel_id)),
        "{event}"
    );
}

#[tokio::test]
async fn no_push_for_the_channel_the_owner_is_viewing() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    r.setup().await;
    let viewing = |channel: Value| {
        r.http
            .post(format!("{}/api/human/viewing", r.base))
            .bearer_auth(common::ADMIN)
            .json(&json!({ "channel_id": channel }))
            .send()
    };

    assert!(
        viewing(json!(r.channel_id))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let sent = r
        .run(&[
            "important",
            "--title",
            "Seen live",
            "--correlation-id",
            "v-1",
        ])
        .await;
    assert!(sent.status.success());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        push.received.lock().unwrap().is_empty(),
        "no banner while the conversation is open"
    );
    let delivered: Option<String> =
        sqlx::query_scalar("SELECT delivered_at FROM notifications WHERE correlation_id = 'v-1'")
            .fetch_one(&r.db)
            .await
            .unwrap();
    assert!(delivered.is_some(), "shown in the app counts as delivered");

    assert!(viewing(Value::Null).await.unwrap().status().is_success());
    assert!(
        r.run(&[
            "important",
            "--title",
            "Away now",
            "--correlation-id",
            "v-2"
        ])
        .await
        .status
        .success()
    );
    let (_, payload) = push.nth("phone", 0).await;
    assert_eq!(payload["title"], "Away now");
}

#[tokio::test]
async fn no_push_while_the_owner_watches_home() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    r.setup().await;
    let viewing = |body: Value| {
        r.http
            .post(format!("{}/api/human/viewing", r.base))
            .bearer_auth(common::ADMIN)
            .json(&body)
            .send()
    };

    let home = viewing(json!({ "channel_id": null, "home": true }))
        .await
        .unwrap();
    assert!(home.status().is_success());
    assert!(
        r.run(&["important", "--title", "On Home"])
            .await
            .status
            .success()
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        push.received.lock().unwrap().is_empty(),
        "Home shows every channel's new items live"
    );

    let settings = viewing(json!({ "channel_id": null })).await.unwrap();
    assert!(settings.status().is_success());
    assert!(
        r.run(&["important", "--title", "On Settings"])
            .await
            .status
            .success()
    );
    let (_, payload) = push.nth("phone", 0).await;
    assert_eq!(payload["title"], "On Settings");
}

#[tokio::test]
async fn deleting_a_channel_removes_its_data_and_revokes_the_key() {
    let r = Roost::start().await;
    r.setup().await;
    assert!(
        r.run(&["important", "--title", "Soon gone"])
            .await
            .status
            .success()
    );
    r.human_says("bye").await;

    let url = format!("{}/api/human/channels/{}", r.base, r.channel_id);
    let res = r
        .http
        .delete(&url)
        .bearer_auth(common::ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = r
        .http
        .delete(&url)
        .bearer_auth(common::ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM notifications) + (SELECT COUNT(*) FROM messages) + (SELECT COUNT(*) FROM claim_codes)",
    )
    .fetch_one(&r.db)
    .await
    .unwrap();
    assert_eq!(
        left, 0,
        "notifications, messages and claim codes go with the channel"
    );
    assert!(
        !r.run(&["whoami"]).await.status.success(),
        "the agent's key no longer works"
    );
}

#[tokio::test]
async fn an_event_pushes_a_reminder_at_its_lead_time() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    r.setup().await;

    let at = (chrono::Utc::now() + chrono::Duration::seconds(15 * 60 + 7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let sent = r
        .run(&[
            "event",
            "--title",
            "Standup",
            "--event-at",
            &at,
            "--correlation-id",
            "ev-1",
        ])
        .await;
    assert!(sent.status.success());
    assert!(stdout_json(&sent)["remind_at"].is_string());
    assert!(String::from_utf8_lossy(&sent.stderr).contains("starting soon"));

    let (_, first) = push.nth("phone", 0).await;
    assert_eq!(first["title"], "Standup");
    let (headers, reminder) = push.nth("phone", 1).await;
    assert_eq!(reminder["title"], "⏰ In 15 min: Standup");
    assert_ne!(
        reminder["tag"], first["tag"],
        "a reminder gets its own banner"
    );
    assert_eq!(headers["urgency"], "high");

    let naive = r
        .run(&[
            "event",
            "--title",
            "Lunch",
            "--event-at",
            "2099-06-26T12:00",
        ])
        .await;
    assert!(naive.status.success());
    assert!(stdout_json(&naive)["remind_at"].is_null());
    assert!(String::from_utf8_lossy(&naive.stderr).contains("no reminder"));
}

#[tokio::test]
async fn a_collapse_key_replaces_the_previous_banner_and_feed_item() {
    let r = Roost::start().await;
    let push = FakePushService::start().await;
    push.subscribe(&r, "phone").await;
    r.setup().await;

    for step in ["Build 3/5", "Build 4/5"] {
        let sent = r
            .run(&["message", "--title", step, "--collapse-key", "build"])
            .await;
        assert!(sent.status.success());
    }
    let (h1, p1) = push.nth("phone", 0).await;
    let (h2, p2) = push.nth("phone", 1).await;
    let topic = h1["topic"].to_str().unwrap();
    assert_eq!(topic.len(), 32);
    assert_eq!(h2["topic"], topic);
    assert_eq!(p1["tag"], p2["tag"]);
    assert_eq!(p2["badge"], 1, "the superseded item no longer counts");

    let human = |path: &str| {
        r.http
            .get(format!("{}/api/human{path}", r.base))
            .bearer_auth(common::ADMIN)
            .send()
    };
    let feed: Value = human("/feed").await.unwrap().json().await.unwrap();
    let titles: Vec<&str> = feed["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Build 4/5"]);
    let badge: Value = human("/badge").await.unwrap().json().await.unwrap();
    assert_eq!(badge["open"], 1);
}

#[tokio::test]
async fn the_owner_sets_a_channels_look_and_urgency_ceiling() {
    let r = Roost::start().await;
    r.setup().await;
    let patch = |body: Value| {
        r.http
            .patch(format!("{}/api/human/channels/{}", r.base, r.channel_id))
            .bearer_auth(common::ADMIN)
            .json(&body)
            .send()
    };
    let res = patch(json!({
        "name": "Laptop", "color": "#112233", "icon": "bolt",
        "max_urgency": "normal", "escalate_after_secs": 120,
    }))
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ch: Value = res.json().await.unwrap();
    assert_eq!(ch["name"], "Laptop");
    assert_eq!(ch["color"], "#112233");
    assert_eq!(ch["escalate_after_secs"], 120);

    for bad in [
        json!({ "color": "red" }),
        json!({ "name": " " }),
        json!({ "max_urgency": "loud" }),
        json!({ "escalate_after_secs": 5 }),
    ] {
        assert_eq!(patch(bad).await.unwrap().status(), StatusCode::BAD_REQUEST);
    }
    let cleared: Value = patch(json!({ "color": null }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["color"], Value::Null);
    assert_eq!(cleared["icon"], "bolt", "a missing field stays as it was");

    let sent = r.run(&["urgent", "--title", "Disk full"]).await;
    let body = stdout_json(&sent);
    assert_eq!(body["degrade_reason"], "channel_max_urgency");
    assert_eq!(body["escalation"]["state"], "disabled_by_channel");
    assert!(String::from_utf8_lossy(&r.run(&["whoami"]).await.stdout).contains("Laptop"));
}

//! Drives the roost agent CLI against a live roost server.

mod common;

use std::time::Duration;

use common::{Roost, stdout_json};
use serde_json::json;

#[tokio::test]
async fn setup_whoami_and_typed_send() {
    let r = Roost::start().await;
    r.setup().await;

    let who = r.run(&["whoami"]).await;
    assert!(who.status.success());
    assert!(String::from_utf8_lossy(&who.stdout).contains("test channel"));

    let sent = r
        .run(&[
            "important",
            "--title",
            "Build finished",
            "--body",
            "all green",
        ])
        .await;
    assert!(sent.status.success());
    let body = stdout_json(&sent);
    assert_eq!(body["status"], "pending");
    assert_eq!(body["profile"], "important");
    assert!(body["correlation_id"].as_str().is_some());
    assert_eq!(body["nobody_listening"], true);
    assert!(String::from_utf8_lossy(&sent.stderr).contains("nobody is listening"));
}

#[tokio::test]
async fn ask_blocks_until_answer() {
    let r = Roost::start().await;
    r.setup().await;

    let child = r
        .cli(&[
            "ask",
            "--title",
            "Deploy?",
            "--actions",
            "yes,no",
            "--timeout",
            "60",
            "--correlation-id",
            "ask-1",
        ])
        .output();
    let waiter = tokio::spawn(child);
    r.wait_for_notification("ask-1").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.answer("ask-1", json!({ "action_id": "yes" })).await;

    let out = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
    let chosen = stdout_json(&out);
    assert_eq!(chosen["kind"], "acted");
    assert_eq!(chosen["action_id"], "yes");
}

#[tokio::test]
async fn approve_is_deny_default() {
    let r = Roost::start().await;
    r.setup().await;

    for (cid, action, success) in [("ap-allow", "allow", true), ("ap-deny", "deny", false)] {
        let waiter = tokio::spawn(
            r.cli(&[
                "approve",
                "Ship it?",
                "--timeout",
                "60",
                "--correlation-id",
                cid,
            ])
            .output(),
        );
        r.wait_for_notification(cid).await;
        r.answer(cid, json!({ "action_id": action })).await;
        let out = tokio::time::timeout(Duration::from_secs(30), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.status.success(), success, "approve with {action}");
    }
}

#[tokio::test]
async fn listen_delivers_and_ack_processes() {
    let r = Roost::start().await;
    r.setup().await;

    let id = r.human_says("please rerun the migration").await;
    let args: &[&str] = &["listen", "--timeout", "20"];
    let out = r.run(args).await;
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("please rerun the migration"));

    let acked = r
        .run(&["ack", "--up-to", &id.to_string(), "--summary", "reran it"])
        .await;
    assert!(acked.status.success());
    let processed: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM messages WHERE id = ?")
            .bind(id)
            .fetch_one(&r.db)
            .await
            .unwrap();
    assert!(processed.is_some());
}

#[tokio::test]
async fn scheduled_send_can_be_cancelled() {
    let r = Roost::start().await;
    r.setup().await;

    let later = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let sent = r
        .run(&[
            "message",
            "--title",
            "Later",
            "--deliver-at",
            &later,
            "--correlation-id",
            "sched-1",
        ])
        .await;
    assert!(sent.status.success());
    assert_eq!(stdout_json(&sent)["status"], "scheduled");

    assert!(r.run(&["cancel", "sched-1"]).await.status.success());
    assert!(!r.run(&["cancel", "no-such-cid"]).await.status.success());
}

#[tokio::test]
async fn a_held_listen_wakes_on_a_new_message() {
    let r = Roost::start().await;
    r.setup().await;

    let waiter = tokio::spawn(r.cli(&["listen", "--timeout", "30"]).output());
    tokio::time::sleep(Duration::from_secs(2)).await;
    r.human_says("are you there?").await;
    let out = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("are you there?"));
}

#[tokio::test]
async fn ask_keeps_waiting_through_a_snooze() {
    let r = Roost::start().await;
    r.setup().await;

    let waiter = tokio::spawn(
        r.cli(&[
            "ask",
            "--title",
            "Review?",
            "--actions",
            "approve,reject",
            "--timeout",
            "60",
            "--correlation-id",
            "sn-1",
        ])
        .output(),
    );
    r.wait_for_notification("sn-1").await;
    let snoozed = r
        .answer(
            "sn-1",
            json!({ "action_id": "snooze", "snooze_seconds": 1 }),
        )
        .await;
    assert_eq!(snoozed["kind"], "snoozed");
    let refired = r
        .inbox_until("sn-1", "a snooze re-fire", |n| n["status"] == "pending")
        .await;
    assert_eq!(refired["chosen_action"]["kind"], "snoozed");
    assert!(!waiter.is_finished(), "ask must not return on a snooze");

    r.answer("sn-1", json!({ "action_id": "approve" })).await;
    let out = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["action_id"], "approve");
}

#[tokio::test]
async fn scheduled_send_fires_at_deliver_at() {
    let r = Roost::start().await;
    r.setup().await;

    let soon = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339();
    let sent = r
        .run(&[
            "message",
            "--title",
            "Soon",
            "--deliver-at",
            &soon,
            "--correlation-id",
            "sched-2",
        ])
        .await;
    assert!(sent.status.success());
    assert_eq!(stdout_json(&sent)["status"], "scheduled");

    r.inbox_until("sched-2", "delivery", |n| n["status"] == "pending")
        .await;
    assert!(
        !r.run(&["cancel", "sched-2"]).await.status.success(),
        "a fired notification can no longer be cancelled"
    );
}

#[tokio::test]
async fn simultaneous_answers_count_once() {
    let r = Roost::start().await;
    r.setup().await;
    let sent = r
        .run(&[
            "important",
            "--actions",
            "yes,no",
            "--title",
            "Ship it?",
            "--correlation-id",
            "race-1",
        ])
        .await;
    assert!(sent.status.success());

    let answers = (0..10).map(|i| {
        let action = if i % 2 == 0 { "yes" } else { "no" };
        r.try_answer("race-1", json!({ "action_id": action }))
    });
    let codes: Vec<u16> = futures_util::future::join_all(answers)
        .await
        .iter()
        .map(|res| res.status().as_u16())
        .collect();
    assert_eq!(codes.iter().filter(|c| **c < 300).count(), 1, "{codes:?}");
    assert!(codes.iter().all(|c| *c < 300 || *c == 409), "{codes:?}");

    let replies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE kind = 'notification_reply' AND correlation_id = 'race-1'",
    )
    .fetch_one(&r.db)
    .await
    .unwrap();
    assert_eq!(replies, 1);
}

#[tokio::test]
async fn a_wait_ends_when_its_send_is_cancelled() {
    let r = Roost::start().await;
    r.setup().await;
    let later = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let sent = r
        .run(&[
            "important",
            "--title",
            "Standup",
            "--actions",
            "done",
            "--deliver-at",
            &later,
            "--correlation-id",
            "cx-1",
        ])
        .await;
    assert!(sent.status.success());

    let started = std::time::Instant::now();
    let (waited, cancelled) = tokio::join!(r.run(&["wait", "cx-1", "--timeout", "60"]), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        r.run(&["cancel", "cx-1"]).await
    });
    assert!(cancelled.status.success());
    assert_eq!(waited.status.code(), Some(2));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "waited {:?}",
        started.elapsed()
    );
    assert!(String::from_utf8_lossy(&waited.stderr).contains("was cancelled"));

    // A wait started after the cancel ends at once.
    let out = r.run(&["wait", "cx-1", "--timeout", "60"]).await;
    assert_eq!(out.status.code(), Some(2));
}

#[tokio::test]
async fn setup_inside_pi_installs_the_skill_and_extension() {
    let r = Roost::start().await;
    let code = roost::server::channels::mint_claim_code(&r.db, r.channel_id)
        .await
        .unwrap()
        .code;
    let out = r
        .cli(&["setup", "--claim", &code, "--url", &r.base, "--global"])
        .env("PI_CODING_AGENT", "true")
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let work = r.tmp.path().join("work");

    let skill = std::fs::read_to_string(work.join(".agents/skills/roost/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: roost\n"));
    let online =
        std::fs::read_to_string(work.join(".agents/skills/roost/references/stay-online.md"))
            .unwrap();
    assert!(online.contains("roost_watch"));

    let ext = std::fs::read_to_string(work.join(".pi/extensions/roost.ts")).unwrap();
    assert!(ext.contains("roost-extension"));
    assert!(ext.contains("registerTool"));
    let config = ext
        .lines()
        .find(|l| l.ends_with("// roost-config"))
        .expect("config line");
    assert!(config.contains("\"XDG_CONFIG_HOME\""), "{config}");
    assert!(
        !work.join(".claude/settings.json").exists(),
        "no Claude hook outside Claude Code"
    );

    let presence = r
        .cli(&["presence"])
        .env("PI_CODING_AGENT", "true")
        .output()
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&presence.stdout);
    assert!(
        said.contains("OFFLINE") && said.contains("roost_watch"),
        "{said}"
    );

    let out = r.run(&["hook", "uninstall", "--target", "pi"]).await;
    assert_eq!(stdout_json(&out)["removed"], true);
    assert!(!work.join(".pi/extensions/roost.ts").exists());
}

#[tokio::test]
async fn skill_and_session_integration_install_globally() {
    let r = Roost::start().await;
    let home = r.tmp.path();
    let claude_dir = home.join("claude-config");
    let run = |args: &'static [&'static str]| {
        let mut cmd = r.cli(args);
        cmd.env("CLAUDE_CONFIG_DIR", &claude_dir)
            .env("ROOST_AGENT", "bot");
        async move { cmd.output().await.unwrap() }
    };

    assert!(
        run(&["skill", "install", "--global"])
            .await
            .status
            .success()
    );
    assert!(claude_dir.join("skills/roost/SKILL.md").exists());
    assert!(
        run(&["skill", "install", "--target", "pi", "--global"])
            .await
            .status
            .success()
    );
    assert!(
        home.join(".pi/agent/skills/roost/references/stay-online.md")
            .exists()
    );
    let agents = run(&["skill", "install", "--target", "agents", "--global"]).await;
    assert_eq!(agents.status.code(), Some(1));

    let hook = run(&["hook", "install", "--global"]).await;
    assert!(hook.status.success());
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();
    let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.ends_with(" presence --if-configured"), "{command}");
    assert!(
        !command.contains("ROOST_AGENT"),
        "a global hook keeps each project's identity: {command}"
    );
    assert!(!command.contains("XDG_CONFIG_HOME"), "{command}");

    assert!(
        run(&["hook", "install", "--target", "pi", "--global"])
            .await
            .status
            .success()
    );
    let ext = std::fs::read_to_string(home.join(".pi/agent/extensions/roost.ts")).unwrap();
    let config = ext
        .lines()
        .find(|l| l.ends_with("// roost-config"))
        .unwrap();
    assert!(
        config.contains(r#""presence":["presence","--if-configured"]"#),
        "{config}"
    );
    assert!(!config.contains("ROOST_AGENT"), "{config}");
    assert!(!config.contains("XDG_CONFIG_HOME"), "{config}");

    let project = run(&["hook", "install", "--target", "pi"]).await;
    assert!(String::from_utf8_lossy(&project.stderr).contains("Pi would load both"));

    // Without a key, a global hook's presence says nothing.
    let quiet = r.run(&["presence", "--if-configured"]).await;
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
    let loud = r.run(&["presence"]).await;
    assert!(String::from_utf8_lossy(&loud.stdout).contains("state unknown"));

    let gone = run(&["hook", "uninstall", "--global"]).await;
    assert_eq!(stdout_json(&gone)["removed"], true);
    let gone = run(&["hook", "uninstall", "--target", "pi", "--global"]).await;
    assert_eq!(stdout_json(&gone)["removed"], true);
    assert!(!home.join(".pi/agent/extensions/roost.ts").exists());
}

#[tokio::test]
async fn setup_adds_no_project_copies_of_a_global_install() {
    let r = Roost::start().await;
    for args in [
        ["skill", "install", "--target", "pi", "--global"],
        ["hook", "install", "--target", "pi", "--global"],
    ] {
        assert!(r.run(&args).await.status.success());
    }
    let code = roost::server::channels::mint_claim_code(&r.db, r.channel_id)
        .await
        .unwrap()
        .code;
    let out = r
        .cli(&["setup", "--claim", &code, "--url", &r.base])
        .env("PI_CODING_AGENT", "true")
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("Pi skill installed globally"), "{said}");
    assert!(said.contains("Pi extension installed globally"), "{said}");
    let work = r.tmp.path().join("work");
    assert!(!work.join(".agents/skills/roost").exists());
    assert!(!work.join(".pi/extensions/roost.ts").exists());
}

#[tokio::test]
async fn early_answer_resolves_a_scheduled_send() {
    let r = Roost::start().await;
    r.setup().await;

    let later = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let sent = r
        .run(&[
            "important",
            "--title",
            "Pay rent",
            "--actions",
            "done",
            "--deliver-at",
            &later,
            "--correlation-id",
            "early-1",
        ])
        .await;
    assert!(sent.status.success());
    r.answer("early-1", json!({ "action_id": "done" })).await;

    let out = r.run(&["wait", "early-1", "--timeout", "10"]).await;
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["kind"], "completed");
    let row = r
        .inbox_until("early-1", "responded", |n| n["responded"] == true)
        .await;
    assert_eq!(row["early_resolution"], true);
}

#[tokio::test]
async fn urgent_escalation_fires_when_unanswered() {
    let r = Roost::start().await;
    r.setup().await;
    sqlx::query("UPDATE channels SET escalate_after_secs = 1 WHERE id = ?")
        .bind(r.channel_id)
        .execute(&r.db)
        .await
        .unwrap();

    let sent = r
        .run(&[
            "urgent",
            "--title",
            "Server down",
            "--correlation-id",
            "esc-1",
        ])
        .await;
    assert!(sent.status.success());
    assert!(String::from_utf8_lossy(&sent.stderr).contains("re-alerts if unanswered"));
    let esc = &stdout_json(&sent)["escalation"];
    assert_eq!(esc["state"], "armed");
    assert_eq!(esc["after_minutes"], 1);
    assert!(esc["fires_at"].is_string());

    // No device can take the re-push yet, so the fuse reports it honestly.
    r.inbox_until("esc-1", "a fired fuse", |n| {
        n["escalation"]["state"] == "fired_not_pushed"
    })
    .await;

    let sent = r
        .run(&[
            "urgent",
            "--title",
            "Disk full",
            "--correlation-id",
            "esc-2",
        ])
        .await;
    assert!(sent.status.success());
    r.answer("esc-2", json!({ "action_id": "done" })).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let row = r.inbox_until("esc-2", "defused", |_| true).await;
    assert_eq!(row["escalation"]["state"], "defused");
}

#[tokio::test]
async fn escalate_re_alerts_after_a_minute() {
    let r = Roost::start().await;
    r.setup().await;
    let sent = r
        .run(&[
            "urgent",
            "--escalate",
            "--title",
            "Prod is down",
            "--correlation-id",
            "esc-3",
        ])
        .await;
    assert!(sent.status.success());
    let esc = &stdout_json(&sent)["escalation"];
    assert_eq!(esc["state"], "armed");
    assert_eq!(esc["after_minutes"], 1);

    let sent = r
        .run(&[
            "urgent",
            "--title",
            "Also down",
            "--correlation-id",
            "esc-4",
        ])
        .await;
    assert_eq!(stdout_json(&sent)["escalation"]["after_minutes"], 5);
}

#[tokio::test]
async fn inbox_counts_what_waits_for_an_answer() {
    let r = Roost::start().await;
    r.setup().await;
    for (cid, actions) in [
        ("in-1", Some("yes,no")),
        ("in-2", Some("yes,no")),
        ("in-3", None),
    ] {
        let mut args = vec!["important", "--title", cid, "--correlation-id", cid];
        if let Some(a) = actions {
            args.extend(["--actions", a]);
        }
        assert!(r.run(&args).await.status.success());
    }
    r.answer("in-2", json!({ "action_id": "yes" })).await;

    let out = r.run(&["inbox"]).await;
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("3 notification(s), 1 waiting for an answer"),
        "{err}"
    );

    let out = r.run(&["inbox", "--summary"]).await;
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("3 sent, 1 answered, 2 open"), "{err}");
}

#[tokio::test]
async fn a_listener_is_visible_and_selftest_passes() {
    let r = Roost::start().await;
    r.setup().await;

    let listener = tokio::spawn(
        r.cli(&["listen", "--all", "--ack-on-read", "--timeout", "40"])
            .output(),
    );
    let mut who = serde_json::Value::Null;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        who = stdout_json(&r.run(&["whoami"]).await);
        if who["listening_state"] == "listening" {
            break;
        }
    }
    assert_eq!(who["listening_state"], "listening", "{who:#}");
    assert!(
        who["consumers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["live"] == true)
    );

    let st = r.run(&["selftest", "--window", "20"]).await;
    assert!(st.status.success(), "selftest should pass with a listener");
    assert_eq!(stdout_json(&st)["status"], "passed");
    let out = tokio::time::timeout(Duration::from_secs(30), listener)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("selftest"));
}

#[tokio::test]
async fn selftest_fails_with_nobody_listening() {
    let r = Roost::start().await;
    r.setup().await;
    let st = r.run(&["selftest", "--window", "5"]).await;
    assert_eq!(st.status.code(), Some(2));
    assert_eq!(stdout_json(&st)["status"], "failed");
    assert_eq!(
        stdout_json(&r.run(&["whoami"]).await)["listening_state"],
        "offline"
    );
}

#[tokio::test]
async fn listen_exec_acks_with_the_handler_summary() {
    let r = Roost::start().await;
    r.setup().await;
    let id = r.human_says("rotate the logs").await;
    let handler = format!(
        "grep -q 'rotate the logs' && echo handled && echo '{}: rotated them'",
        "roost-summary"
    );
    let out = r
        .run(&["listen", "--timeout", "20", "--exec", &handler])
        .await;
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("handled"));
    let summary: Option<String> = sqlx::query_scalar(
        "SELECT handler_summary FROM messages WHERE id = ? AND processed_at IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(&r.db)
    .await
    .unwrap()
    .flatten();
    assert_eq!(summary.as_deref(), Some("rotated them"));
}

#[tokio::test]
async fn a_failing_exec_handler_leaves_the_batch_unacked() {
    let r = Roost::start().await;
    r.setup().await;
    let id = r.human_says("this will fail").await;
    let out = r
        .run(&[
            "listen",
            "--timeout",
            "20",
            "--exec",
            "cat >/dev/null; exit 7",
        ])
        .await;
    assert_eq!(out.status.code(), Some(2));
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .expect("a handler_failed line");
    assert_eq!(line["type"], "handler_failed");
    assert_eq!(line["exit"], 7);
    assert_eq!(line["ids"], json!([id]));
    let processed: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM messages WHERE id = ?")
            .bind(id)
            .fetch_one(&r.db)
            .await
            .unwrap();
    assert!(processed.is_none());
}

#[tokio::test]
async fn one_consumer_per_channel_and_ndjson_batches() {
    let r = Roost::start().await;
    r.setup().await;
    let first = tokio::spawn(r.cli(&["listen", "--ndjson", "--timeout", "20"]).output());
    let mut who = serde_json::Value::Null;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        who = stdout_json(&r.run(&["whoami"]).await);
        if who["listening_state"] == "listening" {
            break;
        }
    }
    assert_eq!(who["listening_state"], "listening", "{who:#}");

    let second = r.run(&["listen", "--timeout", "5"]).await;
    assert_eq!(
        second.status.code(),
        Some(2),
        "a second listener must be refused"
    );

    let id = r.human_says("hello there").await;
    let out = tokio::time::timeout(Duration::from_secs(30), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(out.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every ndjson line is JSON"))
        .collect();
    let msg = lines
        .iter()
        .find(|l| l["type"] == "message")
        .expect("a message line");
    assert_eq!(msg["id"], id);
    assert_eq!(msg["body"], "hello there");
    let end = lines.last().unwrap();
    assert_eq!(end["type"], "batch_end");
    assert_eq!(end["count"], 1);
    assert_eq!(end["max_ackable_id"], id);
}

#[tokio::test]
async fn after_queues_a_send_until_the_earlier_one_is_settled() {
    let r = Roost::start().await;
    r.setup().await;
    let send = |title: &'static str, cid: &'static str, after: Option<&'static str>| {
        let mut args = vec![
            "important",
            "--title",
            title,
            "--actions",
            "yes,no",
            "--correlation-id",
            cid,
        ];
        if let Some(a) = after {
            args.extend(["--after", a]);
        }
        args
    };
    assert!(r.run(&send("First?", "q-1", None)).await.status.success());
    let second = r.run(&send("Second?", "q-2", Some("q-1"))).await;
    assert!(second.status.success());
    assert_eq!(stdout_json(&second)["status"], "queued");
    assert_eq!(
        stdout_json(&second)["delivered_at"],
        serde_json::Value::Null
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("queued behind q-1"));
    assert!(
        r.run(&send("Third?", "q-3", Some("q-2")))
            .await
            .status
            .success()
    );

    let unknown = r.run(&send("Lost?", "q-x", Some("nope"))).await;
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stdout).contains("unknown_after"));

    r.answer("q-1", json!({ "action_id": "yes" })).await;
    r.inbox_until("q-2", "released", |n| n["status"] == "pending")
        .await;
    r.inbox_until("q-3", "still queued", |n| n["status"] == "queued")
        .await;

    // A snooze doesn't settle a question; only an answer or a cancel does.
    r.answer(
        "q-2",
        json!({ "action_id": "snooze", "snooze_minutes": 30 }),
    )
    .await;
    r.inbox_until("q-3", "still queued", |n| n["status"] == "queued")
        .await;
    r.answer("q-2", json!({ "action_id": "no" })).await;
    r.inbox_until("q-3", "released", |n| n["status"] == "pending")
        .await;
}

#[tokio::test]
async fn a_cancelled_queued_send_releases_the_next() {
    let r = Roost::start().await;
    r.setup().await;
    for (cid, after) in [("c-1", None), ("c-2", Some("c-1")), ("c-3", Some("c-2"))] {
        let mut args = vec![
            "important",
            "--title",
            "Q",
            "--actions",
            "yes,no",
            "--correlation-id",
            cid,
        ];
        if let Some(a) = after {
            args.extend(["--after", a]);
        }
        assert!(r.run(&args).await.status.success());
    }
    assert!(r.run(&["cancel", "c-2"]).await.status.success());
    r.inbox_until("c-3", "released", |n| n["status"] == "pending")
        .await;
}

async fn typing_until(r: &Roost) -> serde_json::Value {
    let list: serde_json::Value = r
        .http
        .get(format!("{}/api/human/channels", r.base))
        .bearer_auth(common::ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list["channels"][0]["typing_until"].clone()
}

#[tokio::test]
async fn typing_shows_until_the_next_send() {
    let r = Roost::start().await;
    r.setup().await;

    let out = r.run(&["typing", "1000"]).await;
    assert!(out.status.success());
    let body = stdout_json(&out);
    assert_eq!(body["typing"], true);
    assert_eq!(body["seconds"], 300);
    assert_eq!(body["clamped"], true);
    assert!(typing_until(&r).await.is_string());

    assert!(
        r.run(&["message", "--title", "Done"])
            .await
            .status
            .success()
    );
    assert!(typing_until(&r).await.is_null(), "a send clears the dots");

    assert_eq!(stdout_json(&r.run(&["typing"]).await)["seconds"], 60);
    let off = r.run(&["typing", "off"]).await;
    assert_eq!(stdout_json(&off)["typing"], false);
    assert!(typing_until(&r).await.is_null());
    assert_eq!(r.run(&["typing", "soon"]).await.status.code(), Some(1));
}

#[tokio::test]
async fn an_exec_handler_shows_typing_while_it_runs() {
    let r = Roost::start().await;
    r.setup().await;
    r.human_says("take your time").await;
    let listen = tokio::spawn(
        r.cli(&[
            "listen",
            "--timeout",
            "20",
            "--exec",
            "cat >/dev/null; sleep 3",
        ])
        .output(),
    );
    let mut seen = false;
    for _ in 0..40 {
        if typing_until(&r).await.is_string() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seen, "no typing dots while the handler ran");
    let out = listen.await.unwrap().unwrap();
    assert!(out.status.success());
    assert!(
        typing_until(&r).await.is_null(),
        "the dots stop with the handler"
    );
}

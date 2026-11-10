//! Gate 5 public-boundary tests: durable attention, delivery attempts, owner commands,
//! snooze, manual sweeps, outcomes and closure. Every fixture is disposable. Oracles are
//! independent of product labels: SQL rows, the deterministic test sink's append-only file,
//! real `git` reading the fixture repository, and kernel process identity.
#![cfg(feature = "fault-injection")]
use asher_bridge::{
    Owner,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const HOUR: i64 = 3_600_000;
const MIN: i64 = 60_000;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("state");
        let mut o = Owner::initialize(&root).unwrap();
        o.add_permit(
            "p",
            &PermitScope {
                fixture_id: "fixture:symbol".into(),
                action: "prepare_fixture".into(),
                max_attempts: 1,
                max_runtime_ms: 60000,
                network: false,
            },
            now_ms().unwrap() + 23 * HOUR,
        )
        .unwrap();
        o.submit(&SubmitWork {
            protocol_version: 1,
            request_key: "w".into(),
            fixture_id: "fixture:symbol".into(),
            success_criteria: "Prepare synthetic symbol correction; capture remains incomplete"
                .into(),
            source_evidence: vec!["fixture:malformed-symbol".into()],
            permit_id: "p".into(),
        })
        .unwrap();
        drop(o);
        let f = Self { temp: t, root };
        fs::create_dir_all(f.sink_dir()).unwrap();
        f
    }
    fn sink_dir(&self) -> PathBuf {
        self.temp.path().join("sink")
    }
    fn clock_file(&self) -> PathBuf {
        self.temp.path().join("clock-offset-ms")
    }
    fn set_clock(&self, ms: i64) {
        fs::write(self.clock_file(), ms.to_string()).unwrap();
    }
    fn sink_mode(&self, m: &str) {
        fs::write(self.sink_dir().join("mode"), m).unwrap();
    }
    fn sink_lines(&self) -> Vec<Value> {
        fs::read_to_string(self.sink_dir().join("delivered.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn journal(&self) -> PathBuf {
        self.temp.path().join("executor.journal")
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action)
            .arg(&self.root)
            .env("ASHER_TEST_CLOCK_OFFSET_FILE", self.clock_file())
            .env("ASHER_TEST_SINK_DIR", self.sink_dir())
            .env("ASHER_TEST_EFFECT_JOURNAL", self.journal());
        c
    }
    fn run(&self, action: &str, args: &[&str]) -> Output {
        self.cmd(action).args(args).output().unwrap()
    }
    fn json(&self, action: &str, args: &[&str]) -> Value {
        let out = self.run(action, args);
        assert!(
            out.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn fails(&self, action: &str, args: &[&str]) -> String {
        let out = self.run(action, args);
        assert!(!out.status.success(), "{action} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into()
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.join("ledger.db")).unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn text(&self, sql: &str) -> String {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn exec(&self, sql: &str) {
        self.db().execute_batch(sql).unwrap();
    }
    fn work_id(&self) -> String {
        self.text("SELECT id FROM work WHERE request_key='w'")
    }
    fn work(&self) -> (String, String, String, String, i64, i64) {
        self.db()
            .query_row(
                "SELECT mode,phase,next_owner,next_action,due_at,version FROM work WHERE request_key='w'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap()
    }
    // ---- fixture flow (same recipes as the Task 3/4 suites)
    fn prepared(&self) {
        self.json("symbol-intent", &["w", "a"]);
        self.json("reproduce", &["a"]);
        self.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
    }
    fn validated(&self) -> String {
        self.prepared();
        let art = self.json("seal", &["a"]);
        let id = art["id"].as_str().unwrap().to_string();
        assert_eq!(self.json("validate", &[&id])["validation_passed"], true);
        id
    }
    fn validated_failing(&self) -> String {
        self.prepared();
        let p: PathBuf = self
            .text("SELECT fixture_path FROM attempt WHERE id='a'")
            .into();
        fs::write(
            p.join("candidate/symbols.csv"),
            b"symbol,scope\nBAD SYMBOL,core\n",
        )
        .unwrap();
        let art = self.json("seal", &["a"]);
        let id = art["id"].as_str().unwrap().to_string();
        assert_eq!(self.json("validate", &[&id])["validation_passed"], false);
        id
    }
    fn review(&self) -> Value {
        self.json("review-create", &["a"])
    }
    fn decide(&self, review: &str, id: &str, choice: &str) -> Value {
        let t = self.json("decision-template", &[review, id, choice]);
        let out = self.send("decide", &t);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn approve(&self, review: &str, id: &str) -> String {
        self.decide(review, id, "approve")["effect_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn applied(&self) -> (String, String, String, String) {
        self.validated();
        let r = self.review();
        let rid = r["review_id"].as_str().unwrap().to_string();
        let old = r["envelope"]["action"]["expected_old"]
            .as_str()
            .unwrap()
            .to_string();
        let new = r["envelope"]["action"]["new_commit"]
            .as_str()
            .unwrap()
            .to_string();
        let effect = self.approve(&rid, "d1");
        let d = self.json("dispatch", &[&effect]);
        assert_eq!(d["classification"], "confirmed_applied");
        (rid, effect, old, new)
    }
    fn send(&self, action: &str, body: &Value) -> Output {
        self.send_bytes(action, &serde_json::to_vec(body).unwrap())
    }
    fn send_bytes(&self, action: &str, body: &[u8]) -> Output {
        let mut c = self
            .cmd(action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(body).unwrap();
        c.wait_with_output().unwrap()
    }
    // ---- owner commands
    fn signed(&self, action: &str, target: &str, id: &str, ver: i64, payload: &Value) -> Value {
        self.json(
            "command-template",
            &[action, target, id, &ver.to_string(), &payload.to_string()],
        )
    }
    fn command(&self, action: &str, target: &str, id: &str, ver: i64, payload: &Value) -> Value {
        let t = self.signed(action, target, id, ver, payload);
        let out = self.send("command", &t);
        assert!(
            out.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn command_fails(
        &self,
        action: &str,
        target: &str,
        id: &str,
        ver: i64,
        payload: &Value,
    ) -> String {
        let t = self.signed(action, target, id, ver, payload);
        let out = self.send("command", &t);
        assert!(
            !out.status.success(),
            "{action} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).into()
    }
    // ---- attention
    fn sweep(&self) -> Value {
        self.json("sweep", &["--backend", "sink"])
    }
    fn inbox(&self) -> Value {
        self.json("inbox", &[])
    }
    fn open_item(&self, kind: &str) -> Value {
        self.inbox()["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["kind"] == kind && i["state"] != "resolved")
            .cloned()
            .unwrap_or_else(|| panic!("no live {kind} item"))
    }
    fn closure(&self, outcome: &str) -> Value {
        self.json("closure-summary", &["w", outcome])
    }
    fn close(&self, outcome: &str, id: &str) -> Value {
        let s = self.closure(outcome);
        assert_eq!(s["eligible"], true, "{s}");
        let w = self.work();
        self.command(
            "close",
            &format!("work:{}", self.work_id()),
            id,
            w.5,
            &json!({"outcome": outcome, "reason": format!("owner closes as {outcome}"), "evidence_digest": s["digest"]}),
        )
    }
    fn dump(&self, tables: &[&str]) -> Vec<String> {
        let c = self.db();
        let mut out = Vec::new();
        for t in tables {
            let mut q = c
                .prepare(&format!("SELECT * FROM {t} ORDER BY rowid"))
                .unwrap();
            let n = q.column_count();
            let rows = q
                .query_map([], |r| {
                    Ok((0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap();
            for r in rows {
                out.push(format!("{t}:{}", r.unwrap()));
            }
        }
        out
    }
    // ---- git oracle
    fn repo(&self) -> PathBuf {
        self.text("SELECT path FROM fixture_repo").into()
    }
    fn git(&self, args: &[&str]) -> String {
        let home = self.temp.path().join("git-home");
        fs::create_dir_all(&home).unwrap();
        let o = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", self.repo())
            .env("GIT_COMMITTER_NAME", "w")
            .env("GIT_COMMITTER_EMAIL", "w@invalid")
            .env("GIT_AUTHOR_NAME", "w")
            .env("GIT_AUTHOR_EMAIL", "w@invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn current_ref(&self) -> String {
        self.git(&["rev-parse", "refs/heads/fixture"])
    }
    fn paused(&self, mut c: Command, name: &str) -> (Child, TempDir) {
        let b = tempfile::tempdir().unwrap();
        let mut child = c
            .env("ASHER_TEST_BARRIER", name)
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        while !b.path().join(name).exists() {
            if Instant::now() > end || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let out = child.wait_with_output().unwrap();
                panic!(
                    "barrier {name} not reached: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        (child, b)
    }
}
fn kill(mut child: Child) {
    let pid = child.id();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

// ============================================================ notification_failures
#[test]
fn notification_failures() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();

    // Queued: durable attention + outbox exist; no delivery claim.
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    assert_eq!(item["delivery"]["status"], "queued");
    assert_eq!(item["delivery"]["attempts"], 0);
    assert_eq!(item["work"]["next_action"], "decide_exact_review");
    assert_eq!(item["review_id"], rid.as_str());
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM delivery_attempt WHERE attention_id='{aid}'"
        )),
        0
    );
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM outbox WHERE attention_id='{aid}'"
        )),
        "queued"
    );

    // Backend offline: attempt recorded as failed; the obligation is untouched.
    f.sink_mode("offline");
    let s = f.sweep();
    assert_eq!(s["attempts"][0]["state"], "failed");
    let item = f.open_item("responsibility");
    assert_eq!(item["delivery"]["status"], "failed");
    assert_eq!(item["state"], "open");
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM outbox WHERE attention_id='{aid}'"
        )),
        "retry_wait"
    );
    assert!(f.sink_lines().is_empty());

    // Bounded retry: nothing is due yet, so an immediate sweep does not retry.
    assert!(f.sweep()["attempts"].as_array().unwrap().is_empty());
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM delivery_attempt WHERE attention_id='{aid}'"
        )),
        1
    );

    // Same attention ID is redelivered after the delay; acceptance is not human receipt.
    f.set_clock(6_000);
    f.sink_mode("accept");
    let s = f.sweep();
    assert_eq!(s["attempts"][0]["state"], "accepted");
    assert_eq!(s["attempts"][0]["attention_id"], aid.as_str());
    let lines = f.sink_lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["attention_id"], aid.as_str());
    let item = f.open_item("responsibility");
    assert_eq!(item["delivery"]["status"], "backend_accepted");
    assert_eq!(item["delivery"]["backend_accepted"], true);
    assert_eq!(item["human_receipt"], "not established");
    assert_eq!(item["state"], "open", "acceptance is not acknowledgment");
    assert_eq!(
        f.count("SELECT count(*) FROM decision"),
        0,
        "acceptance is not a decision"
    );
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "pending"
    );
    // No further delivery once accepted and nothing is due.
    f.set_clock(7_000);
    assert!(f.sweep()["attempts"].as_array().unwrap().is_empty());
    assert_eq!(f.sink_lines().len(), 1);
}

#[test]
fn notification_failures_crash_after_send_before_receipt_commit() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut c = f.cmd("sweep");
    c.args(["--backend", "sink"]);
    let (child, _d) = f.paused(c, "after_backend_send");
    kill(child);
    // The backend displayed it, but no receipt was committed: in doubt, not accepted.
    assert_eq!(f.sink_lines().len(), 1);
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM delivery_attempt WHERE attention_id='{aid}' AND ordinal=1"
        )),
        "attempted"
    );
    let item = f.open_item("responsibility");
    assert_eq!(item["delivery"]["status"], "attempted");
    assert_eq!(item["state"], "open");
    // The pending notice survives restart (fresh process) and is still visible.
    f.json("owner-key-status", &[]);
    assert_eq!(f.open_item("responsibility")["attention_id"], aid.as_str());
    // Within the claim window nothing is redelivered.
    assert!(f.sweep()["attempts"].as_array().unwrap().is_empty());
    // After the claim expires the attempt becomes delivery-unknown and is redelivered, same ID.
    f.set_clock(30_000);
    let s = f.sweep();
    assert_eq!(s["stale_attempts_marked_unknown"], 1);
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM delivery_attempt WHERE attention_id='{aid}' AND ordinal=1"
        )),
        "unknown"
    );
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM delivery_attempt WHERE attention_id='{aid}' AND ordinal=2"
        )),
        "accepted"
    );
    let lines = f.sink_lines();
    assert_eq!(lines.len(), 2, "duplicate display is possible and allowed");
    assert!(lines.iter().all(|l| l["attention_id"] == aid.as_str()));
    // Redelivery cannot duplicate an owner decision, effect or attention.
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
    assert_eq!(
        f.count(&format!("SELECT count(*) FROM attention WHERE id='{aid}'")),
        1
    );
}

#[test]
fn notification_failures_lost_receipt_is_delivery_unknown() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.sink_mode("lose_receipt");
    let s = f.sweep();
    assert_eq!(s["attempts"][0]["state"], "unknown");
    assert_eq!(f.sink_lines().len(), 1, "the backend did show it");
    let item = f.open_item("responsibility");
    assert_eq!(item["delivery"]["status"], "delivery_unknown");
    assert_eq!(item["delivery"]["backend_accepted"], false);
    assert_eq!(item["state"], "open");
    f.set_clock(6_000);
    f.sink_mode("accept");
    assert_eq!(f.sweep()["attempts"][0]["state"], "accepted");
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM delivery_attempt WHERE attention_id='{aid}'"
        )),
        2
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='responsibility' AND state='open'"),
        1
    );
}

#[test]
fn notification_failures_desktop_backend_unavailable_is_recorded_not_cleared() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let out = f
        .cmd("sweep")
        .args(["--backend", "desktop"])
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/asher-test-bus",
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(s["attempts"][0]["state"], "failed");
    assert_eq!(s["attempts"][0]["backend"], "desktop");
    assert!(
        s["attempts"][0]["error"]
            .as_str()
            .unwrap()
            .contains("desktop")
    );
    let item = f.open_item("responsibility");
    assert_eq!(item["state"], "open");
    assert_eq!(item["delivery"]["status"], "failed");
}

#[test]
fn notification_retry_is_bounded_and_escalation_stays_on_the_same_channel() {
    let f = Fixture::new();
    // Responsibility without an expiring review, so only delivery behavior is exercised.
    f.validated();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.sink_mode("offline");
    let mut t = 0;
    for _ in 0..40 {
        t += 20 * MIN;
        f.set_clock(t);
        f.sweep();
    }
    let n = f.count(&format!(
        "SELECT count(*) FROM delivery_attempt WHERE attention_id='{aid}'"
    ));
    assert!(n <= 24, "retries are bounded per escalation level, got {n}");
    assert_eq!(
        f.text(&format!("SELECT state FROM attention WHERE id='{aid}'")),
        "open",
        "exhaustion never clears the obligation"
    );
    assert!(f.count(&format!("SELECT count(*) FROM delivery_attempt WHERE attention_id='{aid}' AND backend<>'test_sink'")) == 0);
    assert_eq!(
        f.count("SELECT count(*) FROM outbox WHERE channel<>'desktop'"),
        0
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE recipient<>'local-owner'"),
        0
    );
}

// ============================================================ ack_is_not_decision
#[test]
fn ack_is_not_decision() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let target = format!("attention:{aid}");
    let ver = item["version"].as_i64().unwrap();
    let before = f.work();
    let good = f.signed("ack", &target, "ack-1", ver, &json!({}));

    // Unauthorized variants never change anything and are inspectable.
    let mut agent = good.clone();
    agent["actor"] = json!("agent");
    let mut bad_proof = good.clone();
    bad_proof["owner_proof"] = json!("0".repeat(64));
    let mut no_proof = good.clone();
    no_proof.as_object_mut().unwrap().remove("owner_proof");
    let mut extra = good.clone();
    extra["role"] = json!("local-owner");
    let mut tampered = good.clone();
    tampered["payload"] = json!({"note": "tampered after signing"});
    let mut retarget = good.clone();
    retarget["target"] = json!("attention:other");
    for bad in [&agent, &bad_proof, &no_proof, &extra, &tampered, &retarget] {
        assert!(!f.send("command", bad).status.success(), "accepted {bad}");
    }
    let dup = format!(
        "{{\"actor\":\"agent\",{}",
        &serde_json::to_string(&good).unwrap()[1..]
    );
    assert!(!f.send_bytes("command", dup.as_bytes()).status.success());
    assert_eq!(
        f.text(&format!("SELECT state FROM attention WHERE id='{aid}'")),
        "open"
    );
    assert_eq!(f.count("SELECT count(*) FROM owner_command"), 0);
    assert!(
        f.count("SELECT count(*) FROM event WHERE kind IN ('command.forgery','command.rejected')")
            >= 6
    );

    // An agent claim or delivery callback is not an owner path: accepted delivery stays open.
    f.sweep();
    assert_eq!(f.open_item("responsibility")["state"], "open");

    // Authenticated acknowledgment.
    let r = f.command("ack", &target, "ack-1", ver, &json!({}));
    assert_eq!(r["disposition"], "recorded");
    assert_eq!(r["approved"], false);
    assert_eq!(r["resolved"], false);
    let item = f.open_item("responsibility");
    assert_eq!(item["state"], "acknowledged");
    // Acknowledgment is neither a decision nor a resolution: review pending, work untouched.
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "pending"
    );
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
    assert_eq!(
        f.work(),
        before,
        "ack changed work state, version or deadline"
    );

    // Identical authenticated replay returns the recorded receipt without repeating anything.
    let again: Value = serde_json::from_slice(&f.send("command", &good).stdout).unwrap();
    assert_eq!(again["disposition"], "existing");
    assert_eq!(again["recorded_at"], r["recorded_at"]);
    assert_eq!(f.count("SELECT count(*) FROM owner_command"), 1);
    // Same ID, changed content: conflict, inspectable. Stale version: rejected.
    let changed = f.signed("ack", &target, "ack-1", ver, &json!({"note": "different"}));
    let out = f.send("command", &changed);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("different content"));
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='command.conflict' AND entity_id='ack-1'"),
        1
    );
    assert!(
        f.command_fails("ack", &target, "ack-2", ver, &json!({}))
            .contains("stale")
    );
    let v2 = f.open_item("responsibility")["version"].as_i64().unwrap();
    assert!(
        f.command_fails("ack", &target, "ack-3", v2, &json!({}))
            .contains("not open")
    );

    // The review still expires: acknowledgment does not extend approval.
    f.set_clock(2 * HOUR);
    f.sweep();
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "expired"
    );
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
}

#[test]
fn snooze_is_bounded_and_touches_nothing_else() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let target = format!("attention:{aid}");
    let ver = item["version"].as_i64().unwrap();
    for (i, bad) in [
        json!({"reason": "r", "duration_ms": 0}),
        json!({"reason": "r", "duration_ms": 3_600_001}),
        json!({"reason": "r", "duration_ms": -5}),
        json!({"duration_ms": 1000}),
        json!({"reason": "", "duration_ms": 1000}),
        json!({"reason": "r", "duration_ms": 1000, "extend_review": true}),
    ]
    .iter()
    .enumerate()
    {
        let e = f.command_fails("snooze", &target, &format!("sn-bad-{i}"), ver, bad);
        assert!(
            e.contains("snooze")
                || e.contains("duration")
                || e.contains("reason")
                || e.contains("payload"),
            "{e}"
        );
    }
    assert_eq!(
        f.text(&format!("SELECT state FROM attention WHERE id='{aid}'")),
        "open"
    );

    let frozen = [
        "permit", "attempt", "review", "evidence", "artifact", "effect", "decision",
    ];
    let before_rows = f.dump(&frozen);
    let before_work = f.work();
    let real_before = now_ms().unwrap();
    let r = f.command(
        "snooze",
        &target,
        "sn-1",
        ver,
        &json!({"reason": "in a meeting", "duration_ms": 10 * MIN}),
    );
    let until = r["result"]["snoozed_until"].as_i64().unwrap();
    assert!(
        until >= real_before + 10 * MIN && until <= now_ms().unwrap() + 10 * MIN,
        "bounded duration honoured"
    );
    assert_eq!(
        f.dump(&frozen),
        before_rows,
        "snooze touched underlying obligations"
    );
    assert_eq!(
        f.work(),
        before_work,
        "snooze moved a work deadline or version"
    );
    let item = f.open_item("responsibility");
    assert_eq!(item["state"], "open", "snooze is not resolution");
    assert_eq!(item["snoozed_until"].as_i64().unwrap(), until);
    // Delivery is suppressed only while snoozed.
    assert!(f.sweep()["attempts"].as_array().unwrap().is_empty());
    assert!(f.sink_lines().is_empty());
    f.set_clock(10 * MIN + 1000);
    let s = f.sweep();
    assert!(
        s["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["attention_id"] == aid.as_str()),
        "{s}"
    );
    // The exact one-hour maximum is accepted (separate item, no review to expire).
    let g = Fixture::new();
    g.validated();
    let it = g.open_item("responsibility");
    let r = g.command(
        "snooze",
        &format!("attention:{}", it["attention_id"].as_str().unwrap()),
        "sn-max",
        it["version"].as_i64().unwrap(),
        &json!({"reason": "max", "duration_ms": HOUR}),
    );
    assert!(r["result"]["snoozed_until"].as_i64().unwrap() <= now_ms().unwrap() + HOUR);
}

#[test]
fn snooze_does_not_hide_overdue_or_escalation() {
    let f = Fixture::new();
    f.validated(); // work: blocked / local-owner / create_exact_review, due in ~10 s
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let w0 = f.work();
    f.command(
        "snooze",
        &format!("attention:{aid}"),
        "sn-1",
        item["version"].as_i64().unwrap(),
        &json!({"reason": "later", "duration_ms": HOUR}),
    );
    // Past the deadline and the escalation threshold, still snoozed.
    f.set_clock(6 * MIN);
    let s = f.sweep();
    assert!(s["overdue_recorded"].as_i64().unwrap() >= 1);
    let overdue = f.open_item("overdue");
    assert_eq!(overdue["severity"], "urgent");
    assert_eq!(overdue["work"]["overdue"], true);
    assert_eq!(
        f.work(),
        w0,
        "recording overdue must not move the deadline or version"
    );
    // The snoozed item's escalation is RECORDED but its delivery is deferred.
    assert_eq!(
        f.count(&format!(
            "SELECT escalation_level FROM attention WHERE id='{aid}'"
        )),
        1
    );
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM event WHERE kind='attention.escalated' AND entity_id='{aid}'"
        )),
        1
    );
    assert!(
        f.sink_lines()
            .iter()
            .all(|l| l["attention_id"] != aid.as_str()),
        "snoozed item delivered"
    );
    assert!(
        f.sink_lines()
            .iter()
            .any(|l| l["attention_id"] == overdue["attention_id"])
    );
    // The snoozed item stays visible and unresolved.
    let item = f.open_item("responsibility");
    assert_eq!(item["state"], "open");
    assert_eq!(item["work"]["overdue"], true);
    // Repeating the sweep does not duplicate the overdue item.
    f.sweep();
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue'"),
        1
    );
}

#[test]
fn owner_authority_matrix_and_key_recovery() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let target = format!("attention:{aid}");
    let ver = item["version"].as_i64().unwrap();
    // Invalid key: commands are refused with an actionable error; recovery operations work.
    let key = f.root.join("owner.key");
    let pre = fs::read(&key).unwrap();
    fs::write(&key, b"short").unwrap();
    assert!(
        f.fails(
            "command-template",
            &["ack", &target, "k-1", &ver.to_string(), "{}"]
        )
        .contains("owner credential")
    );
    assert!(!f.send_bytes("command", b"{}").status.success());
    assert_eq!(fs::read(&key).unwrap(), b"short");
    assert_eq!(f.sweep()["backend"], "test_sink");
    assert!(!f.inbox()["items"].as_array().unwrap().is_empty());
    // Reset is the only path; commands signed with the old key no longer authenticate.
    fs::write(&key, &pre).unwrap();
    let old_signed = f.signed("ack", &target, "k-old", ver, &json!({}));
    fs::write(&key, b"short").unwrap();
    f.json("owner-key-reset", &["--replace-invalid"]);
    let out = f.send("command", &old_signed);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("authentication"));
    assert_eq!(f.open_item("responsibility")["state"], "open");
    let r = f.command("ack", &target, "k-new", ver, &json!({}));
    assert_eq!(r["disposition"], "recorded");
}

// ============================================================ closure_and_cancel
#[test]
fn closure_and_cancel_succeeded_requires_owner_command_and_atomic_notice() {
    let f = Fixture::new();
    let (rid, effect, _old, new) = f.applied();
    let wid = f.work_id();
    let w = f.work();
    assert_eq!(w.1, "finalizing");
    assert_eq!(
        f.count("SELECT count(*) FROM work_outcome"),
        0,
        "an action result is not closure"
    );
    let s = f.closure("succeeded");
    assert_eq!(s["eligible"], true, "{s}");
    assert_eq!(s["evidence"]["effect"]["id"], effect.as_str());
    assert_eq!(s["evidence"]["review"]["id"], rid.as_str());
    assert_eq!(s["evidence"]["readback"]["observed_ref"], new.as_str());
    assert!(
        s["evidence"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g.as_str().unwrap().contains("capture"))
    );

    // Without owner authentication nothing closes; a stale digest is refused.
    let target = format!("work:{wid}");
    let payload = json!({"outcome": "succeeded", "reason": "fixture ref updated and verified", "evidence_digest": s["digest"]});
    let mut t = f.signed("close", &target, "c-1", w.5, &payload);
    t["actor"] = json!("agent");
    assert!(!f.send("command", &t).status.success());
    assert_eq!(f.work().0, "active");
    let e = f.command_fails(
        "close",
        &target,
        "c-stale",
        w.5,
        &json!({"outcome": "succeeded", "reason": "x", "evidence_digest": "0".repeat(64)}),
    );
    assert!(e.contains("evidence"), "{e}");
    assert_eq!(f.work().0, "active");

    let r = f.command("close", &target, "c-1", w.5, &payload);
    assert_eq!(r["result"]["closed"], true);
    assert_eq!(r["result"]["outcome"], "succeeded");
    let c = f.work();
    assert_eq!(
        (c.0.as_str(), c.1.as_str(), c.2.as_str(), c.3.as_str()),
        ("closed", "done", "none", "none")
    );
    // Closed record: outcome, evidence references, authorized actor, reason and time.
    let (outcome, actor, reason, ev, at): (String, String, String, String, i64) = f
        .db()
        .query_row(
            "SELECT outcome,closed_by,reason,evidence_json,closed_at FROM work_outcome",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (outcome.as_str(), actor.as_str()),
        ("succeeded", "local-owner")
    );
    assert!(reason.contains("verified") && at > 0);
    assert!(ev.contains(&effect) && ev.contains(&rid));
    // Atomic: owner command, audit event and the nonblocking outcome notice exist together.
    assert_eq!(
        f.count("SELECT count(*) FROM owner_command WHERE id='c-1' AND action='close'"),
        1
    );
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.closed'"),
        1
    );
    let notice = f.open_item("outcome");
    assert_eq!(notice["blocking"], false);
    assert_eq!(notice["delivery"]["status"], "queued");
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE blocking=1 AND state<>'resolved'"),
        0,
        "no live blocking attention after closure"
    );
    // Closed work is immutable and not re-closable.
    assert!(
        f.db()
            .execute("UPDATE work SET next_action='x'", [])
            .is_err()
    );
    assert!(
        f.command_fails("close", &target, "c-2", c.5, &payload)
            .contains("closed")
    );
    // Replay returns the recorded receipt.
    let again: Value = serde_json::from_slice(
        &f.send("command", &f.signed("close", &target, "c-1", w.5, &payload))
            .stdout,
    )
    .unwrap();
    assert_eq!(again["disposition"], "existing");
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 1);
    // The notice is delivered and acknowledged separately from the closed outcome.
    f.sweep();
    assert_eq!(
        f.open_item("outcome")["delivery"]["status"],
        "backend_accepted"
    );
    let n = f.open_item("outcome");
    f.command(
        "ack",
        &format!("attention:{}", n["attention_id"].as_str().unwrap()),
        "ack-n",
        n["version"].as_i64().unwrap(),
        &json!({}),
    );
    assert_eq!(f.work().0, "closed");
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "succeeded");
    assert!(f.run("check", &[]).status.success());
    assert_eq!(f.current_ref(), new);
}

#[test]
fn closure_rolls_back_with_notice() {
    let f = Fixture::new();
    f.applied();
    let wid = f.work_id();
    let s = f.closure("succeeded");
    let w = f.work();
    let payload = json!({"outcome": "succeeded", "reason": "ok", "evidence_digest": s["digest"]});
    // A real storage failure while writing the notice must prevent the closure entirely.
    f.exec("CREATE TRIGGER fail_notice BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT,'disk full (modelled)'); END;");
    let e = f.command_fails("close", &format!("work:{wid}"), "c-1", w.5, &payload);
    assert!(e.contains("disk full") || e.contains("storage"), "{e}");
    assert_eq!(
        f.work(),
        w,
        "work changed although the notice could not be written"
    );
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 0);
    assert_eq!(
        f.count("SELECT count(*) FROM owner_command WHERE id='c-1'"),
        0
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='outcome'"),
        0
    );
    // Once storage works the same command succeeds.
    f.exec("DROP TRIGGER fail_notice;");
    let r = f.command("close", &format!("work:{wid}"), "c-1", w.5, &payload);
    assert_eq!(r["result"]["closed"], true);
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='outcome'"),
        1
    );
}

#[test]
fn transition_creates_attention_atomically() {
    let f = Fixture::new();
    f.validated();
    let before = f.work();
    f.exec("CREATE TRIGGER fail_outbox BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT,'disk full (modelled)'); END;");
    f.fails("review-create", &["a"]);
    assert_eq!(
        f.work(),
        before,
        "work transition committed without its attention/outbox"
    );
    assert_eq!(f.count("SELECT count(*) FROM review"), 0);
    f.exec("DROP TRIGGER fail_outbox;");
    f.review();
    assert_eq!(
        f.open_item("responsibility")["work"]["next_action"],
        "decide_exact_review"
    );
}

#[test]
fn closure_guards_per_outcome() {
    // Unknown effect (cause not established) can never close as success, failure, cancel or abandon.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let mut c = f.cmd("dispatch");
    c.arg(&effect);
    let (child, _d) = f.paused(c, "after_ref_rename");
    kill(child);
    f.json("reconcile-effects", &[]);
    assert_eq!(
        f.text(&format!("SELECT state FROM effect WHERE id='{effect}'")),
        "unknown"
    );
    let w = f.work();
    for o in ["succeeded", "failed", "rejected", "cancelled"] {
        let s = f.closure(o);
        assert_eq!(s["eligible"], false, "{o}: {s}");
        assert!(!s["blockers"].as_array().unwrap().is_empty());
    }
    let wid = f.work_id();
    let e = f.command_fails(
        "abandon",
        &format!("work:{wid}"),
        "ab-1",
        w.5,
        &json!({"reason": "give up", "residual_risk": "unknown ref write"}),
    );
    assert!(e.contains("unknown") || e.contains("unresolved"), "{e}");
    assert!(f.command_fails("close", &format!("work:{wid}"), "cl-1", w.5, &json!({"outcome": "succeeded", "reason": "x", "evidence_digest": f.closure("succeeded")["digest"]})).contains("refused"));
    assert_eq!(
        f.work(),
        w,
        "a refused closure must not change responsibility, version or deadline"
    );
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 0);
    assert!(
        f.count("SELECT count(*) FROM event WHERE kind='command.refused'") >= 2,
        "refusals are inspectable"
    );

    // Legacy-quarantined / unattributed confirmation cannot be a success.
    let g = Fixture::new();
    g.applied();
    g.exec("DROP TRIGGER effect_observation_immutable; UPDATE effect_observation SET payload_json='{\"attribution\":\"desired state attained with no other writes; audit append missing (crash after rename)\"}'; CREATE TRIGGER effect_observation_immutable BEFORE UPDATE ON effect_observation BEGIN SELECT RAISE(ABORT,'observations immutable'); END;");
    let s = g.closure("succeeded");
    assert_eq!(s["eligible"], false);
    assert!(s["blockers"].to_string().contains("attribution"));

    // Target moved after the effect: readback no longer consistent with the result.
    let h = Fixture::new();
    let (_r, _e, old, _n) = h.applied();
    h.git(&["update-ref", "refs/heads/fixture", &old]);
    let s = h.closure("succeeded");
    assert_eq!(s["eligible"], false);
    assert!(s["blockers"].to_string().contains("readback"));

    // Frozen artifact lost: evidence identity cannot be verified.
    let i = Fixture::new();
    let art = {
        i.applied();
        i.text("SELECT id FROM artifact WHERE role='candidate'")
    };
    fs::remove_dir_all(i.root.join("artifacts").join(&art)).unwrap();
    let s = i.closure("succeeded");
    assert_eq!(s["eligible"], false);
    assert!(s["blockers"].to_string().contains("artifact"));

    // Failed requires evidence of failure; a passing candidate is not a failure.
    let j = Fixture::new();
    j.validated();
    assert_eq!(j.closure("failed")["eligible"], false);
    let k = Fixture::new();
    k.validated_failing();
    let s = k.closure("failed");
    assert_eq!(s["eligible"], true, "{s}");
    assert_eq!(k.closure("succeeded")["eligible"], false);
    let w = k.work();
    let r = k.command("close", &format!("work:{}", k.work_id()), "f-1", w.5, &json!({"outcome": "failed", "reason": "candidate failed trusted validation", "evidence_digest": s["digest"]}));
    assert_eq!(r["result"]["outcome"], "failed");
    // Non-success outcomes are plainly distinguishable from success.
    let n = k.open_item("outcome");
    assert!(
        n["reason"].as_str().unwrap().starts_with("NOT SUCCESS"),
        "{n}"
    );
    assert_eq!(n["severity"], "action");

    // Rejected is tied to the exact review and an authenticated reject decision.
    let l = Fixture::new();
    l.validated();
    let rid = l.review()["review_id"].as_str().unwrap().to_string();
    assert_eq!(l.closure("rejected")["eligible"], false);
    l.decide(&rid, "d-r", "reject");
    let s = l.closure("rejected");
    assert_eq!(s["eligible"], true, "{s}");
    assert_eq!(s["evidence"]["review"]["id"], rid.as_str());
    let w = l.work();
    l.command("close", &format!("work:{}", l.work_id()), "rj-1", w.5, &json!({"outcome": "rejected", "reason": "owner rejected the exact review", "evidence_digest": s["digest"]}));
    assert_eq!(l.text("SELECT outcome FROM work_outcome"), "rejected");
}

#[test]
fn closure_and_cancel() {
    // Cancellation request is not terminal and prevents a not-yet-dispatched approval.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let old = r["envelope"]["action"]["expected_old"]
        .as_str()
        .unwrap()
        .to_string();
    let effect = f.approve(&rid, "d1");
    let wid = f.work_id();
    assert_eq!(
        f.closure("cancelled")["eligible"],
        false,
        "no cancellation requested yet"
    );
    let w = f.work();
    let rcpt = f.command(
        "cancel",
        &format!("work:{wid}"),
        "x-1",
        w.5,
        &json!({"reason": "owner changed their mind"}),
    );
    assert_eq!(rcpt["result"]["terminal"], false);
    assert_eq!(
        f.work().0,
        "active",
        "requesting cancellation is not itself terminal"
    );
    assert_eq!(
        f.text(&format!("SELECT state FROM effect WHERE id='{effect}'")),
        "cancelled"
    );
    assert!(f.fails("dispatch", &[&effect]).contains("cancel"));
    assert_eq!(f.current_ref(), old);
    let s = f.closure("cancelled");
    assert_eq!(s["eligible"], true, "{s}");
    let w = f.work();
    f.command("close", &format!("work:{wid}"), "x-2", w.5, &json!({"outcome": "cancelled", "reason": "cancelled before any effect", "evidence_digest": s["digest"]}));
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "cancelled");

    // Cancellation after the effect does not undo it, and the effect is accounted for.
    let g = Fixture::new();
    let (_r, effect, _old, new) = g.applied();
    let wid = g.work_id();
    let w = g.work();
    g.command(
        "cancel",
        &format!("work:{wid}"),
        "x-1",
        w.5,
        &json!({"reason": "stop"}),
    );
    assert_eq!(
        g.current_ref(),
        new,
        "cancellation must not roll back the effect"
    );
    let s = g.closure("cancelled");
    assert_eq!(s["eligible"], true, "{s}");
    assert_eq!(s["evidence"]["effect"]["id"], effect.as_str());
    assert!(
        s["evidence"]["residual_risks"]
            .to_string()
            .contains("not undone")
    );
    let w = g.work();
    g.command("close", &format!("work:{wid}"), "x-2", w.5, &json!({"outcome": "cancelled", "reason": "cancelled after the effect", "evidence_digest": s["digest"]}));
    assert_eq!(g.current_ref(), new);
    assert_eq!(
        g.text(&format!("SELECT state FROM effect WHERE id='{effect}'")),
        "confirmed",
        "effect record retained"
    );

    // Unresolved live-attempt slot: cancel/abandon can neither release it nor move deadlines.
    let h = Fixture::new();
    h.validated();
    // Model lost supervision with a durable unknown fact (reconciliation derives state from facts).
    h.exec(
        "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','unknown',1,'{}')",
    );
    h.json("owner-key-status", &[]);
    assert_eq!(h.text("SELECT state FROM attempt WHERE id='a'"), "unknown");
    let wid = h.work_id();
    let w = h.work();
    let r = h.command(
        "cancel",
        &format!("work:{wid}"),
        "x-1",
        w.5,
        &json!({"reason": "stop"}),
    );
    assert_eq!(r["result"]["terminal"], false);
    assert!(r["result"]["hazards"].to_string().contains("attempt"));
    assert_eq!(h.closure("cancelled")["eligible"], false);
    let w2 = h.work();
    assert!(
        h.command_fails(
            "abandon",
            &format!("work:{wid}"),
            "x-3",
            w2.5,
            &json!({"reason": "r", "residual_risk": "live unknown attempt"})
        )
        .contains("attempt")
    );
    assert_eq!(
        h.text("SELECT state FROM attempt WHERE id='a'"),
        "unknown",
        "slot retained"
    );
    assert_eq!(
        h.work().4,
        w.4,
        "cancel/abandon must not suppress or move the pending deadline"
    );
    assert_eq!(h.count("SELECT count(*) FROM work_outcome"), 0);
    assert!(
        !h.fails("symbol-intent", &["w", "a2"]).is_empty(),
        "second attempt over unresolved activity is refused"
    );
}

#[test]
fn abandon_closes_only_without_live_hazard_and_records_residual_risk() {
    let f = Fixture::new();
    f.validated();
    let wid = f.work_id();
    let w = f.work();
    let r = f.command("abandon", &format!("work:{wid}"), "ab-1", w.5, &json!({"reason": "no longer wanted", "residual_risk": "prepared candidate left unreviewed"}));
    assert_eq!(r["result"]["closed"], true);
    assert_eq!(r["result"]["outcome"], "abandoned");
    let ev = f.text("SELECT evidence_json FROM work_outcome");
    assert!(ev.contains("prepared candidate left unreviewed"));
    let n = f.open_item("outcome");
    assert!(n["reason"].as_str().unwrap().starts_with("NOT SUCCESS"));
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "abandoned");
}

// ============================================================ obligation_sweep
#[test]
fn obligation_sweep() {
    let f = Fixture::new();
    f.validated(); // blocked / local-owner / create_exact_review, short deadline
    let w0 = f.work();
    // No sweep has run: that is stated, never implied as background progress.
    let i = f.inbox();
    assert!(i["last_sweep_at"].is_null());
    assert_eq!(i["sweep_stale"], true);
    assert_eq!(i["background_progress"], false);
    // Time passes with no sweep: the overdue condition is VISIBLE but nothing ran or was recorded.
    f.set_clock(10 * MIN);
    let i = f.inbox();
    assert_eq!(
        i["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["kind"] == "responsibility" && x["work"]["overdue"] == true)
            .count(),
        1
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue'"),
        0,
        "no scheduler: nothing may run by itself"
    );
    assert_eq!(f.count("SELECT count(*) FROM sweep_run"), 0);
    // An explicit sweep records the overdue obligation once, and itself.
    let s = f.sweep();
    assert!(s["overdue_recorded"].as_i64().unwrap() >= 1);
    assert_eq!(f.count("SELECT count(*) FROM sweep_run"), 1);
    assert_eq!(
        f.work(),
        w0,
        "a sweep must not postpone or rewrite the deadline"
    );
    let i = f.inbox();
    assert!(i["last_sweep_at"].as_i64().unwrap() > 0);
    assert_eq!(i["sweep_stale"], false);
    // Idempotent: further sweeps add no duplicate obligation.
    f.sweep();
    f.sweep();
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue'"),
        1
    );
    assert_eq!(f.count("SELECT count(*) FROM sweep_run"), 3);
    assert_eq!(f.work(), w0);
    // The record goes stale again when time passes without a sweep.
    f.set_clock(30 * MIN);
    assert_eq!(f.inbox()["sweep_stale"], true);
    // Underlying expiry still runs when a sweep executes (review expiry).
    let g = Fixture::new();
    g.validated();
    let rid = g.review()["review_id"].as_str().unwrap().to_string();
    g.set_clock(2 * HOUR);
    g.sweep();
    assert_eq!(
        g.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "expired"
    );
    assert_eq!(
        g.open_item("responsibility")["work"]["next_action"],
        "review_or_evidence_expired_revalidate_then_new_review"
    );
}

#[test]
fn expired_review_creates_attention_and_pending_attention_survives_restart() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Restart: any mutating open, then the same obligation is still there, same ID.
    f.json("owner-key-status", &[]);
    f.json("reconcile-effects", &[]);
    assert_eq!(f.open_item("responsibility")["attention_id"], aid.as_str());
    // The review expires: the old item is superseded by replacement attention, atomically.
    f.set_clock(2 * HOUR);
    f.json("task4-status", &["a"]);
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "expired"
    );
    assert_eq!(
        f.text(&format!("SELECT state FROM attention WHERE id='{aid}'")),
        "resolved"
    );
    assert!(
        f.text(&format!(
            "SELECT resolution FROM attention WHERE id='{aid}'"
        ))
        .contains("superseded")
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='responsibility' AND state<>'resolved'"),
        1
    );
}

// ============================================================ happy path and migration
#[test]
fn attended_fixture_happy_path() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    // 1. owner is told, explicitly, and acknowledges (not approves)
    f.sweep();
    let item = f.open_item("responsibility");
    assert_eq!(item["delivery"]["status"], "backend_accepted");
    f.command(
        "ack",
        &format!("attention:{}", item["attention_id"].as_str().unwrap()),
        "ack-1",
        item["version"].as_i64().unwrap(),
        &json!({}),
    );
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "pending"
    );
    // 2. owner decides, action runs once
    let effect = f.approve(&rid, "d1");
    assert_eq!(f.count("SELECT count(*) FROM attention WHERE state<>'resolved' AND kind='responsibility' AND review_id IS NOT NULL"), 0, "the decision resolved the review attention");
    assert_eq!(
        f.json("dispatch", &[&effect])["classification"],
        "confirmed_applied"
    );
    // 3. result needs the owner's authenticated closure decision
    let live = f.open_item("responsibility");
    assert_eq!(live["work"]["phase"], "finalizing");
    let s = f.closure("succeeded");
    assert_eq!(s["eligible"], true);
    f.close("succeeded", "close-1");
    // 4. outcome notice delivered; closed outcome and notice acknowledgment are separate
    f.sweep();
    let n = f.open_item("outcome");
    assert_eq!(n["delivery"]["status"], "backend_accepted");
    assert_eq!(f.work().0, "closed");
    let i = f.inbox();
    assert_eq!(i["closed_outcomes"][0]["outcome"], "succeeded");
    // Status views must not keep claiming the work is open once it is closed.
    let st = f.json("task4-status", &["a"]);
    assert_eq!(st["work_closed"], true);
    assert_eq!(st["work"]["mode"], "closed");
    assert_eq!(st["outcome"], "succeeded");
    assert_eq!(f.json("task3-status", &["a"])["work_closed"], true);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 1);
    assert!(f.run("check", &[]).status.success());
}

#[test]
fn schema_five_migration_preserves_records_and_backfills_attention_on_sweep() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("ledger.db");
    let c = Connection::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    for m in [
        include_str!("../migrations/001.sql"),
        include_str!("../migrations/002.sql"),
        include_str!("../migrations/003.sql"),
        include_str!("../migrations/004.sql"),
        include_str!("../migrations/005.sql"),
    ] {
        c.execute_batch(m).unwrap();
    }
    c.execute_batch(
        "INSERT INTO permit VALUES('p',1,'local-owner','hash','{}',1,99999999999999,NULL);
         INSERT INTO work VALUES('old','local-owner','key','hash','{}','p','active','reviewing',3,'local-owner','decide_exact_review',4102444800000,0);
         INSERT INTO work VALUES('mid','local-owner','key2','hash2','{}','p','active','applying',2,'coordinator','dispatch_approved_effect',4102444800000,0);
         INSERT INTO event VALUES(1,'old','local-owner','work.accepted','old',1,'{}');",
    )
    .unwrap();
    let dump = |c: &Connection| -> Vec<String> {
        let mut out = Vec::new();
        for t in ["permit", "work", "event"] {
            let mut q = c
                .prepare(&format!(
                    "SELECT * FROM {t} {} ORDER BY rowid",
                    if t == "event" {
                        "WHERE kind NOT LIKE 'owner_key.%'"
                    } else {
                        ""
                    }
                ))
                .unwrap();
            let n = q.column_count();
            let rows = q
                .query_map([], |r| {
                    Ok((0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap();
            for r in rows {
                out.push(format!("{t}:{}", r.unwrap()));
            }
        }
        out
    };
    let before = dump(&c);
    drop(c);
    drop(Owner::open(&root).unwrap());
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        c.query_row("SELECT version FROM schema_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(dump(&c), before, "migration changed historical rows");
    for t in [
        "owner_command",
        "work_outcome",
        "delivery_attempt",
        "cancel_request",
        "sweep_run",
    ] {
        assert_eq!(
            c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    // Closed rows are only reachable through an outcome; the closed shape is enforced.
    assert!(c.execute("UPDATE work SET mode='closed', phase='done', next_owner='none', next_action='none' WHERE id='old'", []).is_err());
    assert!(c.execute("DELETE FROM work", []).is_err());
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
    drop(q);
    drop(c);
    // The pending owner obligation had no attention before schema 6; an explicit sweep creates it once.
    let out = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .args(["sweep"])
        .arg(&root)
        .args(["--backend", "sink"])
        .env("ASHER_TEST_SINK_DIR", t.path().join("sink"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attention WHERE work_id='old' AND state='open'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attention WHERE work_id='mid'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "coordinator-owned work needs no owner attention"
    );
    let w: (i64, i64) = c
        .query_row("SELECT due_at,version FROM work WHERE id='old'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        w,
        (4102444800000, 3),
        "backfill must not move the deadline or version"
    );
    // Unsupported newer schema is still refused without mutation.
    c.execute("UPDATE schema_meta SET version=9", []).unwrap();
    drop(c);
    assert!(matches!(
        Owner::open(&root),
        Err(asher_bridge::Error::Schema(9))
    ));
}

#[test]
fn resolve_applies_only_to_nonblocking_notices_and_cancels_their_delivery() {
    let f = Fixture::new();
    f.validated();
    // A blocking obligation cannot be resolved by command: that would erase the obligation.
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let e = f.command_fails(
        "resolve",
        &format!("attention:{aid}"),
        "rs-1",
        item["version"].as_i64().unwrap(),
        &json!({"reason": "make it go away"}),
    );
    assert!(e.contains("obligation still live"), "{e}");
    assert_eq!(f.open_item("responsibility")["attention_id"], aid.as_str());
    // An outcome notice can be resolved by the owner; closure stays as it was.
    let w = f.work();
    let s = f.closure("abandoned");
    assert_eq!(s["eligible"], true, "{s}");
    f.command(
        "abandon",
        &format!("work:{}", f.work_id()),
        "ab-1",
        w.5,
        &json!({"reason": "r", "residual_risk": "candidate unreviewed"}),
    );
    let n = f.open_item("outcome");
    let nid = n["attention_id"].as_str().unwrap().to_string();
    let r = f.command(
        "resolve",
        &format!("attention:{nid}"),
        "rs-2",
        n["version"].as_i64().unwrap(),
        &json!({"reason": "read it"}),
    );
    assert_eq!(r["resolved"], true);
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM outbox WHERE attention_id='{nid}'"
        )),
        "cancelled"
    );
    assert_eq!(f.work().0, "closed");
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "abandoned");
    assert!(
        f.sweep()["attempts"].as_array().unwrap().is_empty(),
        "a resolved notice is not delivered"
    );
}

// ============================================================ Gate 5 regressions
// Regressions found in review. Each test drives the problematic
// operation SEQUENCE through the public CLI boundary, not a single command in isolation.

fn third_commit(f: &Fixture, parent: &str) -> String {
    let run = |args: &[&str], input: Option<&[u8]>| -> String {
        let mut c = Command::new("git");
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", f.temp.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", f.repo())
            .env("GIT_COMMITTER_NAME", "w")
            .env("GIT_COMMITTER_EMAIL", "w@invalid")
            .env("GIT_AUTHOR_NAME", "w")
            .env("GIT_AUTHOR_EMAIL", "w@invalid")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        match input {
            Some(i) => ch.stdin.take().unwrap().write_all(i).unwrap(),
            None => drop(ch.stdin.take()),
        }
        let o = ch.wait_with_output().unwrap();
        assert!(o.status.success(), "git {args:?}");
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    let blob = run(&["hash-object", "-w", "--stdin"], Some(b"competing\n"));
    let tree = run(
        &["mktree"],
        Some(format!("100644 blob {blob}\tother.txt\n").as_bytes()),
    );
    run(
        &["commit-tree", &tree, "-p", parent, "-m", "competing"],
        None,
    )
}

/// Every coordinator entry point an owner relies on must still work (nothing is wedged).
fn assert_not_wedged(f: &Fixture, second_key: &str) {
    for (a, args) in [
        ("owner-key-status", vec![]),
        ("reconcile", vec![]),
        ("reconcile-effects", vec![]),
        ("sweep", vec!["--backend", "sink"]),
        ("closure-summary", vec!["w", "failed"]),
        ("check", vec![]),
        ("inbox", vec![]),
    ] {
        let o = f.run(a, &args);
        assert!(
            o.status.success(),
            "{a} wedged: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let o = f.run("permit", &["p2", "fixture:other"]);
    assert!(
        o.status.success(),
        "permit wedged: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let sub = serde_json::to_vec(&json!({"protocol_version": 1, "request_key": second_key,
        "fixture_id": "fixture:symbol",
        "success_criteria": "Prepare synthetic symbol correction; capture remains incomplete",
        "source_evidence": ["fixture:malformed-symbol"], "permit_id": "p"}))
    .unwrap();
    let o = f.send_bytes("submit", &sub);
    assert!(
        o.status.success(),
        "submit wedged: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// Live post-closure discrepancy responsibilities: one blocking item per adverse fact.
fn post_closure_items(f: &Fixture) -> i64 {
    f.count("SELECT count(*) FROM attention WHERE source_key LIKE 'discrepancy:%' AND state<>'resolved' AND severity='urgent' AND blocking=1")
}

#[test]
fn closure_retires_pending_launch_intent_and_closed_work_never_launches() {
    // Sequence: intent -> reproduce -> cancel -> close(cancelled) -> stale launch.
    let f = Fixture::new();
    f.json("symbol-intent", &["w", "a"]);
    f.json("reproduce", &["a"]);
    let wid = f.work_id();
    let w0 = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w0.5,
        &json!({"reason": "stop"}),
    );
    let s = f.closure("cancelled");
    assert_eq!(s["eligible"], true, "{s}");
    assert_eq!(
        s["evidence"]["pending_launch_intents_retired_by_closure"],
        json!(["a"]),
        "the owner reviews the retirement before closing: {s}"
    );
    let w = f.work();
    f.command(
        "close",
        &format!("work:{wid}"),
        "cl",
        w.5,
        &json!({"outcome": "cancelled", "reason": "stopped", "evidence_digest": s["digest"]}),
    );
    // Retired durably in the closing transaction.
    assert!(
        f.text("SELECT reason FROM attempt WHERE id='a'")
            .contains("retired")
    );
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='attempt.retired' AND entity_id='a'"),
        1
    );
    let closed = f.work();
    assert_eq!(closed.0, "closed");
    // The stale launch is refused before any supervisor or workload runs.
    let err = f.fails("run-fake", &["w", "a", "prepare_symbol", "1000"]);
    assert!(err.contains("closed"), "{err}");
    assert_eq!(f.count("SELECT count(*) FROM supervisor_command"), 0);
    assert_eq!(f.count("SELECT count(*) FROM supervisor_fact"), 0);
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "intent");
    assert_eq!(f.work(), closed, "a closed row is never asked to change");
    assert!(f.fails("seal", &["a"]).contains("closed"));
    assert_not_wedged(&f, "w2");
    assert_eq!(f.work(), closed);
    assert_eq!(post_closure_items(&f), 0, "nothing adverse happened");

    // Same for abandonment of a work whose only attempt is an un-launched intent.
    let g = Fixture::new();
    g.json("symbol-intent", &["w", "a"]);
    let wid = g.work_id();
    let w = g.work();
    g.command(
        "abandon",
        &format!("work:{wid}"),
        "ab",
        w.5,
        &json!({"reason": "not needed", "residual_risk": "none known"}),
    );
    assert!(
        g.text("SELECT reason FROM attempt WHERE id='a'")
            .contains("retired")
    );
    assert!(g.fails("reproduce", &["a"]).contains("closed"));
    assert!(
        g.fails("run-fake", &["w", "a", "prepare_symbol", "1000"])
            .contains("closed")
    );
    assert_eq!(g.count("SELECT count(*) FROM supervisor_command"), 0);
}

#[test]
fn adverse_evidence_after_closure_cannot_wedge_startup() {
    // (a) Launch evidence arriving for an already-closed work: exactly the ledger the
    // an earlier build produced. Startup must not wedge.
    let f = Fixture::new();
    f.json("symbol-intent", &["w", "a"]);
    let wid = f.work_id();
    let w0 = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w0.5,
        &json!({"reason": "stop"}),
    );
    f.close("cancelled", "cl");
    let closed = f.work();
    f.exec(
        "INSERT INTO supervisor_command VALUES('launch:a','a',(SELECT spec_hash FROM attempt WHERE id='a'),1,999999999,'0','boot',1);
         INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','launch_accepted',1,'{}');",
    );
    f.json("owner-key-status", &[]);
    assert_eq!(f.work(), closed, "closed work is not reopened or rewritten");
    assert_eq!(
        f.text("SELECT state FROM attempt WHERE id='a'"),
        "unknown",
        "the adverse evidence is still reconciled on the attempt"
    );
    assert_eq!(
        post_closure_items(&f),
        1,
        "and raised to the owner, not dropped"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.discrepancy_opened'"),
        1
    );
    f.json("owner-key-status", &[]);
    assert_eq!(post_closure_items(&f), 1, "repeated startup is idempotent");
    assert_not_wedged(&f, "w2");
    assert_eq!(post_closure_items(&f), 1);
    let i = f.inbox().to_string();
    assert!(i.contains("DISCREPANCY"), "{i}");

    // (b) The sealed candidate disappears after a SUCCEEDED closure.
    let g = Fixture::new();
    g.applied();
    g.close("succeeded", "cl");
    let closed = g.work();
    let art = g.text("SELECT id FROM artifact WHERE role='candidate'");
    fs::rename(
        g.root.join("artifacts").join(&art),
        g.root.join("artifacts").join(format!("{art}-moved")),
    )
    .unwrap();
    g.json("owner-key-status", &[]);
    assert_eq!(
        g.text(&format!("SELECT state FROM artifact WHERE id='{art}'")),
        "invalid"
    );
    assert_eq!(g.work(), closed);
    assert_eq!(post_closure_items(&g), 1);
    assert_eq!(
        g.text("SELECT outcome FROM work_outcome"),
        "succeeded",
        "outcome is immutable"
    );
    assert_not_wedged(&g, "w2");
    // The discrepancy is a blocking responsibility: a reason alone cannot resolve it.
    let it = discrepancy_items(&g).remove(0);
    assert!(
        g.command_fails(
            "resolve",
            &format!("attention:{}", it["attention_id"].as_str().unwrap()),
            "r1",
            it["version"].as_i64().unwrap(),
            &json!({"reason": "inspected: artifact moved by me"}),
        )
        .contains("blocking")
    );
    assert_eq!(post_closure_items(&g), 1);
}

#[test]
fn cancellation_is_a_persistent_execution_fence() {
    // (a) Cancel with an open review, then new review -> approve -> dispatch
    // -> close(succeeded). Every forward step after the cancel must be refused.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let wid = f.work_id();
    let w = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w.5,
        &json!({"reason": "owner cancels"}),
    );
    let err = f.fails("review-create", &["a"]);
    assert!(err.contains("cancel"), "{err}");
    assert_eq!(f.count("SELECT count(*) FROM review"), 1);
    let t = f.json("decision-template", &[&rid, "d1", "approve"]);
    assert!(!f.send("decide", &t).status.success());
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
    let s = f.closure("succeeded");
    assert_eq!(s["eligible"], false);
    assert!(s["blockers"].to_string().contains("cancellation"), "{s}");
    // The request stays a visible, blocking obligation that cannot be resolved away.
    let item = f.open_item("responsibility");
    assert!(
        item["reason"].as_str().unwrap().contains("cancellation"),
        "{item}"
    );
    assert_eq!(item["blocking"], true);
    assert!(
        f.command_fails(
            "resolve",
            &format!("attention:{}", item["attention_id"].as_str().unwrap()),
            "r1",
            item["version"].as_i64().unwrap(),
            &json!({"reason": "never mind"})
        )
        .contains("blocking")
    );
    // No implicit withdrawal: a second cancel is refused, the request row is unchanged.
    let w = f.work();
    assert!(
        f.command_fails(
            "cancel",
            &format!("work:{wid}"),
            "c2",
            w.5,
            &json!({"reason": "again"})
        )
        .contains("already")
    );
    f.close("cancelled", "cl");
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "cancelled");

    // (b) Cancel with NO open review must still change the responsibility,
    // deadline and attention, and stop preparation.
    let g = Fixture::new();
    g.json("symbol-intent", &["w", "a"]);
    let w0 = g.work();
    let wid = g.work_id();
    let items0 = g.inbox()["items"].as_array().unwrap().len();
    g.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w0.5,
        &json!({"reason": "stop"}),
    );
    let w1 = g.work();
    assert_eq!(
        (w1.1.as_str(), w1.2.as_str(), w1.3.as_str()),
        (
            "blocked",
            "local-owner",
            "cancellation_requested_account_and_close"
        )
    );
    assert!(w1.5 > w0.5);
    assert!(g.inbox()["items"].as_array().unwrap().len() > items0);
    for (a, args) in [
        ("reproduce", vec!["a"]),
        ("run-fake", vec!["w", "a", "prepare_symbol", "1000"]),
        ("seal", vec!["a"]),
    ] {
        let e = g.fails(a, &args);
        assert!(e.contains("cancel"), "{a}: {e}");
    }
    assert_eq!(g.count("SELECT count(*) FROM supervisor_command"), 0);
    assert_eq!(
        g.work(),
        w1,
        "refusals do not move the responsibility or deadline"
    );

    // (c) Cancel after a confirmed effect: no ordinary success afterwards; cancelled with the
    // effect disclosed remains available.
    let h = Fixture::new();
    h.applied();
    assert_eq!(h.closure("succeeded")["eligible"], true);
    let wid = h.work_id();
    let w = h.work();
    h.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w.5,
        &json!({"reason": "stop"}),
    );
    let s = h.closure("succeeded");
    assert_eq!(s["eligible"], false, "{s}");
    let s = h.closure("cancelled");
    assert_eq!(s["eligible"], true, "{s}");
    assert!(
        s["evidence"]["residual_risks"]
            .to_string()
            .contains("not undone")
    );
}

/// Dispatch killed at `boundary`, then a competing writer moves the ref: effect `conflict`.
/// Before the CAS our contribution is unproven; after it the audit entry attributes it.
fn conflicted(boundary: &str) -> Fixture {
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let old = r["envelope"]["action"]["expected_old"]
        .as_str()
        .unwrap()
        .to_string();
    let new = r["envelope"]["action"]["new_commit"]
        .as_str()
        .unwrap()
        .to_string();
    let eff = f.approve(&rid, "d1");
    let mut dc = f.cmd("dispatch");
    dc.arg(&eff);
    let (child, _b) = f.paused(dc, boundary);
    kill(child);
    let parent = if boundary == "before_ref_cas" {
        old
    } else {
        new
    };
    let third = third_commit(&f, &parent);
    f.git(&["update-ref", "refs/heads/fixture", &third, &parent]);
    f.json("reconcile-effects", &[]);
    assert_eq!(f.text("SELECT state FROM effect"), "conflict");
    f
}

#[test]
fn conflicted_effect_closure_respects_attribution_and_keeps_responsibility() {
    // Conflict attribution and responsibility.
    // (a) Unproven contribution: no outcome may close it; the named reconciliation
    // responsibility, its attention and its deadline stay exactly as they were.
    let f = conflicted("before_ref_cas");
    let obs: Value = serde_json::from_str(
        &f.text("SELECT payload_json FROM effect_observation ORDER BY rowid DESC LIMIT 1"),
    )
    .unwrap();
    assert_ne!(obs["attribution"], "audit_entry");
    let before = f.work();
    assert_eq!(before.3, "inspect_conflicting_target_no_automatic_rollback");
    for o in ["failed", "abandoned"] {
        let s = f.closure(o);
        assert_eq!(s["eligible"], false, "{o}: {s}");
        assert!(s["blockers"].to_string().contains("unproven"), "{o}: {s}");
    }
    let wid = f.work_id();
    assert!(
        !f.command_fails(
            "abandon",
            &format!("work:{wid}"),
            "ab",
            before.5,
            &json!({"reason": "give up", "residual_risk": "accept"})
        )
        .is_empty()
    );
    let w = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w.5,
        &json!({"reason": "stop"}),
    );
    assert_eq!(f.closure("cancelled")["eligible"], false);
    f.sweep();
    let after = f.work();
    assert_eq!(
        (after.0.as_str(), after.3.as_str()),
        ("active", "inspect_conflicting_target_no_automatic_rollback")
    );
    assert_eq!(
        after.4, before.4,
        "deadline preserved: not moved by cancel, sweep or refusals"
    );
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 0);
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='responsibility' AND blocking=1 AND state<>'resolved' AND source_key LIKE '%inspect_conflicting_target%'"),
        1,
        "the named obligation stays live"
    );

    // (b) Known application, later overwritten: failed would conceal the applied effect;
    // abandonment discloses it with attribution-specific text (not "unproven").
    let g = conflicted("after_ref_cas");
    let s = g.closure("failed");
    assert_eq!(s["eligible"], false, "{s}");
    assert!(s["blockers"].to_string().contains("applied"), "{s}");
    let s = g.closure("abandoned");
    assert_eq!(s["eligible"], true, "{s}");
    let risks = s["evidence"]["residual_risks"].to_string();
    assert!(
        risks.contains("applied") && risks.contains("overwritten") && risks.contains("audit_entry"),
        "{risks}"
    );
    assert!(!risks.contains("unproven"), "{risks}");
    let wid = g.work_id();
    let w = g.work();
    g.command(
        "abandon",
        &format!("work:{wid}"),
        "ab",
        w.5,
        &json!({"reason": "give up", "residual_risk": "owner accepts the overwritten target"}),
    );
    let n = g.open_item("outcome");
    assert!(
        n["reason"].as_str().unwrap().contains("RESIDUAL RISK"),
        "{n}"
    );
    assert_eq!(n["severity"], "urgent");
    // The applied-then-overwritten consequence keeps a live, blocking,
    // uniquely identified responsibility after closure, restart, ack and notice resolution.
    let eff = g.text("SELECT id FROM effect");
    let key = format!("discrepancy:{wid}:effect:{eff}:conflict_applied_overwritten");
    let live = |g: &Fixture| {
        g.count(&format!("SELECT count(*) FROM attention WHERE source_key='{key}' AND blocking=1 AND state<>'resolved'"))
    };
    assert_eq!(
        live(&g),
        1,
        "REVIEW: attributed overwritten consequence has no live responsibility"
    );
    let reason = g.text(&format!(
        "SELECT reason FROM attention WHERE source_key='{key}' AND state<>'resolved'"
    ));
    assert!(
        reason.contains("audit_entry")
            && reason.contains("observation")
            && reason.contains("deadline"),
        "{reason}"
    );
    let it = discrepancy_items(&g).remove(0);
    g.command(
        "ack",
        &format!("attention:{}", it["attention_id"].as_str().unwrap()),
        "ack1",
        it["version"].as_i64().unwrap(),
        &json!({}),
    );
    g.command(
        "resolve",
        &format!("attention:{}", n["attention_id"].as_str().unwrap()),
        "rn",
        n["version"].as_i64().unwrap(),
        &json!({"reason": "read the notice"}),
    );
    g.sweep();
    g.json("owner-key-status", &[]);
    assert_eq!(live(&g), 1);
}

/// Invariant across supported terminal outcomes: every unresolved consequential fact either
/// prevents closure or retains live, discoverable responsibility afterward.
#[test]
fn every_unresolved_consequential_fact_blocks_closure_or_keeps_live_responsibility() {
    for scenario in [
        "conflict_unattributed",
        "conflict_attributed",
        "artifact_lost",
    ] {
        for outcome in ["succeeded", "failed", "rejected", "cancelled", "abandoned"] {
            let f = match scenario {
                "conflict_unattributed" => conflicted("before_ref_cas"),
                "conflict_attributed" => conflicted("after_ref_cas"),
                _ => {
                    let f = Fixture::new();
                    let art = f.validated();
                    move_artifact(&f, &art);
                    f.json("owner-key-status", &[]);
                    f
                }
            };
            let wid = f.work_id();
            if outcome == "cancelled" {
                let w = f.work();
                f.command(
                    "cancel",
                    &format!("work:{wid}"),
                    "c1",
                    w.5,
                    &json!({"reason": "stop"}),
                );
            }
            let s = f.closure(outcome);
            let w = f.work();
            let attempt = if outcome == "abandoned" {
                f.send(
                    "command",
                    &f.signed(
                        "abandon",
                        &format!("work:{wid}"),
                        "x",
                        w.5,
                        &json!({"reason": "r", "residual_risk": "r"}),
                    ),
                )
            } else {
                f.send(
                    "command",
                    &f.signed(
                        "close",
                        &format!("work:{wid}"),
                        "x",
                        w.5,
                        &json!({"outcome": outcome, "reason": "r", "evidence_digest": s["digest"]}),
                    ),
                )
            };
            f.sweep();
            f.json("owner-key-status", &[]);
            let live_blocking = f.count(&format!("SELECT count(*) FROM attention WHERE work_id='{wid}' AND blocking=1 AND state<>'resolved'"));
            if attempt.status.success() {
                assert_eq!(f.work().0, "closed");
                assert!(
                    post_closure_items(&f) >= 1,
                    "{scenario}/{outcome}: closed without live responsibility"
                );
                assert_eq!(
                    f.inbox()["closed_outcomes"][0]["challenged"],
                    true,
                    "{scenario}/{outcome}"
                );
            } else {
                assert_eq!(s["eligible"], false, "{scenario}/{outcome}");
                assert_eq!(f.work().0, "active", "{scenario}/{outcome}");
            }
            assert!(
                live_blocking >= 1,
                "{scenario}/{outcome}: no live responsibility"
            );
        }
    }
}

#[test]
fn slow_sweep_preparation_stays_within_the_whole_budget() {
    // Each preparation item is slowed (test seam); the budget must cut INSIDE the batch.
    let f = Fixture::new();
    for n in 0..24 {
        let sub = serde_json::to_vec(
            &json!({"protocol_version": 1, "request_key": format!("x{n}"),
            "fixture_id": "fixture:symbol",
            "success_criteria": "Prepare synthetic symbol correction; capture remains incomplete",
            "source_evidence": ["fixture:malformed-symbol"], "permit_id": "p"}),
        )
        .unwrap();
        assert!(f.send_bytes("submit", &sub).status.success());
    }
    f.set_clock(60_000);
    let start = Instant::now();
    let child = f
        .cmd("sweep")
        .args(["--backend", "sink"])
        .env("ASHER_TEST_SWEEP_BUDGET_MS", "1000")
        .env("ASHER_TEST_PREP_ITEM_DELAY_MS", "300")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = bounded(child, Duration::from_secs(20));
    let took = start.elapsed();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        took < Duration::from_millis(2500),
        "preparation overran the budget: {took:?} {s}"
    );
    assert_eq!(s["complete"], false, "{s}");
    let n = s["overdue_recorded"].as_i64().unwrap();
    assert!((1..20).contains(&n), "stopped inside the batch: {s}");
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue' AND state='open'"),
        n,
        "partial progress committed"
    );
    f.sweep();
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue' AND state='open'"),
        25
    );
}

/// A hostile session bus: completes AUTH and Hello, then floods unrelated signals and never
/// answers Notify.
fn flooding_bus(path: &Path) -> thread::JoinHandle<()> {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let l = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            thread::spawn(move || {
                let mut got = Vec::new();
                let mut b = [0u8; 1];
                while !got.ends_with(b"\r\n") {
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    got.push(b[0]);
                }
                if s.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")
                    .is_err()
                {
                    return;
                }
                let mut begin = [0u8; 7];
                let mut head = [0u8; 16];
                if s.read_exact(&mut begin).is_err() || s.read_exact(&mut head).is_err() {
                    return;
                }
                let body = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
                let fields = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
                let pad = (8 - (16 + fields) % 8) % 8;
                let mut rest = vec![0u8; fields + pad + body];
                if s.read_exact(&mut rest).is_err() {
                    return;
                }
                // Hello reply: METHOD_RETURN with REPLY_SERIAL=1.
                let mut reply = vec![b'l', 2, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0];
                reply.extend_from_slice(&[5, 1, b'u', 0, 1, 0, 0, 0]);
                if s.write_all(&reply).is_err() {
                    return;
                }
                // Then an unrelated SIGNAL every 200 ms, forever.
                let mut serial = 2u32;
                loop {
                    let mut sig = vec![b'l', 4, 0, 1, 0, 0, 0, 0];
                    sig.extend_from_slice(&serial.to_le_bytes());
                    sig.extend_from_slice(&[0, 0, 0, 0]);
                    if s.write_all(&sig).is_err() {
                        return;
                    }
                    serial += 1;
                    thread::sleep(Duration::from_millis(200));
                }
            });
        }
    })
}

/// Wait for a child with a hard limit, so a red (unbounded) run fails instead of hanging.
fn bounded(mut child: Child, limit: Duration) -> Output {
    let end = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!(
                "still running after {limit:?} (unbounded): {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn desktop_delivery_and_sweep_have_absolute_time_budgets() {
    // (a) Per-notification budget against a hostile bus.
    let f = Fixture::new();
    f.validated();
    f.review();
    let bus = tempfile::tempdir().unwrap();
    let path = bus.path().join("bus");
    let _h = flooding_bus(&path);
    let start = Instant::now();
    let child = f
        .cmd("sweep")
        .args(["--backend", "desktop"])
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", path.display()),
        )
        .env("ASHER_TEST_NOTIFY_BUDGET_MS", "1500")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = bounded(child, Duration::from_secs(20));
    let took = start.elapsed();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(took < Duration::from_secs(8), "sweep not bounded: {took:?}");
    let s: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(s["attempts"][0]["state"], "unknown", "{s}");
    // The lock is released: owner commands work again immediately.
    f.json("owner-key-status", &[]);
    assert_eq!(
        f.open_item("responsibility")["state"],
        "open",
        "obligation stays open"
    );

    // (b) Whole-sweep budget: a slow backend cannot hold the coordinator lock past it.
    let g = Fixture::new();
    g.json("symbol-intent", &["w", "a"]);
    let sub = serde_json::to_vec(&json!({"protocol_version": 1, "request_key": "w2",
        "fixture_id": "fixture:symbol",
        "success_criteria": "Prepare synthetic symbol correction; capture remains incomplete",
        "source_evidence": ["fixture:malformed-symbol"], "permit_id": "p"}))
    .unwrap();
    assert!(g.send_bytes("submit", &sub).status.success());
    g.json("symbol-intent", &["w2", "b"]);
    g.set_clock(60_000); // both coordinator deadlines are now overdue: two owner items
    g.sink_mode("slow:5000");
    let start = Instant::now();
    let child = g
        .cmd("sweep")
        .args(["--backend", "sink"])
        .env("ASHER_TEST_SWEEP_BUDGET_MS", "1500")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = bounded(child, Duration::from_secs(30));
    let took = start.elapsed();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        took < Duration::from_secs(5),
        "sweep budget not enforced: {took:?} {s}"
    );
    assert_eq!(s["budget_exhausted"], true, "{s}");
    assert!(s["deferred_by_budget"].as_i64().unwrap() >= 1, "{s}");
    assert_eq!(s["attempts"].as_array().unwrap().len(), 1, "{s}");
    assert_eq!(s["attempts"][0]["state"], "unknown");
    assert_eq!(
        g.count("SELECT count(*) FROM attention WHERE kind='overdue' AND state='open'"),
        2,
        "deferred delivery never clears an obligation"
    );
    assert_eq!(
        g.count("SELECT count(*) FROM delivery_attempt WHERE state='attempted'"),
        0
    );
}

#[test]
fn migrated_confirmed_work_gets_current_closure_wording() {
    // A schema-5 ledger left confirmed, attributed work on the stale action that
    // says closure is "not implemented". Startup repairs it with an audited transition.
    let f = Fixture::new();
    f.applied();
    let before = f.work();
    f.exec("UPDATE work SET next_action='action_applied_verify_and_close_task5_not_implemented' WHERE request_key='w'");
    f.json("owner-key-status", &[]);
    let w = f.work();
    assert_eq!(w.3, "action_applied_review_closure_summary_and_close");
    assert_eq!(w.4, before.4, "deadline preserved");
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.legacy_wording_repaired'"),
        1
    );
    f.sweep();
    let i = f.inbox().to_string();
    assert!(!i.contains("not_implemented"), "{i}");
    assert!(
        !f.sink_lines()
            .iter()
            .any(|l| l.to_string().contains("not_implemented"))
    );
    f.json("owner-key-status", &[]);
    assert_eq!(f.work().5, w.5, "repair is idempotent");
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.legacy_wording_repaired'"),
        1
    );
}

#[test]
fn notification_failures_crash_after_attempt_recorded_before_send() {
    // Killed after the attempt row commits, before any backend call.
    let f = Fixture::new();
    f.validated();
    f.review();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut c = f.cmd("sweep");
    c.args(["--backend", "sink"]);
    let (child, _d) = f.paused(c, "after_attempt_recorded");
    kill(child);
    assert_eq!(f.sink_lines().len(), 0, "nothing was displayed");
    assert_eq!(f.text("SELECT state FROM delivery_attempt"), "attempted");
    assert_eq!(
        f.open_item("responsibility")["delivery"]["status"],
        "attempted"
    );
    assert!(
        f.sweep()["attempts"].as_array().unwrap().is_empty(),
        "claim window holds"
    );
    f.set_clock(30_000);
    let s = f.sweep();
    assert_eq!(s["stale_attempts_marked_unknown"], 1);
    assert_eq!(
        f.text("SELECT group_concat(state,',') FROM (SELECT state FROM delivery_attempt ORDER BY ordinal)"),
        "unknown,accepted"
    );
    assert_eq!(f.sink_lines().len(), 1);
    assert_eq!(f.sink_lines()[0]["attention_id"], aid.as_str());
    assert_eq!(f.open_item("responsibility")["state"], "open");
}

#[test]
fn closure_crash_before_commit_leaves_nothing_and_retry_applies_once() {
    // Killed at `before_closure_commit`.
    let f = Fixture::new();
    f.applied();
    let wid = f.work_id();
    let s = f.closure("succeeded");
    let w = f.work();
    let t = f.signed(
        "close",
        &format!("work:{wid}"),
        "c-1",
        w.5,
        &json!({"outcome": "succeeded", "reason": "ok", "evidence_digest": s["digest"]}),
    );
    let b = tempfile::tempdir().unwrap();
    let mut child = f
        .cmd("command")
        .env("ASHER_TEST_BARRIER", "before_closure_commit")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&t).unwrap())
        .unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    while !b.path().join("before_closure_commit").exists() {
        assert!(Instant::now() < end, "barrier not reached");
        thread::sleep(Duration::from_millis(5));
    }
    kill(child);
    assert_eq!(f.work(), w, "nothing closed");
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 0);
    assert_eq!(f.count("SELECT count(*) FROM owner_command"), 0);
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='outcome'"),
        0
    );
    let r = f.send("command", &t);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let r2: Value = serde_json::from_slice(&f.send("command", &t).stdout).unwrap();
    assert_eq!(r2["disposition"], "existing");
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='outcome'"),
        1
    );
    assert_eq!(f.count("SELECT count(*) FROM owner_command"), 1);
}

#[test]
fn cancellation_survives_later_transitions_and_post_closure_commands_are_refused() {
    // Sequence: prepare -> cancel (cancellation responsibility) -> adverse artifact change
    // (inspection responsibility) -> the cancellation must stay visible and fenced, success
    // must stay unavailable; after closure, owner commands that would progress the work are
    // refused WITHOUT raising spurious post-closure notices.
    let f = Fixture::new();
    let art = f.validated();
    let wid = f.work_id();
    let w = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w.5,
        &json!({"reason": "stop"}),
    );
    assert_eq!(f.work().3, "cancellation_requested_account_and_close");
    fs::rename(
        f.root.join("artifacts").join(&art),
        f.root.join("artifacts").join(format!("{art}-moved")),
    )
    .unwrap();
    f.json("owner-key-status", &[]);
    assert_eq!(f.work().3, "inspect_invalid_artifact");
    let item = f.open_item("responsibility");
    assert!(
        item["reason"]
            .as_str()
            .unwrap()
            .contains("CANCELLATION REQUESTED"),
        "{item}"
    );
    assert!(f.fails("revalidate", &[&art]).contains("cancel"));
    assert!(f.fails("review-create", &["a"]).contains("cancel"));
    assert_eq!(f.closure("succeeded")["eligible"], false);
    f.close("cancelled", "cl");
    let closed = f.work();
    for (a, args) in [
        ("revalidate", vec![art.as_str()]),
        ("review-create", vec!["a"]),
        ("seal", vec!["a"]),
        ("validate", vec![art.as_str()]),
    ] {
        let e = f.fails(a, &args);
        assert!(e.contains("closed"), "{a}: {e}");
    }
    assert_eq!(f.work(), closed);
    // The invalid artifact was an open inspection obligation at closure: it is carried
    // forward as exactly one discrepancy; refused commands add nothing.
    assert_eq!(
        post_closure_items(&f),
        1,
        "carried forward, and refused commands are not adverse evidence"
    );
}

// ============================================================ Gate 5 further regressions
// Further regressions found in review. Each
// asserts the final DURABLE obligation state (rows), not only messages or exit codes.

fn discrepancy_items(f: &Fixture) -> Vec<Value> {
    f.inbox()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["reason"]
                .as_str()
                .unwrap_or_default()
                .starts_with("DISCREPANCY")
        })
        .cloned()
        .collect()
}
fn move_artifact(f: &Fixture, id: &str) {
    fs::rename(
        f.root.join("artifacts").join(id),
        f.root.join("artifacts").join(format!("{id}-moved")),
    )
    .unwrap();
}

#[test]
fn each_post_closure_fact_keeps_its_own_responsibility_until_evidence_resolves_it() {
    // A post-closure acknowledgement must not suppress a new discrepancy, nor resolve erase an obligation.
    let f = Fixture::new();
    f.applied();
    f.close("succeeded", "cl");
    let closed = f.work();
    let cand = f.text("SELECT id FROM artifact WHERE role='candidate'");
    let init = f.text("SELECT id FROM artifact WHERE role='initial'");
    move_artifact(&f, &cand);
    f.json("owner-key-status", &[]);
    let first = discrepancy_items(&f);
    assert_eq!(first.len(), 1);
    assert!(first[0]["reason"].as_str().unwrap().contains(&cand));
    let aid = first[0]["attention_id"].as_str().unwrap().to_string();
    // Acknowledge the first fact: seen, not resolved.
    f.command(
        "ack",
        &format!("attention:{aid}"),
        "ack1",
        first[0]["version"].as_i64().unwrap(),
        &json!({"note": "seen"}),
    );
    // A second, independent fact is discovered later.
    move_artifact(&f, &init);
    f.json("owner-key-status", &[]);
    assert_eq!(
        post_closure_items(&f),
        2,
        "one durable responsibility per fact"
    );
    assert_eq!(
        f.count(
            "SELECT count(*) FROM attention WHERE source_key LIKE 'discrepancy:%' AND state='open'"
        ),
        1,
        "the new fact is NOT covered by the earlier acknowledgment"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE source_key LIKE 'discrepancy:%' AND state='acknowledged'"),
        1
    );
    // A reason alone resolves neither; sweeps and restarts keep both.
    for it in discrepancy_items(&f) {
        let e = f.command_fails(
            "resolve",
            &format!("attention:{}", it["attention_id"].as_str().unwrap()),
            &format!("r-{}", it["version"]),
            it["version"].as_i64().unwrap(),
            &json!({"reason": "read it"}),
        );
        assert!(e.contains("blocking"), "{e}");
    }
    f.sweep();
    f.json("owner-key-status", &[]);
    assert_eq!(post_closure_items(&f), 2);
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.discrepancy_opened'"),
        2
    );
    // History is preserved and the outcome is shown as challenged, not silently successful.
    assert_eq!(f.work(), closed);
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "succeeded");
    let o = &f.inbox()["closed_outcomes"][0];
    assert_eq!(o["success"], true, "historical record unchanged: {o}");
    assert_eq!(o["challenged"], true, "{o}");
    assert_eq!(o["open_discrepancies"], 2, "{o}");
}

#[test]
fn post_success_ref_change_challenges_the_outcome_until_readback_restores_it() {
    // postclosure-ref.py: the fixture ref moves back after a SUCCEEDED closure.
    let f = Fixture::new();
    let (_r, effect, old, new) = f.applied();
    f.close("succeeded", "cl");
    f.git(&["update-ref", "refs/heads/fixture", &old, &new]);
    f.json("reconcile-effects", &[]);
    assert_eq!(post_closure_items(&f), 1);
    let d = discrepancy_items(&f).remove(0);
    let reason = d["reason"].as_str().unwrap();
    assert!(
        reason.contains(&effect) && reason.contains(&old),
        "{reason}"
    );
    assert_eq!(f.inbox()["closed_outcomes"][0]["challenged"], true);
    f.json("owner-key-status", &[]);
    assert_eq!(post_closure_items(&f), 1, "idempotent while the fact holds");
    // Supported evidence: a fresh coordinator readback shows the applied commit again.
    f.git(&["update-ref", "refs/heads/fixture", &new, &old]);
    f.json("owner-key-status", &[]);
    assert_eq!(post_closure_items(&f), 0);
    let res = f.text(&format!(
        "SELECT resolution FROM attention WHERE id='{}'",
        d["attention_id"].as_str().unwrap()
    ));
    assert!(res.contains("readback") && res.contains(&new), "{res}");
    assert_eq!(
        f.text(&format!(
            "SELECT resolved_by FROM attention WHERE id='{}'",
            d["attention_id"].as_str().unwrap()
        )),
        "coordinator"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.discrepancy_resolved'"),
        1
    );
    assert_eq!(f.inbox()["closed_outcomes"][0]["challenged"], false);
}

#[test]
fn failed_closure_requires_trusted_fresh_failure_evidence_even_after_cancel() {
    // (a) cancel-invalid-evidence.py: failing validation -> cancel -> artifact lost.
    let f = Fixture::new();
    let art = f.validated_failing();
    let wid = f.work_id();
    let w = f.work();
    f.command(
        "cancel",
        &format!("work:{wid}"),
        "c1",
        w.5,
        &json!({"reason": "stop"}),
    );
    move_artifact(&f, &art);
    let ev = f.json("evidence-status", &["a"]);
    assert!(
        ev.as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "candidate_validation" && e["trusted"] == false)
    );
    let s = f.closure("failed");
    assert_eq!(s["eligible"], false, "{s}");
    assert!(s["blockers"].to_string().contains("trusted"), "{s}");
    let w = f.work();
    assert!(
        !f.command_fails(
            "close",
            &format!("work:{wid}"),
            "cl-f",
            w.5,
            &json!({"outcome": "failed", "reason": "failure", "evidence_digest": s["digest"]})
        )
        .is_empty()
    );
    // The inspection responsibility survives the refused closure ...
    let w = f.work();
    assert_eq!(
        (w.0.as_str(), w.3.as_str()),
        ("active", "inspect_invalid_artifact")
    );
    assert_eq!(f.count("SELECT count(*) FROM work_outcome"), 0);
    // ... and a supported alternative disposition carries it forward instead of erasing it.
    let s = f.closure("cancelled");
    assert_eq!(s["eligible"], true, "{s}");
    assert!(
        s["evidence"]["carried_forward_discrepancies"]
            .to_string()
            .contains(&art),
        "{s}"
    );
    f.close("cancelled", "cl-c");
    assert_eq!(f.text("SELECT outcome FROM work_outcome"), "cancelled");
    assert_eq!(
        post_closure_items(&f),
        1,
        "the invalid-artifact obligation is carried, not erased"
    );
    assert!(
        discrepancy_items(&f)[0]["reason"]
            .as_str()
            .unwrap()
            .contains(&art)
    );

    // (b) Same without cancellation (failed closure with a lost validation artifact).
    let g = Fixture::new();
    let art = g.validated_failing();
    move_artifact(&g, &art);
    assert_eq!(g.closure("failed")["eligible"], false);

    // (c) Stale failure evidence is not excused either.
    let h = Fixture::new();
    h.validated_failing();
    h.set_clock(2 * HOUR);
    let s = h.closure("failed");
    assert_eq!(s["eligible"], false, "{s}");
    assert!(s["blockers"].to_string().contains("fresh"), "{s}");
    // With trusted fresh evidence the failure closes normally.
    let k = Fixture::new();
    k.validated_failing();
    assert_eq!(k.closure("failed")["eligible"], true);
}

#[test]
fn whole_sweep_is_bounded_with_durable_partial_progress() {
    // Preparation (overdue recording) is inside the budget.
    let f = Fixture::new();
    for n in 0..24 {
        let key = format!("x{n}");
        let sub = serde_json::to_vec(&json!({"protocol_version": 1, "request_key": key,
            "fixture_id": "fixture:symbol",
            "success_criteria": "Prepare synthetic symbol correction; capture remains incomplete",
            "source_evidence": ["fixture:malformed-symbol"], "permit_id": "p"}))
        .unwrap();
        assert!(f.send_bytes("submit", &sub).status.success());
    }
    let works = f.count("SELECT count(*) FROM work WHERE mode='active'");
    assert_eq!(works, 25);
    f.set_clock(60_000);
    let tight = |f: &Fixture| -> Value {
        let out = f
            .cmd("sweep")
            .args(["--backend", "sink"])
            .env("ASHER_TEST_SWEEP_BUDGET_MS", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    // Every bounded sweep commits progress (at least one item), never repeats one, never
    // loses one, and delivers nothing while preparation is unfinished (obligations retained).
    let mut total = 0i64;
    let mut sweeps = 0;
    while total < 25 {
        let s = tight(&f);
        sweeps += 1;
        assert!(sweeps <= 25, "no progress: {s}");
        let n = s["overdue_recorded"].as_i64().unwrap();
        assert!(n >= 1, "each bounded sweep progresses: {s}");
        assert!(n <= 20, "never more than one batch: {s}");
        assert_eq!(s["complete"], false, "{s}");
        assert_eq!(s["budget_exhausted"], true);
        total += n;
        assert_eq!(
            f.count("SELECT count(*) FROM attention WHERE kind='overdue' AND state='open'"),
            total,
            "committed partial progress, no duplicates"
        );
        assert_eq!(f.count("SELECT count(*) FROM delivery_attempt"), 0);
    }
    assert_eq!(total, 25);
    assert_eq!(
        f.count("SELECT count(*) FROM sweep_run WHERE json_extract(summary_json,'$.complete')=0"),
        sweeps
    );
    // An ordinary sweep completes and delivers.
    let s3 = f.sweep();
    assert_eq!(s3["complete"], true, "{s3}");
    assert_eq!(s3["overdue_recorded"], 0);
    assert_eq!(s3["attempts"].as_array().unwrap().len(), 20);
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='overdue' AND state='open'"),
        25,
        "delivery never resolves"
    );
}

// ============================================================ shutdown across reboot
/// Orderly shutdown with a review pending, then a SIMULATED host reboot (the fault-injection
/// boot seam points at a new boot id; no host is rebooted). The drained attempt keeps its
/// durable cleanup classification, the pending review and its attention item survive with the
/// same identities, the boot change is durable, and nothing is decided or dispatched.
/// Schema migration across every historical version is covered by
/// `migration_008_is_additive_from_every_historical_version` (tests/restart_time.rs).
#[test]
fn shutdown_and_migration() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let aid = f.open_item("responsibility")["attention_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Drained: no attempt is left in a live lifecycle state at shutdown.
    assert_eq!(
        f.count("SELECT count(*) FROM attempt WHERE state<>'cleaned'"),
        0
    );
    let before_boots = f.count("SELECT count(*) FROM host_boot");
    // Simulated reboot: every later process reads another boot identity.
    let boot = f.temp.path().join("boot_id");
    fs::write(&boot, "sim-boot-after-shutdown\n").unwrap();
    let after = |action: &str, args: &[&str]| -> Value {
        let o = f
            .cmd(action)
            .args(args)
            .env("ASHER_TEST_BOOT_ID_FILE", &boot)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap_or(Value::Null)
    };
    after("reconcile-effects", &[]);
    after("task4-status", &["a"]);
    // The boot change is durable and visible.
    assert_eq!(f.count("SELECT count(*) FROM host_boot"), before_boots + 1);
    assert_eq!(
        f.text("SELECT current_boot FROM host_boot ORDER BY seq DESC LIMIT 1"),
        "sim-boot-after-shutdown"
    );
    let d = after("diagnose", &[]);
    assert_eq!(d["ledger"]["host"]["boot_changes"], 1, "{d}");
    // Cleanup confirmed before shutdown is not a reboot gap.
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "cleaned");
    assert_eq!(f.count("SELECT count(*) FROM evidence_gap"), 0);
    // The pending review and its obligation survive with the same identities.
    assert_eq!(
        f.text(&format!("SELECT state FROM review WHERE id='{rid}'")),
        "pending"
    );
    assert_ne!(
        f.text(&format!("SELECT state FROM attention WHERE id='{aid}'")),
        "resolved"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM attention WHERE kind='responsibility' AND state<>'resolved'"),
        1
    );
    // Nothing was decided or dispatched by the restart.
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
}

// ============================================================ adversarial clock and deadline cases
/// (SIMULATED: fault-injection CLOCK_BOOTTIME offset; the host clock is never
/// changed). An attention item is snoozed for 10 minutes. Twenty minutes of suspend-aware
/// boot-local time then pass while the wall clock does not move (an unobserved rollback of 20
/// minutes, which the ledger itself records as a regression). The snooze deadline has passed in
/// truth; a rolled-back clock must not extend it, so the next sweep must deliver the item.
#[test]
fn review_r0_rollback_never_extends_a_snooze() {
    let f = Fixture::new();
    f.validated();
    let item = f.open_item("responsibility");
    let aid = item["attention_id"].as_str().unwrap().to_string();
    let boottime = f.temp.path().join("boottime-offset-ms");
    fs::write(&boottime, "0").unwrap();
    let with_boottime = |action: &str, args: &[&str]| -> Value {
        let o = f
            .cmd(action)
            .args(args)
            .env("ASHER_TEST_BOOTTIME_OFFSET_FILE", &boottime)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap_or(Value::Null)
    };
    f.command(
        "snooze",
        &format!("attention:{aid}"),
        "sn-r0",
        item["version"].as_i64().unwrap(),
        &json!({"reason": "r0", "duration_ms": 10 * MIN}),
    );
    // Control: while the snooze runs, nothing is delivered for the item.
    let s = with_boottime("sweep", &["--backend", "sink"]);
    assert!(
        !s["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["attention_id"] == aid.as_str()),
        "{s}"
    );
    // 20 minutes of boot-local time pass; the wall clock stays where it was.
    fs::write(&boottime, (20 * MIN).to_string()).unwrap();
    let s = with_boottime("sweep", &["--backend", "sink"]);
    let regressions = f.count("SELECT count(*) FROM time_anomaly WHERE kind='regression'");
    assert!(
        regressions >= 1,
        "the ledger did not even notice the rollback"
    );
    assert!(
        s["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["attention_id"] == aid.as_str()),
        "rolled-back wall clock extended a snooze past its deadline: {s}"
    );
    // The responsibility deadline (seconds after validation) is not hidden either.
    assert!(
        s["overdue_recorded"].as_i64().unwrap() >= 1,
        "rolled-back wall clock hid an overdue deadline: {s}"
    );
}

/// (SIMULATED rollback as above): `diagnose` observes the regression but writes
/// nothing: no anomaly row, no floor movement, no event, database bytes unchanged.
#[test]
fn review_r0_diagnose_under_rollback_writes_nothing() {
    let f = Fixture::new();
    f.validated();
    let boottime = f.temp.path().join("boottime-offset-ms");
    fs::write(&boottime, (20 * MIN).to_string()).unwrap();
    let files = || -> Vec<Option<Vec<u8>>> {
        ["ledger.db", "ledger.db-wal"]
            .iter()
            .map(|n| fs::read(f.root.join(n)).ok())
            .collect()
    };
    let snapshot = || {
        (
            f.count("SELECT count(*) FROM time_anomaly"),
            f.text("SELECT wall_ms||':'||boot||':'||boot_ms||':'||epoch FROM time_floor"),
            f.count("SELECT count(*) FROM event"),
            f.count("SELECT count(*) FROM host_boot"),
        )
    };
    let before = snapshot();
    let bytes_before = files();
    let o = f
        .cmd("diagnose")
        .env("ASHER_TEST_BOOTTIME_OFFSET_FILE", &boottime)
        .output()
        .unwrap();
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(d["ledger"]["time"]["authority"], "uncertain", "{d}");
    assert_eq!(snapshot(), before, "diagnose wrote time accounting");
    assert!(
        files() == bytes_before,
        "diagnose changed the database or WAL bytes"
    );
}

/// (SIMULATED: fault-injection CLOCK_BOOTTIME offset; the host clock is never
/// changed). A delivery attempt is recorded and the process dies before the backend call, so
/// the attempt holds a 10-second claim. Thirty seconds of boot-local time then pass while the
/// wall clock does not move (a rollback the ledger records). The claim has ended in truth; the
/// rolled-back clock must not extend it, so the sweep marks the attempt delivery-unknown.
#[test]
fn r1_rollback_never_extends_a_delivery_claim() {
    let f = Fixture::new();
    f.validated();
    f.review();
    let mut c = f.cmd("sweep");
    c.args(["--backend", "sink"]);
    let (child, _d) = f.paused(c, "after_attempt_recorded");
    kill(child);
    assert_eq!(f.text("SELECT state FROM delivery_attempt"), "attempted");
    let boottime = f.temp.path().join("boottime-offset-ms");
    fs::write(&boottime, (30 * 1000).to_string()).unwrap();
    let o = f
        .cmd("sweep")
        .args(["--backend", "sink"])
        .env("ASHER_TEST_BOOTTIME_OFFSET_FILE", &boottime)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let s: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        f.count("SELECT count(*) FROM time_anomaly WHERE kind='regression'") >= 1,
        "the ledger did not notice the rollback"
    );
    assert_eq!(
        s["stale_attempts_marked_unknown"], 1,
        "rolled-back wall clock extended a delivery claim: {s}"
    );
}

// ------------------------------------------------------------------ adversarial cases
/// SIMULATED clock rollback (fault-injection wall offset). Failure evidence that the ledger has
/// already seen go stale (floor advanced 2 h past it) must not become fresh again when the wall
/// clock is stepped back: a "failed" closure must stay ineligible.
#[test]
fn r1_rollback_never_refreshes_stale_failure_evidence_for_closure() {
    let h = Fixture::new();
    h.validated_failing();
    h.set_clock(2 * HOUR);
    let s = h.closure("failed");
    assert_eq!(s["eligible"], false, "{s}");
    // Any authority-time reader advances the floor; a reconcile does.
    h.run("reconcile", &[]);
    let floor: i64 = h
        .db()
        .query_row("SELECT wall_ms FROM time_floor", [], |r| r.get(0))
        .unwrap();
    h.set_clock(0);
    let d = h.json("diagnose", &[]);
    assert_eq!(
        d["ledger"]["time"]["authority"], "uncertain",
        "floor={floor} {d}"
    );
    let s = h.closure("failed");
    // The original probe used the success-asserting `command` helper; a refused
    // close is the correct outcome, so it now asserts ineligibility and the refusal itself.
    assert_eq!(
        s["eligible"], false,
        "rollback refreshed stale evidence: {s}"
    );
    let w = h.work();
    let e = h.command_fails(
        "close",
        &format!("work:{}", h.work_id()),
        "r1-f",
        w.5,
        &json!({"outcome": "failed", "reason": "r1 rollback probe", "evidence_digest": s["digest"]}),
    );
    assert!(e.contains("not fresh"), "{e}");
    assert_eq!(
        h.db()
            .query_row("SELECT mode FROM work", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "active"
    );
}

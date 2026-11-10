//! Task 4 public-boundary tests: exact review, owner decision, fixture ref effect and
//! reconciliation. Every fixture, repository and process is disposable and created
//! beneath a harness temp directory. Oracles are independent of product status labels:
//! SQL rows, the real `git` binary (config/hooks disabled) reading the fixture repo, the
//! ref's reflog, a test-only executor journal, and kernel process identities.
#![cfg(feature = "fault-injection")]
use asher_bridge::{
    Owner, Reader,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const HOUR: i64 = 3_600_000;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    offset: Cell<i64>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_permit_ms(23 * HOUR)
    }
    fn with_permit_ms(ms: i64) -> Self {
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
            now_ms().unwrap() + ms,
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
        Self {
            temp: t,
            root,
            offset: Cell::new(0),
        }
    }
    fn journal(&self) -> PathBuf {
        self.temp.path().join("executor.journal")
    }
    fn clock_file(&self) -> PathBuf {
        self.temp.path().join("clock-offset-ms")
    }
    /// Advance the injected clock for already-running processes too (test seam).
    fn set_clock(&self, ms: i64) {
        fs::write(self.clock_file(), ms.to_string()).unwrap();
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action)
            .arg(&self.root)
            .env("ASHER_TEST_CLOCK_OFFSET_MS", self.offset.get().to_string())
            .env("ASHER_TEST_CLOCK_OFFSET_FILE", self.clock_file())
            .env(
                "ASHER_TEST_BOOTTIME_OFFSET_FILE",
                self.temp.path().join("boottime-offset-ms"),
            )
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
    fn validated(&self) -> String {
        self.json("symbol-intent", &["w", "a"]);
        self.json("reproduce", &["a"]);
        self.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
        let art = self.json("seal", &["a"]);
        let id = art["id"].as_str().unwrap().to_string();
        let v = self.json("validate", &[&id]);
        assert_eq!(v["validation_passed"], true);
        id
    }
    fn review(&self) -> Value {
        self.json("review-create", &["a"])
    }
    fn template(&self, review: &str, id: &str, choice: &str) -> Value {
        self.json("decision-template", &[review, id, choice])
    }
    fn decide(&self, body: &Value) -> Output {
        self.decide_bytes(&serde_json::to_vec(body).unwrap())
    }
    fn decide_bytes(&self, body: &[u8]) -> Output {
        let mut c = self
            .cmd("decide")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(body).unwrap();
        c.wait_with_output().unwrap()
    }
    fn approve(&self, review: &str, id: &str) -> String {
        let out = self.decide(&self.template(review, id, "approve"));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["executed"], false);
        v["effect_id"].as_str().unwrap().into()
    }
    fn repo(&self) -> PathBuf {
        self.text("SELECT path FROM fixture_repo").into()
    }
    /// Real `git` as an independent oracle and competing writer: no system/global
    /// config, empty HOME, explicit GIT_DIR, scrubbed environment.
    fn git(&self, args: &[&str]) -> Output {
        let home = self.temp.path().join("git-home");
        fs::create_dir_all(&home).unwrap();
        Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", self.repo())
            .env("GIT_COMMITTER_NAME", "test writer")
            .env("GIT_COMMITTER_EMAIL", "writer@invalid")
            .env("GIT_AUTHOR_NAME", "test writer")
            .env("GIT_AUTHOR_EMAIL", "writer@invalid")
            .args(args)
            .output()
            .unwrap()
    }
    fn git_ok(&self, args: &[&str]) -> String {
        let o = self.git(args);
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn current_ref(&self) -> String {
        self.git_ok(&["rev-parse", "refs/heads/fixture"])
    }
    fn reflog(&self) -> Vec<String> {
        fs::read_to_string(self.repo().join("logs/refs/heads/fixture"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
    /// Ref mutations attributed to an effect by the shared reflog.
    fn ref_mutations(&self, effect: &str) -> usize {
        self.reflog()
            .iter()
            .filter(|l| l.ends_with(&format!("\tasher-effect {effect}")))
            .count()
    }
    /// Executor-side count of ref-update executions (test journal, append-only).
    fn cas_executions(&self) -> usize {
        fs::read_to_string(self.journal())
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("cas_begin"))
            .count()
    }
    /// Make a third commit with real git (distinct tree) for a competing writer.
    fn third_commit(&self, parent: &str) -> String {
        let blob = {
            let mut c = Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.temp.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_DIR", self.repo())
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(b"competing\n").unwrap();
            String::from_utf8(c.wait_with_output().unwrap().stdout)
                .unwrap()
                .trim()
                .to_string()
        };
        let mut mk = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.temp.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", self.repo())
            .arg("mktree")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        mk.stdin
            .take()
            .unwrap()
            .write_all(format!("100644 blob {blob}\tother.txt\n").as_bytes())
            .unwrap();
        let tree = String::from_utf8(mk.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        self.git_ok(&["commit-tree", &tree, "-p", parent, "-m", "competing writer"])
    }
    /// Reconcile and return the effect's latest independent classification.
    fn reconcile(&self, effect: &str) -> String {
        let v = self.json("reconcile-effects", &[]);
        v["effects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["effect_id"] == effect)
            .and_then(|e| e["classification"].as_str())
            .unwrap_or("none")
            .to_string()
    }
    fn status(&self) -> Value {
        self.json("task4-status", &["a"])
    }
    fn work(&self) -> (String, String, String, i64) {
        self.db()
            .query_row(
                "SELECT phase,next_owner,next_action,due_at FROM work WHERE request_key='w'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }
    fn effect_state(&self, id: &str) -> String {
        self.db()
            .query_row("SELECT state FROM effect WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    }
    fn review_state(&self, id: &str) -> String {
        self.db()
            .query_row("SELECT state FROM review WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    }
    fn assert_accounted(&self) {
        let r = Reader::open(&self.root).unwrap();
        r.assert_accounted().unwrap();
        for w in r.list().unwrap() {
            assert_eq!(w.mode, "active", "no closure exists in this slice");
        }
        assert_eq!(
            self.count("SELECT count(*) FROM effect e JOIN decision d ON d.id=e.decision_id JOIN review r ON r.id=e.review_id WHERE d.choice<>'approve' OR d.envelope_hash<>r.envelope_hash OR d.review_id<>e.review_id"),
            0,
            "every effect binds exactly one approving decision on its own envelope"
        );
        assert_eq!(
            self.count("SELECT count(*) FROM review r WHERE r.state='consumed' AND (SELECT count(*) FROM effect e WHERE e.review_id=r.id AND e.claim_epoch IS NOT NULL)<>1"),
            0,
            "every consumed review has exactly one claimed effect"
        );
    }
    /// Start a command paused at a named barrier.
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
    fn dispatch_paused(&self, effect: &str, name: &str) -> (Child, TempDir) {
        let mut c = self.cmd("dispatch");
        c.arg(effect);
        self.paused(c, name)
    }
}
/// SIGKILL a paused process and prove (kernel oracle) the executor is gone.
fn kill(mut child: Child) {
    let pid = child.id();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "killed executor {pid} still present"
    );
}
fn release(child: Child, dir: &TempDir) -> Output {
    fs::write(dir.path().join("release"), b"").unwrap();
    child.wait_with_output().unwrap()
}

// ------------------------------------------------------------------ happy path
#[test]
fn exact_review_applies_one_ref_update() {
    let f = Fixture::new();
    let artifact = f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap();
    let env = &r["envelope"];
    // Envelope binds the exact content, evidence, policy, permit and typed action.
    assert_eq!(env["action"]["kind"], "fixture.ref_compare_swap");
    assert_eq!(
        env["action"]["scope"],
        "update this disposable fixture ref only"
    );
    assert_eq!(env["permit"]["grants_final_action"], false);
    assert_eq!(env["candidate_artifact"]["id"], artifact);
    assert_eq!(env["evidence"].as_array().unwrap().len(), 3);
    assert_eq!(env["capture"]["restored"], false);
    assert!(
        env["containment"]
            .as_str()
            .unwrap()
            .contains("not isolation")
    );
    let old = env["action"]["expected_old"].as_str().unwrap().to_string();
    let new = env["action"]["new_commit"].as_str().unwrap().to_string();
    // Independent git oracle: repository is valid, base and proposed commits are real.
    assert_eq!(f.current_ref(), old);
    f.git_ok(&["fsck", "--strict", "--no-dangling"]);
    assert_eq!(f.git_ok(&["cat-file", "-t", &new]), "commit");
    assert_eq!(f.git_ok(&["rev-parse", &format!("{new}^")]), old);
    assert_eq!(
        f.git_ok(&["show", &format!("{new}:symbols.csv")]),
        "symbol,scope\nFIXTURE_OK,core"
    );
    assert_eq!(
        f.git_ok(&["show", &format!("{old}:symbols.csv")]),
        "symbol,scope\nBAD SYMBOL,core"
    );
    assert!(f.git_ok(&["remote"]).is_empty(), "fixture has no remote");
    assert_eq!(fs::read_dir(f.repo().join("hooks")).unwrap().count(), 0);
    assert!(f.repo().starts_with(f.temp.path()));
    let s = f.status();
    assert_eq!(s["pending_review"], rid);
    assert_eq!(s["work"]["phase"], "reviewing");
    assert_eq!(s["work"]["next_owner"], "local-owner");
    // Review creation itself is idempotent while valid.
    assert_eq!(f.review()["review_id"], rid);
    let effect = f.approve(rid, "d1");
    assert_eq!(f.cas_executions(), 0, "decision is not execution");
    assert_eq!(f.current_ref(), old);
    let d = f.json("dispatch", &[&effect]);
    assert_eq!(d["classification"], "confirmed_applied");
    assert_eq!(d["work_closed"], false);
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.ref_mutations(&effect), 1);
    assert_eq!(f.reflog().len(), 1);
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(f.review_state(rid), "consumed");
    // Replaying dispatch never re-executes a consumed approval.
    let again = f.json("dispatch", &[&effect]);
    assert_eq!(again["dispatched"], false);
    assert_eq!(f.cas_executions(), 1);
    let s = f.status();
    assert_eq!(s["action_result_is_work_completion"], false);
    assert_eq!(s["work_closed"], false);
    assert_eq!(s["effects"][0]["state"], "confirmed");
    assert_eq!(s["work"]["phase"], "finalizing");
    assert!(
        s["work"]["next_action"]
            .as_str()
            .unwrap()
            .contains("closure_summary_and_close")
    );
    // No new review over an applied effect.
    assert!(f.fails("review-create", &["a"]).contains("applied"));
    f.git_ok(&["fsck", "--strict", "--no-dangling"]);
    f.assert_accounted();
}

// ------------------------------------------------------------------ decisions
#[test]
fn decision_race() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let approve = f.template(&rid, "d-approve", "approve");
    let reject = f.template(&rid, "d-reject", "reject");
    // Real concurrent processes; SQLite and the coordinator lock serialize them.
    let mut outs = Vec::new();
    for _round in 0..20 {
        let a = {
            let mut c = f
                .cmd("decide")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&approve).unwrap())
                .unwrap();
            c
        };
        let b = {
            let mut c = f
                .cmd("decide")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&reject).unwrap())
                .unwrap();
            c
        };
        outs.push(a.wait_with_output().unwrap());
        outs.push(b.wait_with_output().unwrap());
        if f.count("SELECT count(*) FROM decision") == 1 {
            break;
        }
    }
    assert_eq!(
        f.count("SELECT count(*) FROM decision"),
        1,
        "one effective decision"
    );
    assert!(
        f.count("SELECT count(*) FROM effect") <= 1,
        "at most one effect"
    );
    let winner = f.text("SELECT id FROM decision");
    let (win, lose) = if winner == "d-approve" {
        (&approve, &reject)
    } else {
        (&reject, &approve)
    };
    // Replay of the winner returns its original receipt; the loser stays a conflict.
    let first: Value = serde_json::from_slice(&f.decide(win).stdout).unwrap();
    assert_eq!(first["disposition"], "existing");
    assert_eq!(first["decision_id"], winner.as_str());
    let l = f.decide(lose);
    assert!(!l.status.success());
    assert!(String::from_utf8_lossy(&l.stderr).contains("competing decision"));
    assert!(f.count("SELECT count(*) FROM event WHERE kind='decision.conflict'") >= 1);
    assert_eq!(f.count("SELECT count(*) FROM decision"), 1);
    if winner == "d-approve" {
        assert_eq!(f.effect_state(&f.text("SELECT id FROM effect")), "intent");
    } else {
        assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
        assert_eq!(f.review_state(&rid), "rejected");
        // A rejected review is never reset; reconsidering needs a new review.
        assert!(
            !f.decide(&f.template(&rid, "d-late", "approve"))
                .status
                .success()
        );
        let r2 = f.review();
        assert_ne!(r2["review_id"], rid.as_str());
    }
    assert_eq!(f.cas_executions(), 0);
    f.assert_accounted();
}

#[test]
fn same_decision_id_with_changed_bytes_conflicts() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let approve = f.template(&rid, "d1", "approve");
    let first = f.decide(&approve);
    assert!(first.status.success());
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    // Same ID, validly signed, different content: conflict, audited, no state change.
    let changed = f.template(&rid, "d1", "reject");
    let out = f.decide(&changed);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("different content"));
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='decision.conflict' AND entity_id='d1'"),
        1
    );
    // Same content in different key order is the same request.
    let mut reordered = serde_json::Map::new();
    let obj = approve.as_object().unwrap();
    for k in obj.keys().rev() {
        reordered.insert(k.clone(), obj[k].clone());
    }
    let effect = first["effect_id"].as_str().unwrap().to_string();
    f.json("dispatch", &[&effect]);
    // Identical retry returns the recorded receipt even after the review was consumed.
    let replay: Value =
        serde_json::from_slice(&f.decide(&Value::Object(reordered)).stdout).unwrap();
    assert_eq!(replay["disposition"], "existing");
    assert_eq!(replay["effect_id"], first["effect_id"]);
    assert_eq!(replay["recorded_at"], first["recorded_at"]);
    assert_eq!(f.count("SELECT count(*) FROM decision"), 1);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 1);
    assert_eq!(f.cas_executions(), 1);
    f.assert_accounted();
}

#[test]
fn owner_role_forgery_is_rejected() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let good = f.template(&rid, "d1", "approve");
    let mut forged_actor = good.clone();
    forged_actor["actor"] = json!("agent");
    let mut wrong_proof = good.clone();
    wrong_proof["owner_proof"] = json!("0".repeat(64));
    let mut role_field = good.clone();
    role_field["role"] = json!("local-owner");
    let mut no_proof = good.clone();
    no_proof.as_object_mut().unwrap().remove("owner_proof");
    // A decision signed with another key (a forged credential) fails authentication.
    let mut other_key = good.clone();
    other_key["choice"] = json!("reject");
    for bad in [
        &forged_actor,
        &wrong_proof,
        &role_field,
        &no_proof,
        &other_key,
    ] {
        let out = f.decide(bad);
        assert!(!out.status.success(), "accepted forged decision {bad}");
    }
    let dup = format!(
        "{{\"actor\":\"agent\",{}",
        &serde_json::to_string(&good).unwrap()[1..]
    );
    assert!(
        !f.decide_bytes(dup.as_bytes()).status.success(),
        "duplicate keys refused"
    );
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert!(f.count("SELECT count(*) FROM event WHERE kind='decision.forgery'") >= 3);
    assert_eq!(f.review_state(&rid), "pending");
    // An agent claim cannot become a decision or authority.
    let mut c = f
        .cmd("ingest-claim")
        .arg("a")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(&serde_json::to_vec(&json!({"source_epoch":"e","source_seq":1,"observed_at":now_ms().unwrap(),"coverage":"complete","gaps":[],"payload":{"decision":"approve","review_id":rid}})).unwrap()).unwrap();
    assert!(c.wait_with_output().unwrap().status.success());
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    // The real owner decision still works afterwards.
    f.approve(&rid, "d1");
    f.assert_accounted();
}

// ------------------------------------------------------------------ staleness
#[test]
fn approval_staleness() {
    // (a) Post-review snapshot edit: review invalidated, no decision, no effect.
    let f = Fixture::new();
    let art = f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    fs::write(
        f.root.join("artifacts").join(&art).join("symbols.csv"),
        b"symbol,scope\nFIXTURE_OK,core\n",
    )
    .unwrap();
    let out = f.decide(&f.template(&rid, "d1", "approve"));
    assert!(!out.status.success());
    assert_eq!(f.review_state(&rid), "invalidated");
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert!(f.fails("review-create", &["a"]).contains("invalid"));
    f.assert_accounted();

    // (b) Target ref altered after approval, before dispatch: refused before claim.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let old = r["envelope"]["action"]["expected_old"]
        .as_str()
        .unwrap()
        .to_string();
    let effect = f.approve(&rid, "d1");
    let third = f.third_commit(&old);
    f.git_ok(&["update-ref", "refs/heads/fixture", &third, &old]);
    assert!(
        f.fails("dispatch", &[&effect])
            .contains("target ref changed")
    );
    assert_eq!(f.effect_state(&effect), "cancelled");
    assert_eq!(f.review_state(&rid), "invalidated");
    assert_eq!(f.current_ref(), third, "changed target preserved");
    assert_eq!(f.cas_executions(), 0);
    assert!(
        f.fails("review-create", &["a"])
            .contains("not the recorded base")
    );
    f.assert_accounted();

    // (c) Altered proposed commit object after review.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let new = r["envelope"]["action"]["new_commit"]
        .as_str()
        .unwrap()
        .to_string();
    let obj = f.repo().join("objects").join(&new[..2]).join(&new[2..]);
    fs::remove_file(&obj).unwrap();
    assert!(
        !f.decide(&f.template(&rid, "d1", "approve"))
            .status
            .success()
    );
    assert_eq!(f.review_state(&rid), "invalidated");

    // (d) Review expiry: expired review cannot be decided; no status reset.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let body = f.template(&rid, "d1", "approve");
    f.offset.set(HOUR + 60_000);
    assert!(!f.decide(&body).status.success());
    assert_eq!(f.review_state(&rid), "expired");
    assert_eq!(
        f.work().2,
        "review_or_evidence_expired_revalidate_then_new_review"
    );
    // The identical late decision stays refused; nothing resets the review.
    assert!(!f.decide(&body).status.success());
    assert_eq!(f.review_state(&rid), "expired");
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert!(
        f.db()
            .execute("UPDATE review SET state='pending'", [])
            .is_err(),
        "no status reset"
    );
    f.assert_accounted();
}

#[test]
fn evidence_expires_while_review_waits_then_bounded_revalidation() {
    let f = Fixture::new();
    let art = f.validated();
    let r1 = f.review();
    let rid1 = r1["review_id"].as_str().unwrap().to_string();
    let old_evidence: Vec<String> = r1["envelope"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().into())
        .collect();
    assert!(
        f.fails("revalidate", &[&art]).contains("open review"),
        "no revalidation while a valid review is open"
    );
    f.offset.set(HOUR + 60_000);
    // Status sweep retires the stale review (genuine transition, once).
    let s = f.status();
    assert_eq!(f.review_state(&rid1), "expired");
    assert!(s["pending_review"].is_null());
    let due = f.work().3;
    f.status();
    f.status();
    assert_eq!(
        f.work().3,
        due,
        "repeated status never postpones the deadline"
    );
    // Old timestamps are never extended: a new review needs new evidence.
    assert!(f.fails("review-create", &["a"]).contains("stale"));
    assert_eq!(f.work().2, "revalidate_trusted_evidence");
    let rv = f.json("revalidate", &[&art]);
    assert_eq!(rv["new_review_required"], true);
    assert!(f.fails("revalidate", &[&art]).contains("still fresh"));
    let r2 = f.review();
    let rid2 = r2["review_id"].as_str().unwrap().to_string();
    assert_ne!(rid1, rid2);
    assert_ne!(r1["envelope_hash"], r2["envelope_hash"]);
    for e in r2["envelope"]["evidence"].as_array().unwrap() {
        assert!(
            !old_evidence.contains(&e["id"].as_str().unwrap().to_string()),
            "stale evidence reused"
        );
    }
    // Same frozen artifact and same proposed commit; the old review stays expired.
    assert_eq!(
        r1["envelope"]["candidate_artifact"],
        r2["envelope"]["candidate_artifact"]
    );
    assert_eq!(
        r1["envelope"]["action"]["expected_old"],
        r2["envelope"]["action"]["expected_old"]
    );
    assert!(
        !f.decide(&f.template(&rid1, "old", "approve"))
            .status
            .success()
    );
    // Every evidence row ever written still exists; none was modified.
    assert_eq!(
        f.count("SELECT count(*) FROM evidence WHERE authority='trusted_validator'"),
        6
    );
    let effect = f.approve(&rid2, "d2");
    assert_eq!(
        f.json("dispatch", &[&effect])["classification"],
        "confirmed_applied"
    );
    assert_eq!(f.cas_executions(), 1);
    f.assert_accounted();
}

#[test]
fn evidence_expires_at_dispatch() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let old = f.current_ref();
    f.offset.set(HOUR + 60_000);
    let e = f.fails("dispatch", &[&effect]);
    assert!(e.contains("expired") || e.contains("stale"), "{e}");
    assert_eq!(f.effect_state(&effect), "cancelled");
    assert_eq!(f.review_state(&rid), "expired");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    // A cancelled approval is not reset for retry.
    assert!(
        f.db()
            .execute("UPDATE effect SET state='intent'", [])
            .is_err()
    );
    assert!(f.fails("dispatch", &[&effect]).contains("never dispatched"));
    assert_eq!(f.cas_executions(), 0);
    f.assert_accounted();
}

#[test]
fn expired_or_revoked_permit_at_each_boundary() {
    // Expiry: before review creation.
    let f = Fixture::with_permit_ms(120_000);
    f.validated();
    f.offset.set(130_000);
    assert!(f.fails("review-create", &["a"]).contains("permit"));
    assert_eq!(f.count("SELECT count(*) FROM review"), 0);
    // Expiry: after review, before decision (review lifetime is capped by the permit).
    let f = Fixture::with_permit_ms(120_000);
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    assert!(r["expires_at"].as_i64().unwrap() <= now_ms().unwrap() + 120_000);
    f.offset.set(130_000);
    assert!(
        !f.decide(&f.template(&rid, "d1", "approve"))
            .status
            .success()
    );
    assert_eq!(f.review_state(&rid), "expired");
    // Expiry: after approval, before dispatch claim.
    let f = Fixture::with_permit_ms(120_000);
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    f.offset.set(130_000);
    assert!(f.fails("dispatch", &[&effect]).contains("expired"));
    assert_eq!(f.effect_state(&effect), "cancelled");
    assert_eq!(f.cas_executions(), 0);
    // Revocation: before review creation, and before decision.
    let f = Fixture::new();
    f.validated();
    f.json("revoke", &["p"]);
    assert!(f.fails("review-create", &["a"]).contains("permit"));
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let body = f.template(&rid, "d1", "approve");
    f.json("revoke", &["p"]);
    assert!(!f.decide(&body).status.success());
    assert_eq!(f.review_state(&rid), "revoked");
    assert_eq!(
        f.work().2,
        "permit_revoked_new_authority_and_review_required"
    );
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    f.assert_accounted();
}

#[test]
fn revocation_dispatch_race() {
    // Revoked while the executor waits just before its claim: claim refuses.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let old = f.current_ref();
    let (child, dir) = f.dispatch_paused(&effect, "before_effect_claim");
    let rv = f.json("revoke", &["p"]);
    assert_eq!(rv["detail"]["too_late_to_guarantee_prevention"], false);
    let out = release(child, &dir);
    assert!(!out.status.success());
    assert_eq!(f.effect_state(&effect), "cancelled");
    assert_eq!(f.review_state(&rid), "revoked");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    f.assert_accounted();

    // Revoked just after the claim: honestly reported too late to guarantee prevention.
    // This executor happens to see it before the ref update and aborts.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let old = f.current_ref();
    let (child, dir) = f.dispatch_paused(&effect, "after_effect_claim");
    let rv = f.json("revoke", &["p"]);
    assert_eq!(rv["detail"]["too_late_to_guarantee_prevention"], true);
    assert_eq!(rv["detail"]["claimed_effects"][0], effect.as_str());
    let out = release(child, &dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_not_applied");
    assert_eq!(f.effect_state(&effect), "not_applied");
    assert_eq!(
        f.review_state(&rid),
        "consumed",
        "consumed approval is not reset"
    );
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);

    // Revoked after the executor's last check: the ref update still happens. Recorded.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let (child, dir) = f.dispatch_paused(&effect, "after_final_recheck");
    let rv = f.json("revoke", &["p"]);
    assert_eq!(rv["detail"]["too_late_to_guarantee_prevention"], true);
    let out = release(child, &dir);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_applied");
    assert_eq!(v["observation"]["revoked_after_claim"], true);
    assert_eq!(
        v["observation"]["revocation_limit"],
        "revocation after claim cannot guarantee prevention"
    );
    assert_eq!(f.ref_mutations(&effect), 1);
    assert_eq!(f.cas_executions(), 1);
    f.assert_accounted();
}

// ------------------------------------------------------------------ crash/ambiguity
fn crash_after_cas(f: &Fixture) -> (String, String, String, String) {
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
    let effect = f.approve(&rid, "d1");
    let (child, _dir) = f.dispatch_paused(&effect, "after_ref_cas");
    kill(child);
    assert_eq!(f.effect_state(&effect), "dispatching");
    assert_eq!(f.current_ref(), new, "CAS happened before the crash");
    (rid, effect, old, new)
}

#[test]
fn ambiguous_ref_effect() {
    let f = Fixture::new();
    let (rid, effect, _old, new) = crash_after_cas(&f);
    assert_eq!(f.reconcile(&effect), "confirmed_applied");
    assert_eq!(f.effect_state(&effect), "confirmed");
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.ref_mutations(&effect), 1, "no second mutation");
    assert_eq!(f.reflog().len(), 1);
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(f.review_state(&rid), "consumed");
    // Retrying dispatch and reconciliation never re-executes.
    assert_eq!(f.json("dispatch", &[&effect])["dispatched"], false);
    let due = f.work().3;
    f.json("reconcile-effects", &[]);
    assert_eq!(f.work().3, due);
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 1);
    assert_eq!(f.status()["action_result_is_work_completion"], false);
    f.assert_accounted();
}

#[test]
fn ambiguous_ref_conflict() {
    // Competing writer moves the ref to a third value during the crash window.
    let f = Fixture::new();
    let (_rid, effect, _old, new) = crash_after_cas(&f);
    let third = f.third_commit(&new);
    f.git_ok(&["update-ref", "refs/heads/fixture", &third, &new]);
    assert_eq!(f.reconcile(&effect), "conflict");
    assert_eq!(f.effect_state(&effect), "conflict");
    assert_eq!(
        f.current_ref(),
        third,
        "changed target preserved; no automatic rollback"
    );
    assert_eq!(f.ref_mutations(&effect), 1);
    assert_eq!(f.cas_executions(), 1, "zero automatic retry");
    assert_eq!(
        f.work().2,
        "inspect_conflicting_target_no_automatic_rollback"
    );
    assert_eq!(f.json("dispatch", &[&effect])["dispatched"], false);
    assert!(f.fails("review-create", &["a"]).contains("conflicting"));
    f.assert_accounted();

    // Competing writer returns the ref to the OLD value: current ref alone would look
    // not-applied; audit history shows the prior effect, so it is a conflict.
    let f = Fixture::new();
    let (_rid, effect, old, new) = crash_after_cas(&f);
    f.git_ok(&["update-ref", "refs/heads/fixture", &old, &new]);
    assert_eq!(f.reconcile(&effect), "conflict");
    assert_eq!(f.effect_state(&effect), "conflict");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 1);

    // Audit history rewritten: unknown, never not-applied, no dispatch.
    let f = Fixture::new();
    let (_rid, effect, old, new) = crash_after_cas(&f);
    f.git_ok(&["update-ref", "refs/heads/fixture", &old, &new]);
    fs::write(f.repo().join("logs/refs/heads/fixture"), b"").unwrap();
    assert_eq!(f.reconcile(&effect), "unknown");
    assert_eq!(f.effect_state(&effect), "unknown");
    assert_eq!(f.work().2, "inspect_ambiguous_effect_no_retry");
    let due = f.work().3;
    f.json("reconcile-effects", &[]);
    f.status();
    assert_eq!(f.work().3, due, "unchanged unknown is not postponed");
    assert_eq!(f.cas_executions(), 1);
    assert!(f.fails("review-create", &["a"]).contains("unresolved"));
    f.assert_accounted();
}

#[test]
fn effect_not_applied() {
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let old = r["envelope"]["action"]["expected_old"]
        .as_str()
        .unwrap()
        .to_string();
    let effect = f.approve(&rid, "d1");
    let (child, _d) = f.dispatch_paused(&effect, "after_effect_claim");
    kill(child);
    assert_eq!(f.effect_state(&effect), "dispatching");
    // Executor proven gone (kernel + mutation lock), ref unchanged, no audit entry.
    assert_eq!(f.reconcile(&effect), "confirmed_not_applied");
    assert_eq!(f.effect_state(&effect), "not_applied");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    assert_eq!(f.reflog().len(), 0);
    // Old approval not reset; dispatch of it refused; a fresh review is required.
    assert_eq!(f.review_state(&rid), "consumed");
    assert_eq!(f.json("dispatch", &[&effect])["dispatched"], false);
    assert!(
        f.db()
            .execute("UPDATE review SET state='authorized'", [])
            .is_err()
    );
    assert_eq!(f.work().2, "effect_not_applied_new_review_required");
    let r2 = f.review();
    let rid2 = r2["review_id"].as_str().unwrap().to_string();
    assert_ne!(rid, rid2);
    let effect2 = f.approve(&rid2, "d2");
    assert_eq!(
        f.json("dispatch", &[&effect2])["classification"],
        "confirmed_applied"
    );
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(f.ref_mutations(&effect), 0);
    assert_eq!(f.ref_mutations(&effect2), 1);
    f.assert_accounted();
}

#[test]
fn live_executor_blocks_reconciliation() {
    use fs2::FileExt;
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let (child, dir) = f.dispatch_paused(&effect, "after_effect_claim");
    // Live executor holds coordinator ownership: no concurrent reconciler or dispatcher.
    assert!(f.fails("reconcile-effects", &[]).contains("owned"));
    assert!(!f.run("dispatch", &[&effect]).status.success());
    kill(child);
    drop(dir);
    // A still-live executor outside coordinator ownership (simulated by an open file
    // description holding the fixture mutation lock): no conclusion, no new dispatch.
    let repo_id = f.text("SELECT id FROM fixture_repo");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.root.join("locks").join(format!("fixture-{repo_id}.lock")))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let rec = f.json("reconcile-effects", &[]);
    assert_eq!(rec["reprobed"][0]["executor_may_be_live"], true);
    assert_eq!(f.effect_state(&effect), "dispatching");
    assert_eq!(f.count("SELECT count(*) FROM effect_observation"), 0);
    FileExt::unlock(&lock).unwrap();
    drop(lock);
    assert_eq!(f.reconcile(&effect), "confirmed_not_applied");
    assert_eq!(f.cas_executions(), 0);
    f.assert_accounted();
}

#[test]
fn failure_between_decision_intent_claim_and_result_commits() {
    // Kill before the decision commit: nothing recorded; the same decision then records.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let body = f.template(&rid, "d1", "approve");
    let mut c = f.cmd("decide");
    c.stdin(Stdio::piped());
    let (mut child, _d) = {
        let b = tempfile::tempdir().unwrap();
        let mut child = c
            .env("ASHER_TEST_BARRIER", "before_decision_commit")
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        while !b.path().join("before_decision_commit").exists() {
            assert!(Instant::now() < end, "barrier not reached");
            thread::sleep(Duration::from_millis(5));
        }
        (child, b)
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(f.count("SELECT count(*) FROM decision"), 0);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 0);
    assert_eq!(f.review_state(&rid), "pending");
    let first = f.decide(&body);
    assert!(first.status.success());
    // Decision and intent are atomic: both exist together.
    assert_eq!(f.count("SELECT count(*) FROM decision"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM effect WHERE state='intent'"),
        1
    );

    // Kill after the decision commit (response lost): replay returns the receipt.
    let g = Fixture::new();
    g.validated();
    let rid = g.review()["review_id"].as_str().unwrap().to_string();
    let body = g.template(&rid, "d1", "approve");
    {
        let b = tempfile::tempdir().unwrap();
        let mut child = g
            .cmd("decide")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("ASHER_TEST_BARRIER", "after_decision_commit")
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        while !b.path().join("after_decision_commit").exists() {
            assert!(Instant::now() < end, "barrier not reached");
            thread::sleep(Duration::from_millis(5));
        }
        kill(child);
    }
    assert_eq!(g.count("SELECT count(*) FROM decision"), 1);
    assert_eq!(
        g.count("SELECT count(*) FROM effect WHERE state='intent'"),
        1
    );
    let replay: Value = serde_json::from_slice(&g.decide(&body).stdout).unwrap();
    assert_eq!(replay["disposition"], "existing");
    let effect = replay["effect_id"].as_str().unwrap().to_string();

    // Kill before the claim (and before its commit): intent survives, dispatch once.
    let (child, _d) = g.dispatch_paused(&effect, "before_claim_commit");
    kill(child);
    assert_eq!(g.effect_state(&effect), "intent");
    assert_eq!(
        g.count("SELECT count(*) FROM review WHERE state='authorized'"),
        1
    );
    assert_eq!(g.cas_executions(), 0);
    // Kill before the result commit: reconciliation, not a second mutation.
    let (child, _d) = g.dispatch_paused(&effect, "before_result_commit");
    kill(child);
    assert_eq!(g.effect_state(&effect), "dispatching");
    assert_eq!(g.cas_executions(), 1);
    g.json("reconcile-effects", &[]);
    assert_eq!(g.effect_state(&effect), "confirmed");
    assert_eq!(g.ref_mutations(&effect), 1);
    assert_eq!(g.cas_executions(), 1);
    g.assert_accounted();

    // Kill after the result commit: durable result; nothing to redo.
    let h = Fixture::new();
    h.validated();
    let rid = h.review()["review_id"].as_str().unwrap().to_string();
    let effect = h.approve(&rid, "d1");
    let (child, _d) = h.dispatch_paused(&effect, "after_result_commit");
    kill(child);
    assert_eq!(h.effect_state(&effect), "confirmed");
    let n = h.count("SELECT count(*) FROM effect_observation");
    h.json("reconcile-effects", &[]);
    assert_eq!(h.count("SELECT count(*) FROM effect_observation"), n);
    assert_eq!(h.cas_executions(), 1);
    h.assert_accounted();
}

#[test]
fn schema_three_migration_preserves_records() {
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
    ] {
        c.execute_batch(m).unwrap();
    }
    c.execute_batch(
        "INSERT INTO permit VALUES('p',1,'local-owner','hash','{}',1,999999,NULL);
         INSERT INTO work VALUES('old','local-owner','key','hash','{}','p','active','blocked',3,'local-owner','inspect_supervision_uncertainty',100,0);
         INSERT INTO event VALUES(1,'old','local-owner','work.accepted','old',1,'{}');
         INSERT INTO attempt VALUES('a','old','p','spec','{\"mode\":\"hold\",\"runtime_ms\":1000,\"output_limit\":16384}','/nonexistent',1,1,1,'unknown','unresolved supervision or escaped descendant; notification delivery unavailable',0,100);
         INSERT INTO supervisor_command VALUES('launch:a','a','spec',1,999999,'1','old-boot',5);
         INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','unknown',6,'{}');
         INSERT INTO artifact VALUES('art','old','a','candidate','symbol-fixture-v1','0000000000000000000000000000000000000000000000000000000000000000','{}','{}','artifacts/art',1,7,'invalid','snapshot unavailable');
         INSERT INTO evidence VALUES('ev','old','a','art','agent_claim','agent:a','e',1,'agent_claim',8,9,'complete','[]','ph','{}','eh',1);",
    )
    .unwrap();
    let dump = |c: &Connection| -> Vec<String> {
        let mut out = Vec::new();
        for t in [
            "permit",
            "work",
            "event",
            "attempt",
            "supervisor_command",
            "supervisor_fact",
            "artifact",
            "evidence",
        ] {
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
        "review",
        "decision",
        "effect",
        "effect_observation",
        "fixture_repo",
    ] {
        assert_eq!(
            c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    // Protections survive on rebuilt and existing tables.
    for s in [
        "DELETE FROM work",
        "UPDATE work SET request_hash='x'",
        "UPDATE work SET phase='done'",
        "UPDATE work SET mode='closed'",
        "DELETE FROM event",
        "UPDATE evidence SET payload_json='x'",
        "DELETE FROM artifact",
        "UPDATE supervisor_command SET fingerprint='x'",
    ] {
        assert!(c.execute(s, []).is_err(), "{s}");
    }
    assert!(
        c.execute("UPDATE work SET phase='reviewing'", []).is_ok(),
        "new phase admitted"
    );
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
    drop(q);
    // Unsupported newer schema is refused without mutation or replacement.
    c.execute("UPDATE schema_meta SET version=9", []).unwrap();
    let epoch: i64 = c
        .query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))
        .unwrap();
    assert!(matches!(
        Owner::open(&root),
        Err(asher_bridge::Error::Schema(9))
    ));
    assert_eq!(
        c.query_row("SELECT epoch FROM coordinator_epoch", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        epoch
    );
    assert_eq!(
        c.query_row("SELECT version FROM schema_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        9
    );
    let out = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .args(["revoke"])
        .arg(&root)
        .arg("p")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "lock-free revocation also refuses unsupported schema"
    );
}

// ------------------------------------------------------------------ scenarios
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    seed: u64,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    PrepareValidated {},
    ReviewCreate {},
    Approve { id: String },
    DispatchPaused { barrier: String },
    Kill {},
    ExternalRefWrite { to: String },
    Reconcile {},
    Assert { oracle: String, equals: Value },
}
fn run_scenario(json: &str) {
    let s: Scenario = serde_json::from_str(json).expect("scenario parses with no unknown fields");
    let _ = s.seed;
    let f = Fixture::new();
    let (mut review, mut effect, mut old, mut new) =
        (String::new(), String::new(), String::new(), String::new());
    let mut paused: Option<(Child, TempDir)> = None;
    for (i, step) in s.steps.into_iter().enumerate() {
        match step {
            Step::PrepareValidated {} => {
                f.validated();
            }
            Step::ReviewCreate {} => {
                let r = f.review();
                review = r["review_id"].as_str().unwrap().into();
                old = r["envelope"]["action"]["expected_old"]
                    .as_str()
                    .unwrap()
                    .into();
                new = r["envelope"]["action"]["new_commit"]
                    .as_str()
                    .unwrap()
                    .into();
            }
            Step::Approve { id } => effect = f.approve(&review, &id),
            Step::DispatchPaused { barrier } => paused = Some(f.dispatch_paused(&effect, &barrier)),
            Step::Kill {} => kill(paused.take().expect("nothing paused").0),
            Step::ExternalRefWrite { to } => {
                let cur = f.current_ref();
                let target = match to.as_str() {
                    "old" => old.clone(),
                    "third" => f.third_commit(&cur),
                    other => panic!("unknown ref target {other}"),
                };
                f.git_ok(&["update-ref", "refs/heads/fixture", &target, &cur]);
            }
            Step::Reconcile {} => {
                f.json("reconcile-effects", &[]);
            }
            Step::Assert { oracle, equals } => {
                let got = match oracle.as_str() {
                    "effect_state" => json!(f.effect_state(&effect)),
                    "ref_is" => json!(match f.current_ref() {
                        r if r == old => "old".to_string(),
                        r if r == new => "new".to_string(),
                        _ => "third".to_string(),
                    }),
                    "ref_mutation_count" => json!(f.ref_mutations(&effect)),
                    "cas_executions" => json!(f.cas_executions()),
                    "effect_count" => json!(f.count("SELECT count(*) FROM effect")),
                    "work_next_action" => json!(f.work().2),
                    "work_closed" => json!(f.status()["work_closed"]),
                    "all_obligations_accounted" => {
                        f.assert_accounted();
                        json!(true)
                    }
                    other => panic!("unknown oracle {other}"),
                };
                assert_eq!(got, equals, "{} step {i}: oracle {oracle}", s.name);
            }
        }
    }
    assert!(paused.is_none(), "scenario left a paused process");
}
#[test]
fn scenario_ambiguous_ref_effect() {
    run_scenario(include_str!("scenarios/ambiguous_ref_effect.json"));
}
#[test]
fn scenario_ambiguous_ref_conflict() {
    run_scenario(include_str!("scenarios/ambiguous_ref_conflict.json"));
}
#[test]
fn scenario_parser_rejects_unknown_operations() {
    assert!(
        serde_json::from_str::<Scenario>(
            r#"{"name":"x","seed":1,"steps":[{"op":"retry_anyway"}]}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<Scenario>(
            r#"{"name":"x","seed":1,"steps":[{"op":"kill","signal":"TERM"}]}"#
        )
        .is_err()
    );
}

#[test]
fn leftover_ref_locks_are_classified_not_deleted() {
    // Executor killed after creating its ref lock (holding the proposed commit) but
    // before the rename: not applied; the lock is left for the owner, never deleted.
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
    let effect = f.approve(&rid, "d1");
    let (child, _d) = f.dispatch_paused(&effect, "before_ref_cas");
    kill(child);
    let lock = f.repo().join("refs/heads/fixture.lock");
    fs::write(&lock, format!("{new}\n")).unwrap();
    assert_eq!(f.reconcile(&effect), "confirmed_not_applied");
    assert!(lock.exists(), "no automatic deletion of a ref lock");
    assert_eq!(f.current_ref(), old);
    // Real git also refuses to update while that lock exists.
    assert!(
        !f.git(&["update-ref", "refs/heads/fixture", &new, &old])
            .status
            .success()
    );

    // A lock with foreign content means another writer may be mid-update: unknown.
    let g = Fixture::new();
    g.validated();
    let rid = g.review()["review_id"].as_str().unwrap().to_string();
    let effect = g.approve(&rid, "d1");
    let (child, _d) = g.dispatch_paused(&effect, "after_effect_claim");
    kill(child);
    let third = g.third_commit(&g.current_ref());
    fs::write(
        g.repo().join("refs/heads/fixture.lock"),
        format!("{third}\n"),
    )
    .unwrap();
    assert_eq!(g.reconcile(&effect), "unknown");
    assert_eq!(g.effect_state(&effect), "unknown");
    assert_eq!(g.cas_executions(), 0);
    g.assert_accounted();
}

// ================================================================ regressions
// Failure specifications (P1-P4) and the owner's
// decision: desired state alone does not establish attributable execution.

fn crash_at(f: &Fixture, barrier: &str) -> (String, String, String, String) {
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
    let effect = f.approve(&rid, "d1");
    let (child, _dir) = f.dispatch_paused(&effect, barrier);
    kill(child);
    (rid, effect, old, new)
}
fn summary(f: &Fixture, effect: &str) -> Value {
    f.json("reconcile-effects", &[])["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["effect_id"] == effect)
        .cloned()
        .expect("effect summary")
}
fn latest_observation(f: &Fixture, effect: &str) -> Value {
    serde_json::from_str(&f.text(&format!(
        "SELECT payload_json FROM effect_observation WHERE effect_id='{effect}' ORDER BY rowid DESC LIMIT 1"
    )))
    .unwrap()
}
/// Desired state is present but nothing attributes it to this effect: unresolved.
fn assert_unattributed(f: &Fixture, effect: &str, new: &str) {
    let s = summary(f, effect);
    assert_eq!(s["classification"], "unknown", "{s}");
    assert_eq!(s["state"], "unknown");
    assert_eq!(s["desired_state_observed"], true);
    assert_eq!(s["attribution"], "unattributed");
    assert_eq!(s["closure_eligible"], false);
    let reason = s["state_reason"].as_str().unwrap();
    assert!(
        !reason.contains("crash after rename") && !reason.contains("exclusive"),
        "unsupported causal claim: {reason}"
    );
    assert!(reason.contains("not attributed"), "{reason}");
    assert_eq!(f.work().2, "inspect_desired_state_unattributed_no_retry");
    assert_eq!(
        f.current_ref(),
        new,
        "observed state preserved; no rollback"
    );
    let st = f.status();
    assert_eq!(st["effects"][0]["closure_eligible"], false);
    assert_eq!(st["effects"][0]["attribution"], "unattributed");
    assert_eq!(st["effects"][0]["state"], "unknown");
    // No retry path and no new review; unchanged probes never postpone the deadline.
    assert_eq!(f.json("dispatch", &[effect])["dispatched"], false);
    assert!(f.fails("review-create", &["a"]).contains("unresolved"));
    let due = f.work().3;
    summary(f, effect);
    f.status();
    assert_eq!(f.work().3, due);
    f.assert_accounted();
}

#[test]
fn p1_crash_between_ref_rename_and_audit_append_is_unresolved() {
    // Real kill at the named point inside the compare-and-swap: ref renamed, audit absent.
    let f = Fixture::new();
    let (_rid, effect, _old, new) = crash_at(&f, "after_ref_rename");
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.reflog().len(), 0, "audit append never happened");
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(f.effect_state(&effect), "dispatching");
    assert_eq!(f.count("SELECT count(*) FROM effect_executor_result"), 0);
    assert_unattributed(&f, &effect, &new);
    // A lost executor result is not reconstructed as fact.
    let obs = latest_observation(&f, &effect);
    assert_eq!(obs["executor_result"]["source"], "none");
    assert_eq!(obs["readback"]["source"], "coordinator_readback");
    assert_eq!(f.count("SELECT count(*) FROM effect_executor_result"), 0);
    assert_eq!(f.cas_executions(), 1, "no second mutation");
}

#[test]
fn p1_wiped_audit_after_cas_is_unresolved() {
    let f = Fixture::new();
    let (_rid, effect, _old, new) = crash_at(&f, "after_ref_cas");
    fs::write(f.repo().join("logs/refs/heads/fixture"), b"").unwrap();
    assert_unattributed(&f, &effect, &new);
    assert_eq!(f.cas_executions(), 1);
}

#[test]
fn p2_unlogged_foreign_write_without_our_cas_is_not_attributed() {
    let f = Fixture::new();
    let (_rid, effect, old, new) = crash_at(&f, "after_effect_claim");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    // A same-user writer places the proposed commit by raw file write (no reflog line).
    fs::write(f.repo().join("refs/heads/fixture"), format!("{new}\n")).unwrap();
    assert_unattributed(&f, &effect, &new);
    assert_eq!(f.cas_executions(), 0, "our CAS never ran");
    assert_eq!(f.ref_mutations(&effect), 0);
    assert_eq!(f.count("SELECT count(*) FROM effect_executor_result"), 0);

    // Logged foreign writer producing the same value: still not attributed to us.
    let g = Fixture::new();
    let (_rid, effect, old, new) = crash_at(&g, "after_effect_claim");
    g.git_ok(&["update-ref", "refs/heads/fixture", &new, &old]);
    assert_unattributed(&g, &effect, &new);
    assert_eq!(g.ref_mutations(&effect), 0);
}

#[test]
fn p4_audit_append_failure_without_crash_keeps_executor_result_separate() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let new = r["envelope"]["action"]["new_commit"]
        .as_str()
        .unwrap()
        .to_string();
    let effect = f.approve(&rid, "d1");
    fs::set_permissions(
        f.repo().join("logs/refs/heads/fixture"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let out = f.json("dispatch", &[&effect]);
    let obs = &out["observation"];
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.reflog().len(), 0, "audit append failed");
    assert_eq!(f.cas_executions(), 1);
    // Executor's own result and independent readback are separate, explicitly sourced.
    assert_eq!(obs["executor_result"]["source"], "executor_in_process");
    assert_eq!(obs["executor_result"]["cas"]["cas"], "applied");
    assert_eq!(obs["executor_result"]["cas"]["reflog_appended"], false);
    assert_eq!(obs["readback"]["source"], "coordinator_readback");
    assert_eq!(obs["readback"]["observed_ref"], new.as_str());
    assert_eq!(obs["attribution"], "executor_reported_cas");
    assert_eq!(obs["audit_entry_missing"], true);
    assert!(!obs["warnings"].as_array().unwrap().is_empty());
    assert_eq!(out["classification"], "confirmed_applied");
    assert_eq!(
        f.count("SELECT count(*) FROM effect_executor_result WHERE source='executor_in_process'"),
        1
    );
    let st = f.status();
    assert_eq!(st["effects"][0]["attribution"], "executor_reported_cas");
    assert_eq!(st["effects"][0]["audit_entry_missing"], true);
    assert_eq!(st["effects"][0]["closure_eligible"], true);
    assert_eq!(st["work_closed"], false);
    f.assert_accounted();
}

#[test]
fn f3_both_partial_write_orderings_reconcile_conservatively() {
    // Window A (our order: rename, then audit): ref changed but audit missing -> p1 tests.
    // Window B (reversed order would create it): audit exists but ref unchanged.
    let f = Fixture::new();
    let (_rid, effect, old, new) = crash_at(&f, "after_effect_claim");
    let line =
        format!("{old} {new} Asher Fixture <fixture@invalid> 1 +0000\tasher-effect {effect}\n");
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(f.repo().join("logs/refs/heads/fixture"))
        .unwrap();
    log.write_all(line.as_bytes()).unwrap();
    drop(log);
    let s = summary(&f, &effect);
    assert_eq!(s["classification"], "unknown", "{s}");
    assert_eq!(s["desired_state_observed"], false);
    assert_ne!(s["classification"], "confirmed_applied");
    assert_ne!(s["classification"], "confirmed_not_applied");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    assert_eq!(f.work().2, "inspect_ambiguous_effect_no_retry");
    assert_eq!(f.json("dispatch", &[&effect])["dispatched"], false);
    assert!(f.fails("review-create", &["a"]).contains("unresolved"));
    assert_eq!(s["closure_eligible"], false);
    f.assert_accounted();
}

#[test]
fn f4_lost_executor_result_is_not_reconstructed_but_audit_still_attributes() {
    // Kill after the CAS (audit appended) but before the executor result commit.
    let f = Fixture::new();
    let (_rid, effect, _old, new) = crash_at(&f, "after_ref_cas");
    assert_eq!(f.count("SELECT count(*) FROM effect_executor_result"), 0);
    let s = summary(&f, &effect);
    assert_eq!(s["classification"], "confirmed_applied");
    assert_eq!(s["attribution"], "audit_entry");
    let obs = latest_observation(&f, &effect);
    assert_eq!(obs["executor_result"]["source"], "none");
    assert_eq!(obs["executor_result"]["status"], "not_recorded");
    assert_eq!(
        f.count("SELECT count(*) FROM effect_executor_result"),
        0,
        "never reconstructed"
    );
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.cas_executions(), 1);
}

#[test]
fn f4_recorded_executor_result_attributes_when_audit_is_missing() {
    let f = Fixture::new();
    let (_rid, effect, _old, new) = crash_at(&f, "after_executor_result_commit");
    assert_eq!(f.count("SELECT count(*) FROM effect_executor_result"), 1);
    assert_eq!(f.effect_state(&effect), "dispatching");
    fs::write(f.repo().join("logs/refs/heads/fixture"), b"").unwrap();
    let s = summary(&f, &effect);
    assert_eq!(s["classification"], "confirmed_applied");
    assert_eq!(s["attribution"], "executor_reported_cas");
    assert_eq!(s["audit_entry_missing"], true);
    let obs = latest_observation(&f, &effect);
    assert_eq!(obs["executor_result"]["source"], "executor_in_process");
    assert_eq!(obs["readback"]["source"], "coordinator_readback");
    assert_eq!(f.current_ref(), new);
    assert_eq!(f.cas_executions(), 1);
}

#[test]
fn f4_executor_result_contradicted_by_readback_is_unresolved() {
    let f = Fixture::new();
    let (_rid, effect, old, _new) = crash_at(&f, "after_effect_claim");
    // Forge an "applied" executor result while the ref is still the old value.
    f.db()
        .execute(
            "INSERT INTO effect_executor_result VALUES(?1,'executor_in_process','{}','{\"cas\":\"applied\",\"reflog_appended\":true}',1,1)",
            [&effect],
        )
        .unwrap();
    let s = summary(&f, &effect);
    assert_eq!(s["classification"], "unknown", "{s}");
    assert_ne!(s["classification"], "confirmed_not_applied");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    assert_eq!(s["closure_eligible"], false);
    assert!(f.fails("review-create", &["a"]).contains("unresolved"));
}

#[test]
fn f5_delayed_execution_after_claim_rechecks_before_the_cas() {
    // Evidence and review expire while the executor is delayed after its claim.
    let f = Fixture::new();
    f.validated();
    let r = f.review();
    let rid = r["review_id"].as_str().unwrap().to_string();
    let old = r["envelope"]["action"]["expected_old"]
        .as_str()
        .unwrap()
        .to_string();
    let effect = f.approve(&rid, "d1");
    let (child, dir) = f.dispatch_paused(&effect, "after_effect_claim");
    f.set_clock(HOUR + 120_000);
    let out = release(child, &dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_not_applied");
    assert_eq!(v["observation"]["executor_result"]["cas"]["cas"], "aborted");
    let why = v["observation"]["executor_result"]["cas"]["reason"]
        .as_str()
        .unwrap();
    assert!(why.contains("expired") || why.contains("stale"), "{why}");
    assert_eq!(f.current_ref(), old);
    assert_eq!(f.cas_executions(), 0);
    assert_eq!(
        f.review_state(&rid),
        "consumed",
        "consumed approval is not reset"
    );
    assert_eq!(f.effect_state(&effect), "not_applied");
    f.assert_accounted();

    // Preparation permit expires during the delay.
    let g = Fixture::with_permit_ms(120_000);
    g.validated();
    let rid = g.review()["review_id"].as_str().unwrap().to_string();
    let effect = g.approve(&rid, "d1");
    let (child, dir) = g.dispatch_paused(&effect, "after_effect_claim");
    g.set_clock(130_000);
    let v: Value = serde_json::from_slice(&release(child, &dir).stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_not_applied");
    assert!(
        v["observation"]["executor_result"]["cas"]["reason"]
            .as_str()
            .unwrap()
            .contains("expired")
    );
    assert_eq!(g.cas_executions(), 0);

    // Revocation between the claim and the update, caught by the final recheck.
    let h = Fixture::new();
    h.validated();
    let rid = h.review()["review_id"].as_str().unwrap().to_string();
    let effect = h.approve(&rid, "d1");
    let (child, dir) = h.dispatch_paused(&effect, "before_ref_cas");
    h.json("revoke", &["p"]);
    let v: Value = serde_json::from_slice(&release(child, &dir).stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_not_applied");
    assert!(
        v["observation"]["executor_result"]["cas"]["reason"]
            .as_str()
            .unwrap()
            .contains("revoked")
    );
    assert_eq!(h.cas_executions(), 0);
    h.assert_accounted();
}

#[test]
fn f5_remaining_boundary_after_final_recheck_is_documented_and_recorded() {
    // Expiry AFTER the final recheck cannot be prevented; the result is recorded, not hidden.
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let (child, dir) = f.dispatch_paused(&effect, "after_final_recheck");
    f.set_clock(HOUR + 120_000);
    let v: Value = serde_json::from_slice(&release(child, &dir).stdout).unwrap();
    assert_eq!(v["classification"], "confirmed_applied");
    assert_eq!(f.cas_executions(), 1);
    let ops = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/OPERATIONS.md"))
        .unwrap();
    assert!(
        ops.contains("not atomic with the ref update"),
        "limit must be documented"
    );
}

#[test]
fn f6_owner_key_creation_is_interruption_safe() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("state");
    let b = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .arg("init")
        .arg(&root)
        .env("ASHER_TEST_BARRIER", "before_owner_key_rename")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    while !b.path().join("before_owner_key_rename").exists() {
        assert!(Instant::now() < end, "barrier not reached");
        thread::sleep(Duration::from_millis(5));
    }
    kill(child);
    // Interrupted creation leaves NO owner.key (never a partial one).
    assert!(!root.join("owner.key").exists());
    // The next open bootstraps a complete private key; a stray temp is never used.
    let out = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .arg("owner-key-status")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "valid");
    let m = fs::metadata(root.join("owner.key")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!((m.len(), m.mode() & 0o777), (32, 0o600));
}

#[test]
fn f6_invalid_owner_key_is_not_replaced_and_recovery_ops_do_not_need_it() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    let key = f.root.join("owner.key");
    fs::write(&key, b"short").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let status = f.json("owner-key-status", &[]);
    assert_eq!(status["state"], "invalid");
    // Authorization operations are refused with an actionable error; the file is untouched.
    assert!(
        f.fails("decision-template", &[&rid, "d2", "approve"])
            .contains("owner credential")
    );
    assert!(!f.decide_bytes(b"{}").status.success());
    assert_eq!(
        fs::read(&key).unwrap(),
        b"short",
        "never regenerated or replaced"
    );
    // Recovery/continuation operations do not need signing authority.
    assert_eq!(f.status()["effects"][0]["state"], "intent");
    f.json("reconcile-effects", &[]);
    assert_eq!(
        f.json("dispatch", &[&effect])["classification"],
        "confirmed_applied"
    );
    assert_eq!(fs::read(&key).unwrap(), b"short");
    // The recorded decision stays valid.
    assert_eq!(
        f.count("SELECT count(*) FROM decision WHERE id='d1' AND choice='approve'"),
        1
    );
    f.assert_accounted();
}

#[test]
fn f6_missing_key_with_recorded_decisions_is_not_regenerated_and_reset_is_explicit() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    f.approve(&rid, "d1");
    let key = f.root.join("owner.key");
    fs::remove_file(&key).unwrap();
    assert_eq!(f.json("owner-key-status", &[])["state"], "missing");
    assert!(
        !key.exists(),
        "opening must not silently create a replacement once decisions exist"
    );
    // Reset requires the explicit flag.
    assert!(
        f.fails("owner-key-reset", &[])
            .contains("--replace-invalid")
    );
    assert!(!key.exists());
    let r = f.json("owner-key-reset", &["--replace-invalid"]);
    assert_eq!(r["state"], "valid");
    assert_eq!(fs::metadata(&key).unwrap().len(), 32);
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='owner_key.created'"),
        2,
        "bootstrap + explicit reset"
    );
    assert_eq!(f.count("SELECT count(*) FROM event WHERE kind='owner_key.created' AND payload_json LIKE '%explicit owner reset%'"), 1);
    // A valid key is never replaced.
    let before = fs::read(&key).unwrap();
    assert!(
        f.fails("owner-key-reset", &["--replace-invalid"])
            .contains("valid")
    );
    assert_eq!(fs::read(&key).unwrap(), before);

    // Invalid key: preserved aside as evidence, replaced only by explicit reset.
    fs::write(&key, b"KEYBYTES-12").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let r = f.json("owner-key-reset", &["--replace-invalid"]);
    assert_eq!(r["state"], "valid");
    let kept: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("owner.key.invalid-")
        })
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(fs::read(kept[0].path()).unwrap(), b"KEYBYTES-12");
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='owner_key.replaced'"),
        1
    );
    assert!(
        !f.text("SELECT payload_json FROM event WHERE kind='owner_key.replaced'")
            .contains("KEYBYTES"),
        "no key bytes in audit"
    );
    // Prior decision remains valid; the effect it authorized still dispatches.
    assert_eq!(f.count("SELECT count(*) FROM decision WHERE id='d1'"), 1);
}

/// Undo migrations 007 and 006 on a current ledger so a test can model a genuine schema-5
/// database: drop the Gate 6 M1 recovery tables and the Gate 5 tables and rebuild `work` in
/// its schema-5 shape (no closed rows exist).
const GATE5_TEARDOWN: &str = "
PRAGMA foreign_keys=OFF;
DROP TABLE IF EXISTS evidence_gap; DROP TABLE IF EXISTS time_anomaly; DROP TABLE IF EXISTS time_floor; DROP TABLE IF EXISTS host_boot; DROP TABLE IF EXISTS ledger_snapshot; DROP TABLE IF EXISTS ledger_restore;
DROP TABLE IF EXISTS recovery_item;
DROP TABLE IF EXISTS recovery_case; DROP TABLE IF EXISTS recovery_run;
DROP TABLE IF EXISTS work_outcome; DROP TABLE IF EXISTS cancel_request;
DROP TABLE IF EXISTS delivery_attempt; DROP TABLE IF EXISTS sweep_run;
DROP TABLE IF EXISTS outbox; DROP TABLE IF EXISTS attention; DROP TABLE IF EXISTS owner_command;
CREATE TABLE work_v5 (
 id TEXT PRIMARY KEY, principal TEXT NOT NULL, request_key TEXT NOT NULL,
 request_hash TEXT NOT NULL, request_json TEXT NOT NULL,
 permit_id TEXT NOT NULL REFERENCES permit(id),
 mode TEXT NOT NULL CHECK(mode='active'),
 phase TEXT NOT NULL CHECK(phase IN ('queued','preparing','reconciling','blocked','reviewing','applying','finalizing')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 next_owner TEXT NOT NULL CHECK(length(next_owner)>0),
 next_action TEXT NOT NULL CHECK(length(next_action)>0), due_at INTEGER NOT NULL,
 accepted_at INTEGER NOT NULL, CHECK(due_at>=accepted_at), UNIQUE(principal,request_key)
);
INSERT INTO work_v5 SELECT * FROM work;
DROP TABLE work;
ALTER TABLE work_v5 RENAME TO work;
CREATE TRIGGER work_request_immutable BEFORE UPDATE OF principal,request_key,request_hash,request_json,permit_id,accepted_at ON work
 BEGIN SELECT RAISE(ABORT,'accepted request is immutable'); END;
CREATE TRIGGER work_no_delete BEFORE DELETE ON work BEGIN SELECT RAISE(ABORT,'accepted work cannot be deleted'); END;
CREATE INDEX work_due ON work(due_at);
";

#[test]
fn schema_four_migration_preserves_review_and_effect_records() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    f.json("dispatch", &[&effect]);
    // Re-create the schema-4 shape: drop schema 5 additions and mark version 4.
    let c = f.db();
    c.execute(
        "INSERT INTO work SELECT 'control-work',principal,'control-key',request_hash,request_json,permit_id,mode,phase,version,next_owner,next_action,due_at,accepted_at FROM work WHERE request_key='w'",
        [],
    )
    .unwrap();
    let control_before: (String, String, String, i64) = c
        .query_row(
            "SELECT phase,next_owner,next_action,due_at FROM work WHERE id='control-work'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let tables = [
        "review",
        "decision",
        "effect",
        "effect_observation",
        "fixture_repo",
        "review_evidence",
    ];
    let dump = |c: &Connection| -> Vec<String> {
        let mut out = Vec::new();
        for t in tables {
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
    // Model a ledger confirmed under the superseded desired-state rule (no attribution).
    c.execute_batch(
        "DROP TRIGGER effect_observation_immutable;
         UPDATE effect_observation SET payload_json='{\"attribution\":\"desired state attained with no other writes; audit append missing (crash after rename)\"}';
         CREATE TRIGGER effect_observation_immutable BEFORE UPDATE ON effect_observation BEGIN SELECT RAISE(ABORT,'observations immutable'); END;
         DROP TABLE effect_executor_result; DROP TRIGGER effect_confirmed_requires_attribution;
         UPDATE schema_meta SET version=4;",
    )
    .unwrap();
    c.execute_batch(GATE5_TEARDOWN).unwrap();
    // An earlier build left exactly this action on a confirmed-but-unattributed effect.
    c.execute(
        "UPDATE work SET next_action='action_applied_verify_and_close_task5_not_implemented' WHERE request_key='w'",
        [],
    )
    .unwrap();
    let before = dump(&c);
    let original_due: i64 = c
        .query_row("SELECT due_at FROM work WHERE request_key='w'", [], |r| {
            r.get(0)
        })
        .unwrap();
    drop(c);
    let _ = fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700));
    f.json("task4-status", &["a"]);
    let c = f.db();
    assert_eq!(
        c.query_row("SELECT version FROM schema_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(dump(&c), before, "schema 4 -> 5 changed historical rows");
    let control_after: (String, String, String, i64) = c
        .query_row(
            "SELECT phase,next_owner,next_action,due_at FROM work WHERE id='control-work'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        control_after, control_before,
        "unquarantined control changed"
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM effect_executor_result", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // The legacy confirmation is flagged, not rewritten, and is never closure-eligible.
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM event WHERE kind='effect.legacy_confirmation_unattributed'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap(),
        1,
        "schema 4 repair is audited in its migration transaction"
    );
    let st = f.status();
    assert_eq!(st["effects"][0]["state"], "confirmed");
    assert_eq!(st["effects"][0]["closure_eligible"], false);
    assert_eq!(st["work"]["phase"], "finalizing");
    assert_eq!(st["work"]["next_owner"], "local-owner");
    assert_eq!(
        st["work"]["next_action"],
        "inspect_legacy_unattributed_confirmation"
    );
    assert_eq!(st["work"]["due_at"], original_due);
    // The attribution guard now protects the confirmed transition.
    assert!(
        c.execute("UPDATE effect SET state='confirmed'", [])
            .is_err()
    );
    assert!(
        c.execute("UPDATE decision SET choice='reject'", [])
            .is_err()
    );
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
    drop(q);
    // Model a schema-5 database produced by an earlier build, whose migration already
    // recorded quarantine but left the contradictory responsibility behind and emitted no
    // responsibility-repair event.
    c.execute_batch(
        "DROP TRIGGER event_no_delete;
         DELETE FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired';
         CREATE TRIGGER event_no_delete BEFORE DELETE ON event BEGIN SELECT RAISE(ABORT,'events are append-only'); END;",
    )
    .unwrap();
    c.execute(
        "UPDATE work SET next_action='action_applied_verify_and_close_task5_not_implemented' WHERE request_key='w'",
        [],
    )
    .unwrap();
    drop(c);
    let healed = f.status(); // Owner restart repairs the durable row.
    assert_eq!(healed["effects"][0]["closure_eligible"], false);
    assert_eq!(
        healed["work"]["next_action"],
        "inspect_legacy_unattributed_confirmation"
    );
    assert_eq!(healed["work"]["due_at"], original_due);
    let version_after_repair = healed["work"]["version"].as_i64().unwrap();
    assert_eq!(
        f.count(
            "SELECT count(*) FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired'"
        ),
        1,
        "already-schema-5 repair appends an attributable event"
    );
    let repaired_event: Value = serde_json::from_str(&f.text(
        "SELECT payload_json FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired' ORDER BY seq DESC LIMIT 1",
    ))
    .unwrap();
    assert_eq!(
        repaired_event["previous_next_action"],
        "action_applied_verify_and_close_task5_not_implemented"
    );
    assert_eq!(
        repaired_event["next_action"],
        "inspect_legacy_unattributed_confirmation"
    );
    assert_eq!(repaired_event["deadline_preserved"], true);
    assert!(
        repaired_event["quarantined_effect_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &effect)
    );
    let repeated = f.status(); // Repeated open/status projection is idempotent.
    assert_eq!(repeated["work"]["due_at"], original_due);
    assert_eq!(repeated["work"]["version"], version_after_repair);
    assert_eq!(
        f.count(
            "SELECT count(*) FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired'"
        ),
        1,
        "converged replay appends no event"
    );
}

#[test]
fn legacy_quarantine_repair_preserves_later_responsibility_on_same_work() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    f.json("dispatch", &[&effect]);

    // Re-create schema 4 with an unattributed historical confirmation, then let normal startup
    // apply migration 005. This establishes the actual legacy quarantine marker and stale action.
    let c = f.db();
    c.execute_batch(
        "DROP TRIGGER effect_observation_immutable;
         UPDATE effect_observation SET payload_json='{}';
         CREATE TRIGGER effect_observation_immutable BEFORE UPDATE ON effect_observation BEGIN SELECT RAISE(ABORT,'observations immutable'); END;
         DROP TABLE effect_executor_result; DROP TRIGGER effect_confirmed_requires_attribution;
         UPDATE schema_meta SET version=4;",
    )
    .unwrap();
    c.execute_batch(GATE5_TEARDOWN).unwrap();
    drop(c);
    let _ = fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700));
    f.status();

    // There is no supported Task 4 public transition that assigns a new responsibility after
    // this quarantined confirmation. Model a later durable assignment directly to ensure that
    // an old quarantine marker cannot make a future/independent responsibility disappear.
    let c = f.db();
    c.execute(
        "UPDATE work SET phase='blocked',next_owner='independent-reviewer',next_action='inspect_newer_obligation',due_at=due_at+1234,version=version+1 WHERE request_key='w'",
        [],
    )
    .unwrap();
    let before: (String, String, String, i64, i64) = c
        .query_row(
            "SELECT phase,next_owner,next_action,due_at,version FROM work WHERE request_key='w'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    let audit_count: i64 = c
        .query_row(
            "SELECT count(*) FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(c);

    let status = f.status();
    assert_eq!(status["effects"][0]["closure_eligible"], false);
    assert_eq!(
        (
            status["work"]["phase"].as_str().unwrap().to_owned(),
            status["work"]["next_owner"].as_str().unwrap().to_owned(),
            status["work"]["next_action"].as_str().unwrap().to_owned(),
            status["work"]["due_at"].as_i64().unwrap(),
            status["work"]["version"].as_i64().unwrap(),
        ),
        before
    );
    assert_eq!(
        f.count(
            "SELECT count(*) FROM event WHERE kind='work.legacy_quarantine_responsibility_repaired'"
        ),
        audit_count,
        "a later same-work responsibility is neither rewritten nor re-audited"
    );
}

/// SIMULATED (fault-injection boot-local clock offset; the host clock is never
/// changed): two hours of CLOCK_BOOTTIME pass while the wall clock does not move, i.e. an
/// unobserved rollback. A review and an approved envelope that expired in that time are never
/// renewed by the stale wall clock: the decision and the dispatch are refused as expired.
#[test]
fn clock_and_budget_rollback_never_renews_a_review_or_envelope() {
    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let body = f.template(&rid, "d1", "approve");
    fs::write(
        f.temp.path().join("boottime-offset-ms"),
        (2 * HOUR).to_string(),
    )
    .unwrap();
    assert!(
        !f.decide(&body).status.success(),
        "rolled-back clock renewed a review"
    );
    assert_eq!(f.review_state(&rid), "expired");
    let regressions =
        |f: &Fixture| f.count("SELECT count(*) FROM time_anomaly WHERE kind='regression'");
    assert!(regressions(&f) >= 1);

    let f = Fixture::new();
    f.validated();
    let rid = f.review()["review_id"].as_str().unwrap().to_string();
    let effect = f.approve(&rid, "d1");
    fs::write(
        f.temp.path().join("boottime-offset-ms"),
        (2 * HOUR).to_string(),
    )
    .unwrap();
    assert!(
        !f.run("dispatch", &[&effect]).status.success(),
        "rolled-back clock renewed an envelope"
    );
    assert_eq!(f.effect_state(&effect), "cancelled");
    assert_eq!(f.cas_executions(), 0);
    assert!(regressions(&f) >= 1);
}

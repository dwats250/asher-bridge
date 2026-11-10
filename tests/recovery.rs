//! Gate 6 M1 recovery foundation: storage-fault classification at the documented boundaries,
//! durable recovery accounting, refusal and preservation of unsafe ledgers, bounded
//! diagnostics, verified restore into separate targets, and the rule that recovery never
//! repeats consequential work.
//!
//! Faults are produced independently of the product's own logic wherever possible:
//! BUSY by a real competing SQLite write lock; FULL and I/O errors by SQLite itself reacting to
//! a test-only VFS failure at the named boundary's commit; response loss by a real SIGKILL;
//! corruption by overwriting bytes of a disposable ledger; a crashed writer by a helper
//! process that exits without closing its connection. Oracles are SQL rows read independently,
//! the real `git` binary, the fixture reflog, the executor journal and kernel /proc identity.
//! Everything is created beneath a harness temp directory.
#![cfg(feature = "fault-injection")]
use asher_bridge::{
    Owner, Reader,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const HOUR: i64 = 3_600_000;

struct F {
    temp: TempDir,
    root: PathBuf,
}
fn permit() -> PermitScope {
    PermitScope {
        fixture_id: "fixture:symbol".into(),
        action: "prepare_fixture".into(),
        max_attempts: 1,
        max_runtime_ms: 60000,
        network: false,
    }
}
fn request(key: &str) -> SubmitWork {
    SubmitWork {
        protocol_version: 1,
        request_key: key.into(),
        fixture_id: "fixture:symbol".into(),
        success_criteria: "Prepare synthetic symbol correction; capture remains incomplete".into(),
        source_evidence: vec!["fixture:malformed-symbol".into()],
        permit_id: "p".into(),
    }
}
impl F {
    /// A ledger with a permit and no work.
    fn bare() -> Self {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("state");
        let mut o = Owner::initialize(&root).unwrap();
        o.add_permit("p", &permit(), now_ms().unwrap() + 23 * HOUR)
            .unwrap();
        drop(o);
        let f = Self { temp: t, root };
        fs::create_dir_all(f.sink_dir()).unwrap();
        f
    }
    /// A ledger with one accepted work `w`.
    fn new() -> Self {
        let f = Self::bare();
        let mut o = Owner::open(&f.root).unwrap();
        o.submit(&request("w")).unwrap();
        drop(o);
        f
    }
    fn sink_dir(&self) -> PathBuf {
        self.temp.path().join("sink")
    }
    fn journal(&self) -> PathBuf {
        self.temp.path().join("executor.journal")
    }
    fn cmd(&self, action: &str) -> Command {
        Self::cmd_at(action, &self.root, self)
    }
    fn cmd_at(action: &str, root: &Path, f: &F) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action)
            .arg(root)
            .env("ASHER_TEST_SINK_DIR", f.sink_dir())
            .env("ASHER_TEST_EFFECT_JOURNAL", f.journal());
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
    fn with_stdin(mut c: Command, body: &[u8]) -> Output {
        let mut ch = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(body).unwrap();
        ch.wait_with_output().unwrap()
    }
    fn submit_cmd(&self) -> Command {
        self.cmd("submit")
    }
    fn submit(&self, key: &str) -> Output {
        Self::with_stdin(
            self.submit_cmd(),
            &serde_json::to_vec(&request(key)).unwrap(),
        )
    }
    /// Read-only oracle: never checkpoints, never creates files.
    fn ro(&self) -> Connection {
        Connection::open_with_flags(
            self.root.join("ledger.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
    }
    /// Read-write connection for setup and lock contention.
    fn db(&self) -> Connection {
        Connection::open(self.root.join("ledger.db")).unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.ro().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn text(&self, sql: &str) -> String {
        self.ro().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn readback(&self, boundary: &str, id: &str) -> Value {
        self.json("readback", &[boundary, id])
    }
    fn diagnose_at(root: &Path) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
            .arg("diagnose")
            .arg(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "diagnose must always report");
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn diagnose(&self) -> Value {
        Self::diagnose_at(&self.root)
    }
    fn hashes(&self) -> (String, String) {
        file_hashes(&self.root)
    }
    // ---- Task 3/4 flow (same public commands the review suite uses)
    fn validated(&self) -> String {
        self.json("symbol-intent", &["w", "a"]);
        self.json("reproduce", &["a"]);
        self.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
        let art = self.json("seal", &["a"]);
        let id = art["id"].as_str().unwrap().to_string();
        assert_eq!(self.json("validate", &[&id])["validation_passed"], true);
        id
    }
    fn review(&self) -> String {
        self.json("review-create", &["a"])["review_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn template(&self, review: &str, id: &str, choice: &str) -> Vec<u8> {
        serde_json::to_vec(&self.json("decision-template", &[review, id, choice])).unwrap()
    }
    fn approve(&self, review: &str, id: &str) -> String {
        let out = Self::with_stdin(self.cmd("decide"), &self.template(review, id, "approve"));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["effect_id"].as_str().unwrap().into()
    }
    fn approved(&self) -> String {
        self.validated();
        let r = self.review();
        self.approve(&r, "d1")
    }
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
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn current_ref(&self) -> String {
        self.git(&["rev-parse", "refs/heads/fixture"])
    }
    fn reflog(&self) -> String {
        fs::read_to_string(self.repo().join("logs/refs/heads/fixture")).unwrap()
    }
    /// Ref updates attributed to an effect by the shared reflog.
    fn ref_mutations(&self, effect: &str) -> usize {
        self.reflog()
            .lines()
            .filter(|l| l.ends_with(&format!("\tasher-effect {effect}")))
            .count()
    }
    /// Executor-side count of ref-update executions (append-only test journal).
    fn cas_executions(&self) -> usize {
        fs::read_to_string(self.journal())
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("cas_begin"))
            .count()
    }
    fn effect_state(&self, id: &str) -> String {
        self.ro()
            .query_row("SELECT state FROM effect WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    }
    /// Start a command paused at a named barrier, optionally feeding stdin first.
    fn paused(&self, mut c: Command, name: &str, stdin: Option<&[u8]>) -> (Child, TempDir) {
        let b = tempfile::tempdir().unwrap();
        let mut child = c
            .env("ASHER_TEST_BARRIER", name)
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(body) = stdin {
            input.write_all(body).unwrap();
        }
        drop(input);
        wait_marker(&mut child, b.path(), name);
        (child, b)
    }
}
fn wait_marker(child: &mut Child, dir: &Path, name: &str) {
    let end = Instant::now() + Duration::from_secs(20);
    while !dir.join(name).exists() {
        if Instant::now() > end || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            panic!("barrier {name} not reached: {err}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}
/// SIGKILL a paused process and prove with the kernel that it is gone.
fn kill(mut child: Child) -> Output {
    let pid = child.id();
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "killed process {pid} still present"
    );
    out
}
fn release(child: Child, dir: &TempDir) -> Output {
    fs::write(dir.path().join("release"), b"").unwrap();
    child.wait_with_output().unwrap()
}
fn fault(out: &Output) -> Value {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .unwrap_or_else(|| {
            panic!(
                "no classification on stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
}
fn assert_fault(out: &Output, boundary: &str, durability: &str, kind: &str, code: i32) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "no acknowledgment may be printed on a fault"
    );
    let j = fault(out);
    let f = &j["storage_fault"];
    assert_eq!(f["boundary"], boundary, "{j}");
    assert_eq!(f["durability"], durability, "{j}");
    assert_eq!(f["kind"], kind, "{j}");
    assert_eq!(f["retry_consequential_work"], false);
    j
}
fn sha(p: &Path) -> String {
    match fs::read(p) {
        Ok(b) => asher_bridge::artifact::sha256_hex(&b),
        Err(_) => "ABSENT".into(),
    }
}
fn file_hashes(root: &Path) -> (String, String) {
    (
        sha(&root.join("ledger.db")),
        sha(&root.join("ledger.db-wal")),
    )
}
fn work_rows(f: &F, key: &str) -> i64 {
    f.ro()
        .query_row(
            "SELECT count(*) FROM work WHERE request_key=?1",
            [key],
            |r| r.get(0),
        )
        .unwrap()
}

/// Not a test of its own: when re-executed with ASHER_RECOVERY_HELPER_DB/_SQL set, this
/// process applies the SQL with automatic checkpoints disabled and exits without closing its
/// connection, as a crashed writer would, so the WAL keeps its frames.
#[test]
fn helper_crashed_sqlite_writer() {
    let (Some(db), Ok(sql)) = (
        std::env::var_os("ASHER_RECOVERY_HELPER_DB"),
        std::env::var("ASHER_RECOVERY_HELPER_SQL"),
    ) else {
        return;
    };
    let c = Connection::open(db).unwrap();
    c.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
    c.execute_batch(&sql).unwrap();
    unsafe { libc::_exit(0) }
}
fn crashed_writer(db: &Path, sql: &str) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_crashed_sqlite_writer",
            "--test-threads=1",
        ])
        .env("ASHER_RECOVERY_HELPER_DB", db)
        .env("ASHER_RECOVERY_HELPER_SQL", sql)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "helper: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let wal = fs::metadata(db.with_file_name("ledger.db-wal"))
        .expect("crashed writer must leave its WAL behind");
    assert!(wal.len() > 0, "the crashed writer's WAL must hold frames");
}
/// Overwrite the root page of the `work` table (always in use) in the main database file.
/// The caller makes sure no WAL holds a newer copy of that page.
fn corrupt_work_root(db: &Path) {
    let root: i64 = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE name='work'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut b = fs::read(db).unwrap();
    let ps = match u16::from_be_bytes([b[16], b[17]]) {
        1 => 65536,
        n => n as usize,
    };
    let start = (root as usize - 1) * ps;
    for x in &mut b[start..start + ps] {
        *x = 0xA5;
    }
    fs::write(db, b).unwrap();
}

// ======================================================================= T1 accept work

#[test]
fn t1_busy_before_commit_is_not_committed_and_retry_is_idempotent() {
    let f = F::bare();
    let body = serde_json::to_vec(&request("k1")).unwrap();
    let (child, bar) = f.paused(f.submit_cmd(), "before_accept_begin", Some(&body));
    // A real competing writer holds SQLite's write lock when T1 begins.
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = release(child, &bar);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    let j = assert_fault(&out, "T1", "not_committed", "busy", 3);
    assert_eq!(j["storage_fault"]["stage"], "before_commit");
    assert_eq!(j["storage_fault"]["stable_id"], "k1");
    assert!(
        !f.root.join("emergency.jsonl").exists(),
        "a known non-commit carries no uncertainty to record"
    );
    assert_eq!(f.readback("T1", "k1")["observed"], "absent");
    assert_eq!(work_rows(&f, "k1"), 0);
    // Retrying with the same identity accepts exactly once.
    let r: Value = serde_json::from_slice(&f.submit("k1").stdout).unwrap();
    assert_eq!(r["disposition"], "accepted");
    let again: Value = serde_json::from_slice(&f.submit("k1").stdout).unwrap();
    assert_eq!(again["disposition"], "existing");
    assert_eq!(again["work"]["id"], r["work"]["id"]);
    assert_eq!(work_rows(&f, "k1"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='work.accepted'"),
        1
    );
}

#[test]
fn t1_disk_full_during_commit_is_unknown_and_imported_once_as_secondary_evidence() {
    let f = F::bare();
    let out = F::with_stdin(
        {
            let mut c = f.submit_cmd();
            c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_write:full");
            c
        },
        &serde_json::to_vec(&request("k1")).unwrap(),
    );
    let j = assert_fault(&out, "T1", "unknown", "full", 4);
    assert_eq!(j["storage_fault"]["stage"], "commit");
    let incident = j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(j["emergency_record"]["durable_receipt"], false);
    // The emergency log is outside SQLite, private and append-only.
    let log = f.root.join("emergency.jsonl");
    assert_eq!(
        fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let before_log = fs::read(&log).unwrap();
    assert_eq!(f.readback("T1", "k1")["observed"], "absent");
    // The next startup imports the record once, reads the stable ID back and resubmits nothing.
    let rec = f.json("recover", &[]);
    assert_eq!(rec["recovery_run"]["state"], "completed");
    let (class, obs): (String, String) = f
        .ro()
        .query_row(
            "SELECT classification,observation_json FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1",
            [&incident],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let obs: Value = serde_json::from_str(&obs).unwrap();
    assert_eq!(class, "accounted");
    assert_eq!(obs["readback"], "absent");
    assert_eq!(work_rows(&f, "k1"), 0, "recovery never resubmits");
    f.json("recover", &[]);
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id='{incident}'"
        )),
        1,
        "an incident is imported exactly once"
    );
    assert_eq!(
        fs::read(&log).unwrap(),
        before_log,
        "import never rewrites the log"
    );
    // The caller's retry with the same identity is accepted once.
    assert!(f.submit("k1").status.success());
    assert!(f.submit("k1").status.success());
    assert_eq!(work_rows(&f, "k1"), 1);
}

#[test]
fn t1_io_error_on_commit_sync_is_an_uncertain_commit_never_resolved_by_an_early_readback() {
    let f = F::bare();
    let body = serde_json::to_vec(&request("k1")).unwrap();
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_sync:ioerr");
    // The frames, including the commit record, reach the WAL; only the fsync fails.
    let (child, _bar) = f.paused(c, "after_storage_fault", Some(&body));
    // While the faulting process still holds its connection, an independent read back sees
    // nothing: this early observation does not establish that the transaction did not commit.
    let early = f.readback("T1", "k1");
    assert_eq!(early["observed"], "absent");
    // The process then crashes without closing (no checkpoint, WAL kept).
    let out = kill(child);
    assert!(out.stdout.is_empty(), "no acknowledgment");
    let late = f.readback("T1", "k1");
    eprintln!(
        "uncertain T1 commit: early readback={} after crash={}",
        early["observed"], late["observed"]
    );
    assert_eq!(
        late["observed"], "present",
        "SQLite recovered the WAL commit record: the 'failed' commit is durable"
    );
    // The caller resolves the uncertainty by resubmitting the same identity: no duplicate.
    let r: Value = serde_json::from_slice(&f.submit("k1").stdout).unwrap();
    assert_eq!(r["disposition"], "existing");
    assert_eq!(r["work"]["id"], late["detail"]["work_id"]);
    assert_eq!(work_rows(&f, "k1"), 1);
    Reader::open(&f.root).unwrap().assert_accounted().unwrap();
}

#[test]
fn t1_uncertain_commit_reported_as_unknown_when_the_process_exits() {
    let f = F::bare();
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_sync:ioerr");
    let out = F::with_stdin(c, &serde_json::to_vec(&request("k1")).unwrap());
    let j = assert_fault(&out, "T1", "unknown", "io_err", 4);
    assert!(
        j["next"]
            .as_str()
            .unwrap()
            .contains("do not assume it failed")
    );
    // Whatever the outcome, resubmission with the same key yields exactly one work.
    assert!(f.submit("k1").status.success());
    assert_eq!(work_rows(&f, "k1"), 1);
}

#[test]
fn t1_response_lost_after_commit_is_committed() {
    let f = F::bare();
    let body = serde_json::to_vec(&request("k1")).unwrap();
    // A real crash right after COMMIT: no response reaches the caller.
    let (child, _b) = f.paused(f.submit_cmd(), "after_accept_commit", Some(&body));
    let out = kill(child);
    assert!(out.stdout.is_empty());
    let rb = f.readback("T1", "k1");
    assert_eq!(rb["observed"], "present");
    let r: Value = serde_json::from_slice(&f.submit("k1").stdout).unwrap();
    assert_eq!(r["disposition"], "existing");
    assert_eq!(r["work"]["id"], rb["detail"]["work_id"]);
    // A non-crash failure after COMMIT is classified as committed, not as a rejection.
    let b = tempfile::tempdir().unwrap();
    fs::write(b.path().join("after_accept_commit"), b"").unwrap();
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_BARRIER", "after_accept_commit")
        .env("ASHER_TEST_BARRIER_DIR", b.path());
    let out = F::with_stdin(c, &serde_json::to_vec(&request("k2")).unwrap());
    assert_fault(&out, "T1", "committed", "post_commit", 5);
    assert_eq!(f.readback("T1", "k2")["observed"], "present");
    assert_eq!(work_rows(&f, "k2"), 1);
}

// ======================================================================= T4 create review

#[test]
fn t4_review_commit_faults_leave_no_partial_review() {
    let f = F::new();
    f.validated();
    let reviews = || f.count("SELECT count(*) FROM review");
    let attention = || f.count("SELECT count(*) FROM attention WHERE review_id IS NOT NULL");
    // Busy before BEGIN: not committed.
    let mut c = f.cmd("review-create");
    c.arg("a");
    let (child, bar) = f.paused(c, "before_review_begin", None);
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = release(child, &bar);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    assert_fault(&out, "T4", "not_committed", "busy", 3);
    assert_eq!((reviews(), attention()), (0, 0));
    // I/O error during COMMIT: unknown; the review and its attention are never split.
    let out = f
        .cmd("review-create")
        .arg("a")
        .env("ASHER_TEST_STORAGE_FAULT", "T4:wal_write:ioerr")
        .output()
        .unwrap();
    let j = assert_fault(&out, "T4", "unknown", "io_err", 4);
    // The fault names the review ID minted before COMMIT, the one identity of this creation.
    assert_eq!(j["storage_fault"]["stable_id_kind"], "review_id");
    let minted = j["storage_fault"]["stable_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(f.readback("T4", &minted)["observed"], "absent");
    // An attempt ID never answers for one creation.
    let rb = f.readback("T4", "a");
    assert_eq!(rb["observed"], "inconclusive");
    assert_eq!(rb["establishes_absence"], false);
    assert_eq!((reviews(), attention()), (0, 0));
    let rid = f.review();
    assert_ne!(rid, minted);
    assert_eq!(f.review(), rid, "re-creating returns the one open review");
    assert_eq!((reviews(), attention()), (1, 1));
}

#[test]
fn t4_review_committed_before_crash_is_read_back_and_never_duplicated() {
    let f = F::new();
    f.validated();
    let mut c = f.cmd("review-create");
    c.arg("a");
    let (child, _b) = f.paused(c, "after_review_commit", None);
    assert!(kill(child).stdout.is_empty());
    // With no response the caller holds no review ID: the attempt lists its reviews but does
    // not stand for this creation; re-creating returns the committed review.
    let rb = f.readback("T4", "a");
    assert_eq!(rb["observed"], "inconclusive");
    let listed = rb["detail"]["reviews_for_attempt"][0]["review_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(f.review(), listed);
    assert_eq!(f.readback("T4", &listed)["observed"], "present");
    assert_eq!(f.count("SELECT count(*) FROM review"), 1);
}

// ======================================================================= T5 decide

#[test]
fn t5_decision_faults_are_resolved_by_replaying_the_same_signed_decision() {
    let f = F::new();
    f.validated();
    let rid = f.review();
    let body = f.template(&rid, "d1", "approve");
    // Busy before BEGIN.
    let (child, bar) = f.paused(f.cmd("decide"), "before_decision_begin", Some(&body));
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = release(child, &bar);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    let j = assert_fault(&out, "T5", "not_committed", "busy", 3);
    assert_eq!(j["storage_fault"]["stable_id"], "d1");
    assert_eq!(f.readback("T5", "d1")["observed"], "absent");
    // Uncertain commit (fsync error) followed by a crash.
    let mut c = f.cmd("decide");
    c.env("ASHER_TEST_STORAGE_FAULT", "T5:wal_sync:ioerr");
    let (child, _b) = f.paused(c, "after_storage_fault", Some(&body));
    assert!(kill(child).stdout.is_empty());
    let rb = f.readback("T5", "d1");
    // Replaying the identical signed decision resolves it either way: one decision, one effect.
    let out = F::with_stdin(f.cmd("decide"), &body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["executed"], false);
    eprintln!("uncertain T5: readback after crash = {}", rb["observed"]);
    assert_eq!(f.count("SELECT count(*) FROM decision"), 1);
    assert_eq!(f.count("SELECT count(*) FROM effect"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM effect WHERE state='intent'"),
        1
    );
    assert_eq!(f.cas_executions(), 0, "a decision is never an execution");
}

// ======================================================================= T6 claim

#[test]
fn t6_busy_claim_is_not_committed_and_never_runs_the_ref_update() {
    let f = F::new();
    let effect = f.approved();
    let before = f.current_ref();
    let mut c = f.cmd("dispatch");
    c.arg(&effect);
    let (child, bar) = f.paused(c, "before_effect_claim", None);
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = release(child, &bar);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    assert_fault(&out, "T6", "not_committed", "busy", 3);
    assert_eq!(f.cas_executions(), 0);
    assert_eq!(f.current_ref(), before);
    assert_eq!(f.effect_state(&effect), "intent");
    assert_eq!(f.readback("T6", &effect)["observed"], "absent");
    // The owner's explicit dispatch is then the first and only execution.
    assert_eq!(
        f.json("dispatch", &[&effect])["classification"],
        "confirmed_applied"
    );
    assert_eq!((f.cas_executions(), f.ref_mutations(&effect)), (1, 1));
}

#[test]
fn t6_uncertain_claim_never_runs_the_action_and_recovery_never_dispatches() {
    for rule in ["T6:wal_write:full", "T6:wal_sync:ioerr"] {
        let f = F::new();
        let effect = f.approved();
        let before = (f.current_ref(), f.reflog());
        let mut c = f.cmd("dispatch");
        c.arg(&effect).env("ASHER_TEST_STORAGE_FAULT", rule);
        let (child, _b) = f.paused(c, "after_storage_fault", None);
        assert!(kill(child).stdout.is_empty());
        let rb = f.readback("T6", &effect);
        eprintln!("{rule}: claim readback after crash = {}", rb["observed"]);
        // Recovery observes and classifies; it never performs the ref update.
        for _ in 0..2 {
            f.json("recover", &[]);
            f.json("reconcile-effects", &[]);
        }
        assert_eq!(
            f.cas_executions(),
            0,
            "{rule}: no ref update during recovery"
        );
        assert_eq!((f.current_ref(), f.reflog()), before, "{rule}");
        let state = f.effect_state(&effect);
        if rb["observed"] == "present" {
            // The claim committed: the approval is consumed and can never be re-dispatched.
            assert_eq!(state, "not_applied", "{rule}");
            let d = f.json("dispatch", &[&effect]);
            assert_eq!(d["dispatched"], false);
            assert_eq!(f.cas_executions(), 0, "{rule}");
        } else {
            assert_eq!(state, "intent", "{rule}");
        }
    }
}

// ======================================================================= T7 record result

#[test]
fn t7_result_faults_after_the_update_are_reconciled_without_a_second_update() {
    // 1st armed T7 commit: the executor's own result; 2nd: the independent result record.
    for rule in ["T7:wal_sync:ioerr", "T7:wal_write:full:2"] {
        let f = F::new();
        let effect = f.approved();
        let out = f
            .cmd("dispatch")
            .arg(&effect)
            .env("ASHER_TEST_STORAGE_FAULT", rule)
            .output()
            .unwrap();
        let kind = if rule.contains("full") {
            "full"
        } else {
            "io_err"
        };
        assert_fault(&out, "T7", "unknown", kind, 4);
        assert_eq!(f.cas_executions(), 1, "{rule}");
        assert_eq!(f.ref_mutations(&effect), 1, "{rule}");
        assert_eq!(
            f.effect_state(&effect),
            "dispatching",
            "{rule}: result not recorded"
        );
        // The next startup reconciles from independent evidence; it never repeats the update.
        let rec = f.json("recover", &[]);
        assert_eq!(rec["recovery_run"]["state"], "completed");
        assert_eq!(f.effect_state(&effect), "confirmed", "{rule}");
        assert_eq!(
            (f.cas_executions(), f.ref_mutations(&effect)),
            (1, 1),
            "{rule}"
        );
        assert_eq!(f.readback("T7", &effect)["observed"], "present");
        let d = f.json("dispatch", &[&effect]);
        assert_eq!(d["dispatched"], false);
        assert_eq!(f.cas_executions(), 1, "{rule}");
    }
}

// ======================================================================= S1 / S2 supervisor

fn launch_paused(f: &F, barrier: &str, extra: &[(&str, &str)]) -> (Child, TempDir) {
    let mut c = f.cmd("run-fake");
    c.args(["w", "a", "complete", "1000"]);
    for (k, v) in extra {
        c.env(k, v);
    }
    f.paused(c, barrier, None)
}

#[test]
fn s1_busy_receipt_is_not_committed_and_never_creates_a_process() {
    let f = F::new();
    let (child, bar) = launch_paused(&f, "before_supervisor_accept", &[]);
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    fs::write(bar.path().join("release"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("storage fault at S1"), "{err}");
    assert!(
        !bar.path().join("wrapper_identity.json").exists(),
        "no wrapper process was created"
    );
    assert_eq!(f.count("SELECT count(*) FROM supervisor_command"), 0);
    assert_eq!(f.readback("S1", "a")["observed"], "absent");
    // Recovery never launches.
    f.json("recover", &[]);
    assert_eq!(f.count("SELECT count(*) FROM supervisor_command"), 0);
    assert_ne!(f.text("SELECT state FROM attempt WHERE id='a'"), "running");
}

#[test]
fn s1_uncertain_receipt_commit_never_creates_a_process() {
    let f = F::new();
    let b = tempfile::tempdir().unwrap();
    let out = f
        .cmd("run-fake")
        .args(["w", "a", "complete", "1000"])
        .env("ASHER_TEST_STORAGE_FAULT", "S1:wal_sync:ioerr")
        .env("ASHER_TEST_BARRIER", "none")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("storage fault at S1"), "{err}");
    assert!(err.contains("commit outcome unknown"), "{err}");
    assert!(
        !b.path().join("wrapper_identity.json").exists(),
        "a process is created only after the receipt commit returned OK"
    );
    let log = fs::read_to_string(f.root.join("emergency.jsonl")).unwrap();
    assert!(log.contains("\"boundary\":\"S1\""), "{log}");
    for _ in 0..2 {
        f.json("recover", &[]);
    }
    assert!(!b.path().join("wrapper_identity.json").exists());
    assert_eq!(
        f.count("SELECT count(*) FROM supervisor_fact WHERE kind<>'launch_accepted'"),
        0,
        "no lifecycle beyond an (uncertain) receipt"
    );
}

#[test]
fn s2_fact_fault_keeps_cleanup_and_never_relaunches() {
    let f = F::new();
    let b = tempfile::tempdir().unwrap();
    let _ = f
        .cmd("run-fake")
        .args(["w", "a", "complete", "1000"])
        .env("ASHER_TEST_STORAGE_FAULT", "S2:wal_write:ioerr")
        .env("ASHER_TEST_BARRIER", "none")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .output()
        .unwrap();
    // The wrapper was created after a durable receipt; cleanup ran without waiting for SQLite.
    let ident: Value =
        serde_json::from_slice(&fs::read(b.path().join("wrapper_identity.json")).unwrap()).unwrap();
    let pid = ident["pid"].as_i64().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "wrapper {pid} survived a fact-write failure"
    );
    let state = f.text("SELECT state FROM attempt WHERE id='a'");
    assert_ne!(state, "running");
    let commands = f.count("SELECT count(*) FROM supervisor_command");
    for _ in 0..2 {
        f.json("recover", &[]);
    }
    assert_eq!(f.count("SELECT count(*) FROM supervisor_command"), commands);
    eprintln!("S2 fault: attempt state after cleanup = {state}");
    // The uncertain fact write leaves a best-effort emergency record naming the fact kind.
    let log = fs::read_to_string(f.root.join("emergency.jsonl")).unwrap();
    assert!(log.contains("\"boundary\":\"S2\""), "{log}");
    let rec: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert!(!rec["fault"]["step"].as_str().unwrap().is_empty());
}

// ======================================================================= refusal and preservation

#[test]
fn refused_ledgers_keep_database_and_wal_byte_for_byte() {
    // Incompatible: a newer schema committed only in the WAL by a writer that crashed.
    let f = F::new();
    crashed_writer(
        &f.root.join("ledger.db"),
        "UPDATE schema_meta SET version=99",
    );
    let before = f.hashes();
    assert_ne!(before.1, "ABSENT");
    for (action, args) in [
        ("reconcile", vec![]),
        ("recover", vec![]),
        ("sweep", vec!["--backend", "sink"]),
    ] {
        let out = f.run(action, &args);
        assert_eq!(out.status.code(), Some(6), "{action}");
        assert_eq!(
            fault(&out)["startup_refused"]["ledger"],
            "incompatible",
            "{action}"
        );
        assert_eq!(
            f.hashes(),
            before,
            "{action} changed the refused ledger's files"
        );
    }
    for (action, args) in [("attempts", vec![]), ("revoke", vec!["p"])] {
        assert!(!f.run(action, &args).status.success(), "{action}");
        assert_eq!(
            f.hashes(),
            before,
            "{action} changed the refused ledger's files"
        );
    }
    let d = f.diagnose();
    assert_eq!(d["status"], "incompatible");
    assert_eq!(f.hashes(), before, "diagnose changed files");

    // Corrupt: a damaged b-tree page in the main file, with a crashed writer's WAL beside it.
    let g = F::new();
    corrupt_work_root(&g.root.join("ledger.db"));
    crashed_writer(
        &g.root.join("ledger.db"),
        "UPDATE coordinator_epoch SET committed_heartbeat_at=committed_heartbeat_at+1",
    );
    let before = g.hashes();
    for action in ["reconcile", "recover", "owner-key-status"] {
        let out = g.run(action, &[]);
        assert_eq!(
            out.status.code(),
            Some(6),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fault(&out)["startup_refused"]["ledger"],
            "corrupt",
            "{action}"
        );
        assert_eq!(
            g.hashes(),
            before,
            "{action} changed the corrupt ledger's files"
        );
    }
    assert!(!g.run("attempts", &[]).status.success());
    assert_eq!(g.hashes(), before);
    // A read back that cannot run never stands for absence.
    let rb = g.readback("T1", "w");
    assert_eq!(rb["observed"], "unavailable");
    assert_eq!(rb["establishes_absence"], false);
    assert_eq!(g.hashes(), before);
    let d = g.diagnose();
    assert_eq!(d["status"], "corrupt", "{d}");
    assert_eq!(d["accessible"], true);
    assert_eq!(g.hashes(), before, "diagnose changed files");
    // Never initialized over: init refuses the existing root.
    assert!(!g.run("init", &[]).status.success());
    assert_eq!(g.hashes(), before);
}

#[test]
fn diagnostics_distinguish_unreadable_unavailable_degraded_and_healthy() {
    let healthy = F::new();
    let d = healthy.diagnose();
    assert_eq!(d["status"], "healthy", "{d}");
    assert_eq!(d["mutating"], false);
    assert_eq!(d["ledger"]["schema_version"], 8);

    // Unreadable: the SQLite header itself is damaged.
    let u = F::new();
    let db = u.root.join("ledger.db");
    let mut b = fs::read(&db).unwrap();
    b[..16].copy_from_slice(b"NOT A LEDGER!!!\0");
    fs::write(&db, &b).unwrap();
    let before = u.hashes();
    let d = u.diagnose();
    assert_eq!(d["status"], "unreadable", "{d}");
    assert_eq!(d["accessible"], false);
    assert_eq!(u.hashes(), before);
    let out = u.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(fault(&out)["startup_refused"]["ledger"], "unreadable");

    // Unavailable: the file exists but cannot be opened by this user now.
    let n = F::new();
    let db = n.root.join("ledger.db");
    fs::set_permissions(&db, fs::Permissions::from_mode(0o000)).unwrap();
    let d = n.diagnose();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(d["status"], "unavailable", "{d}");
    assert_eq!(d["accessible"], false);

    // Absent: never created by diagnosis.
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("state");
    fs::create_dir(&missing).unwrap();
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o700)).unwrap();
    let d = F::diagnose_at(&missing);
    assert_eq!(d["status"], "absent");
    assert_eq!(
        fs::read_dir(&missing).unwrap().count(),
        0,
        "diagnose created files"
    );
}

// ======================================================================= recovery accounting

#[test]
fn startup_records_runs_examined_obligations_and_assigned_cases() {
    let f = F::new();
    // An escaped descendant leaves the attempt unknown: an unresolved obligation.
    f.json("run-fake", &["w", "a", "escape", "1000"]);
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "unknown");
    let rec = f.json("recover", &[]);
    let run = &rec["recovery_run"];
    assert_eq!(run["state"], "completed");
    let rid = run["run_id"].as_str().unwrap();
    let (examined, items): (i64, i64) = f
        .ro()
        .query_row(
            "SELECT examined,(SELECT count(*) FROM recovery_item WHERE run_id=r.id) FROM recovery_run r WHERE id=?1",
            [rid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(examined, items, "every examined obligation is recorded");
    assert!(items >= 2, "the work and its attempt are examined");
    let (owner, action, case_state): (String, String, String) = f
        .ro()
        .query_row(
            "SELECT responsible,required_action,state FROM recovery_case WHERE subject_kind='attempt' AND subject_id='a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(case_state, "open");
    assert_eq!(owner, "local-owner");
    let work_action = f.text("SELECT next_action FROM work WHERE request_key='w'");
    assert_eq!(
        action, work_action,
        "the case carries the work's assigned responsibility"
    );
    // Repeated startups record new runs but never duplicate an open case or move a deadline.
    let due = f.count("SELECT due_at FROM work WHERE request_key='w'");
    for _ in 0..3 {
        f.json("recover", &[]);
        assert!(f.run("list", &[]).status.success());
    }
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_case WHERE subject_key='attempt:a'"),
        1
    );
    assert_eq!(
        f.count("SELECT due_at FROM work WHERE request_key='w'"),
        due
    );
    assert!(f.count("SELECT count(*) FROM recovery_run WHERE state='completed'") >= 4);
    // The database itself refuses a completion claim without every examined item.
    let c = f.db();
    c.execute(
        "INSERT INTO recovery_run(id,coordinator_epoch,trigger,state,started_at) VALUES('x',1,'explicit','started',1)",
        [],
    )
    .unwrap();
    assert!(
        c.execute(
            "UPDATE recovery_run SET state='completed',finished_at=2,examined=5,summary_json='{}' WHERE id='x'",
            [],
        )
        .is_err(),
        "completion without its recorded items is refused"
    );
    assert!(
        c.execute("DELETE FROM recovery_run WHERE id='x'", [])
            .is_err()
    );
}

#[test]
fn interrupted_runs_never_claim_completion_and_the_breaker_bounds_startup() {
    let f = F::new();
    let killed_run = || {
        let (child, _b) = f.paused(f.cmd("reconcile"), "during_recovery_run", None);
        kill(child);
        f.text("SELECT id FROM recovery_run ORDER BY rowid DESC LIMIT 1")
    };
    let first = killed_run();
    assert_eq!(
        f.text(&format!(
            "SELECT state FROM recovery_run WHERE id='{first}'"
        )),
        "started"
    );
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM recovery_item WHERE run_id='{first}'"
        )),
        0
    );
    let d = f.diagnose();
    assert_eq!(d["status"], "degraded", "{d}");
    // The next startup records the dead run as interrupted, never completed.
    assert!(f.run("list", &[]).status.success());
    assert!(f.run("reconcile", &[]).status.success());
    let (state, summary) = (
        f.text(&format!(
            "SELECT state FROM recovery_run WHERE id='{first}'"
        )),
        f.text(&format!(
            "SELECT summary_json FROM recovery_run WHERE id='{first}'"
        )),
    );
    assert_eq!(state, "interrupted");
    assert!(summary.contains("\"completion_claimed\":false"));
    // Three consecutive unfinished passes: ordinary startup is refused before any write.
    for _ in 0..3 {
        killed_run();
    }
    let epoch = f.count("SELECT epoch FROM coordinator_epoch");
    let runs = f.count("SELECT count(*) FROM recovery_run");
    let files = f.hashes();
    let started = Instant::now();
    for (action, args) in [
        ("reconcile", vec![]),
        ("owner-key-status", vec![]),
        ("sweep", vec!["--backend", "sink"]),
    ] {
        let out = f.run(action, &args);
        assert_eq!(out.status.code(), Some(6), "{action}");
        assert_eq!(
            fault(&out)["startup_refused"]["ledger"],
            "degraded_startup_blocked"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "refusal is bounded"
    );
    assert_eq!(
        f.count("SELECT epoch FROM coordinator_epoch"),
        epoch,
        "no epoch change"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_run"),
        runs,
        "no run recorded"
    );
    assert_eq!(f.hashes(), files, "refused startup wrote nothing");
    let d = f.diagnose();
    assert_eq!(d["status"], "degraded");
    assert_eq!(d["accessible"], true);
    assert_eq!(d["ledger"]["recovery"]["startup_blocked"], true);
    // One explicit, bounded pass completes and reopens ordinary startup.
    let rec = f.json("recover", &[]);
    assert_eq!(rec["recovery_run"]["state"], "completed");
    assert_eq!(rec["recovery_run"]["trigger"], "explicit");
    assert!(f.count("SELECT count(*) FROM recovery_run WHERE state='completed'") > 0);
    assert!(f.run("reconcile", &[]).status.success());
    assert_eq!(f.diagnose()["status"], "healthy");
}

#[test]
fn adverse_ledger_content_fails_startup_a_bounded_number_of_times_with_diagnostics() {
    let f = F::new();
    let effect = f.approved();
    // Leave a claimed effect without a result: kill right after the claim commit.
    let mut c = f.cmd("dispatch");
    c.arg(&effect);
    let (child, _b) = f.paused(c, "after_effect_claim", None);
    kill(child);
    assert_eq!(f.effect_state(&effect), "dispatching");
    // Adverse content: the stored action of that effect becomes unparsable.
    f.db()
        .execute_batch(&format!(
            "DROP TRIGGER effect_identity_immutable;
             UPDATE effect SET action_json='{{' WHERE id='{effect}';
             CREATE TRIGGER effect_identity_immutable BEFORE UPDATE OF id,work_id,review_id,decision_id,repo_id,action_kind,action_hash,action_json,intent_epoch,created_at ON effect
              BEGIN SELECT RAISE(ABORT,'effect identity is immutable'); END;"
        ))
        .unwrap();
    for n in 1..=3 {
        let out = f.run("reconcile", &[]);
        assert_eq!(out.status.code(), Some(6), "attempt {n}");
        // Truthful: these startups had begun (epoch advanced, failed run recorded).
        assert_eq!(fault(&out)["startup_refused"]["startup_began"], true);
    }
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_run WHERE state='failed'"),
        3,
        "each failed startup is recorded as failed, never completed"
    );
    let epoch = f.count("SELECT epoch FROM coordinator_epoch");
    for _ in 0..5 {
        let out = f.run("reconcile", &[]);
        assert_eq!(
            fault(&out)["startup_refused"]["ledger"],
            "degraded_startup_blocked"
        );
        assert_eq!(fault(&out)["startup_refused"]["startup_began"], false);
    }
    assert_eq!(
        f.count("SELECT epoch FROM coordinator_epoch"),
        epoch,
        "blocked startups are not retried: no further epochs"
    );
    let d = f.diagnose();
    assert_eq!(d["status"], "degraded");
    let last = &d["ledger"]["recovery"]["recent_runs"][0];
    assert_eq!(last["state"], "failed");
    assert_eq!(last["summary"]["error_class"], "malformed_ledger_content");
    // An explicit pass is recorded too, and never dispatches.
    assert!(!f.run("recover", &[]).status.success());
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_run WHERE trigger='explicit' AND state='failed'"),
        1
    );
    assert_eq!(f.cas_executions(), 0);
}

// ======================================================================= restore

#[test]
fn restore_of_an_older_backup_fences_a_forgotten_effect() {
    let f = F::new();
    let effect = f.approved();
    let backup = f.temp.path().join("backup-1");
    let b = f.json("backup", &[backup.to_str().unwrap()]);
    assert_eq!(b["source_content_written"], false);
    // After the backup the approved effect is applied.
    assert_eq!(
        f.json("dispatch", &[&effect])["classification"],
        "confirmed_applied"
    );
    let applied = f.current_ref();
    assert_eq!((f.cas_executions(), f.ref_mutations(&effect)), (1, 1));
    // Then the live ledger is damaged: ordinary use refuses and its files are preserved.
    corrupt_work_root(&f.root.join("ledger.db"));
    let damaged = f.hashes();
    assert_eq!(f.run("dispatch", &[&effect]).status.code(), Some(6));
    assert_eq!(f.hashes(), damaged);
    // Restore the older backup into a new, separate target.
    let target = f.temp.path().join("restored");
    let r = f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    assert_eq!(r["replay_authorized"], false);
    assert_eq!(r["fence"]["dispatch"], "refused");
    assert_eq!(r["restore_run"]["state"], "completed");
    let cases = r["open_cases"].as_array().unwrap();
    let forgotten = cases
        .iter()
        .find(|c| c["subject_kind"] == "effect" && c["subject_id"] == effect.as_str())
        .unwrap_or_else(|| panic!("no case for the forgotten effect: {r}"));
    assert!(
        forgotten["reason"]
            .as_str()
            .unwrap()
            .contains("after the backup")
    );
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let obs: String = ro
        .query_row(
            "SELECT observation_json FROM recovery_case WHERE subject_kind='effect' AND subject_id=?1",
            [&effect],
            |r| r.get(0),
        )
        .unwrap();
    let obs: Value = serde_json::from_str(&obs).unwrap();
    assert_eq!(obs["fixture_observation"]["audit_entries_for_effect"], 1);
    assert_eq!(obs["fixture_observation"]["ref_now"], applied.as_str());
    let state: String = ro
        .query_row("SELECT state FROM effect WHERE id=?1", [&effect], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "intent", "the backup predates the dispatch");
    drop(ro);
    // The restored ledger never repeats the forgotten effect, by any path.
    for (action, args) in [
        ("dispatch", vec![effect.as_str()]),
        ("recover", vec![]),
        ("reconcile-effects", vec![]),
        ("reconcile", vec![]),
    ] {
        let _ = f.cmd_on(action, &target).args(&args).output().unwrap();
    }
    // A restored ledger is inspect-only: ordinary startup is refused before any write.
    let out = f.cmd_on("dispatch", &target).arg(&effect).output().unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "restored_inspect_only"
    );
    assert_eq!((f.cas_executions(), f.ref_mutations(&effect)), (1, 1));
    assert_eq!(f.current_ref(), applied);
    // Its rows stay what the backup said: the forgotten effect is never recorded as
    // "cancelled: never dispatched" by local inference.
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let state: String = ro
        .query_row("SELECT state FROM effect WHERE id=?1", [&effect], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "intent");
    drop(ro);
    // Restore cases are never resolved by inference.
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let open: i64 = ro
        .query_row(
            "SELECT count(*) FROM recovery_case WHERE subject_kind='effect' AND subject_id=?1 AND state='open'",
            [&effect],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open, 1);
    drop(ro);
    let d = F::diagnose_at(&target);
    assert_eq!(d["status"], "degraded");
    // The damaged original was never touched by restore.
    assert_eq!(f.hashes(), damaged);
}

#[test]
fn restore_only_creates_new_separate_verified_targets() {
    let f = F::new();
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let backup_files = (
        sha(&backup.join("snapshot.db")),
        sha(&backup.join("backup-manifest.json")),
    );
    // Existing target, nested target and the source itself are refused.
    let exists = f.temp.path().join("exists");
    fs::create_dir(&exists).unwrap();
    // Also refused: a target inside the live source ledger root.
    for t in [
        exists.clone(),
        backup.join("inner"),
        backup.clone(),
        f.root.join("restored"),
    ] {
        let out = f.cmd_on("restore", &backup).arg(&t).output().unwrap();
        assert!(
            !out.status.success(),
            "restore into {} must be refused",
            t.display()
        );
    }
    assert_eq!(fs::read_dir(&exists).unwrap().count(), 0);
    // A backup whose bytes no longer match its manifest is refused.
    let bad = f.temp.path().join("bk-tampered");
    fs::create_dir(&bad).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(
        backup.join("backup-manifest.json"),
        bad.join("backup-manifest.json"),
    )
    .unwrap();
    let mut db = fs::read(backup.join("snapshot.db")).unwrap();
    let n = db.len() - 100;
    db[n] ^= 0xff;
    fs::write(bad.join("snapshot.db"), db).unwrap();
    let t = f.temp.path().join("from-tampered");
    let out = f.cmd_on("restore", &bad).arg(&t).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("manifest"));
    assert!(!t.exists());
    // A corrupt live ledger is never backed up as if verified.
    let g = F::new();
    corrupt_work_root(&g.root.join("ledger.db"));
    let out = g.run("backup", &[g.temp.path().join("x").to_str().unwrap()]);
    assert!(!out.status.success());
    // A valid restore never changes the backup.
    let ok = f.temp.path().join("restored-ok");
    f.json_at("restore", &backup, &[ok.to_str().unwrap()]);
    assert_eq!(
        (
            sha(&backup.join("snapshot.db")),
            sha(&backup.join("backup-manifest.json"))
        ),
        backup_files
    );
    let m = fs::metadata(&ok).unwrap();
    assert_eq!(m.permissions().mode() & 0o777, 0o700);
}

#[test]
fn restore_fences_the_launch_of_an_intent_that_may_have_run_after_the_backup() {
    let f = F::new();
    f.json("symbol-intent", &["w", "a"]);
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    let r = f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    assert!(
        r["open_cases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["subject_kind"] == "attempt" && c["subject_id"] == "a"),
        "{r}"
    );
    assert_eq!(r["verification"]["integrity"], "ok");
    let b = tempfile::tempdir().unwrap();
    let out = f
        .cmd_on("run-fake", &target)
        .args(["w", "a", "prepare_symbol", "1000"])
        .env("ASHER_TEST_BARRIER", "none")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "restored_inspect_only"
    );
    assert!(
        !b.path().join("wrapper_identity.json").exists(),
        "no process"
    );
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let n: i64 = ro
        .query_row("SELECT count(*) FROM supervisor_command", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn backup_never_writes_the_source_ledger() {
    let f = F::new();
    // Keep a WAL with frames beside the source, as a live ledger has.
    crashed_writer(
        &f.root.join("ledger.db"),
        "UPDATE coordinator_epoch SET committed_heartbeat_at=committed_heartbeat_at+1",
    );
    let before = f.hashes();
    let out = f.json("backup", &[f.temp.path().join("b").to_str().unwrap()]);
    assert_eq!(out["source_content_written"], false);
    assert_eq!(f.hashes(), before);
    assert!(
        !f.temp.path().join("b/snapshot.db-wal").exists(),
        "a backup is one self-contained file"
    );
}

impl F {
    fn cmd_on(&self, action: &str, root: &Path) -> Command {
        Self::cmd_at(action, root, self)
    }
    fn json_at(&self, action: &str, root: &Path, args: &[&str]) -> Value {
        let out = self.cmd_on(action, root).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

// ======================================================================= non-action guarantees

#[test]
fn recovery_diagnosis_and_restore_never_perform_consequential_actions() {
    let f = F::new();
    let effect = f.approved();
    // A due owner attention item with a queued delivery exists (from the review).
    let deliveries = || f.count("SELECT count(*) FROM delivery_attempt");
    let snapshot = |f: &F| {
        (
            f.cas_executions(),
            f.current_ref(),
            f.reflog(),
            f.count("SELECT count(*) FROM supervisor_command"),
            f.count("SELECT count(*) FROM work WHERE mode='closed'"),
            f.effect_state(&effect),
            fs::read_dir(f.sink_dir()).unwrap().count(),
        )
    };
    let before = snapshot(&f);
    let d0 = deliveries();
    let backup = f.temp.path().join("bk");
    for _ in 0..3 {
        f.json("recover", &[]);
        f.diagnose();
        f.readback("T6", &effect);
    }
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    f.json_at("recover", &target, &[]);
    let _ = F::diagnose_at(&target);
    assert_eq!(
        snapshot(&f),
        before,
        "recovery paths performed a consequential action"
    );
    assert_eq!(deliveries(), d0, "recovery paths attempted a notification");
    assert_eq!(
        f.effect_state(&effect),
        "intent",
        "approval neither dispatched nor cancelled"
    );
}

// ======================================================================= additive migration

#[test]
fn migration_007_is_additive_and_preserves_every_existing_object_and_row() {
    let f = F::new();
    let effect = f.approved();
    f.json("dispatch", &[&effect]);
    f.json("sweep", &["--backend", "sink"]);
    // Model a schema-6 ledger: 007 only creates tables, so dropping them is its exact inverse.
    f.db()
        .execute_batch(
            "DROP TABLE IF EXISTS evidence_gap; DROP TABLE IF EXISTS time_anomaly; DROP TABLE IF EXISTS time_floor; DROP TABLE IF EXISTS host_boot; DROP TABLE ledger_snapshot; DROP TABLE ledger_restore; DROP TABLE recovery_item; DROP TABLE recovery_case;
             DROP TABLE recovery_run; UPDATE schema_meta SET version=6;",
        )
        .unwrap();
    let objects = |c: &Connection| -> Vec<(String, String, Option<String>)> {
        let mut s = c
            .prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name")
            .unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let rows = |c: &Connection, tables: &[String]| -> Vec<String> {
        let mut out = Vec::new();
        for t in tables {
            let mut q = c
                .prepare(&format!("SELECT * FROM \"{t}\" ORDER BY rowid"))
                .unwrap();
            let n = q.column_count();
            let r = q
                .query_map([], |r| {
                    Ok((0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap();
            for x in r {
                out.push(format!("{t}:{}", x.unwrap()));
            }
        }
        out
    };
    let c = f.ro();
    let before_objects = objects(&c);
    // Every table except those startup legitimately appends to or updates.
    let stable: Vec<String> = before_objects
        .iter()
        .filter(|(t, n, _)| {
            t == "table"
                && !n.starts_with("sqlite_")
                && !["coordinator_epoch", "owner_session", "schema_meta", "event"]
                    .contains(&n.as_str())
        })
        .map(|x| x.1.clone())
        .collect();
    assert!(stable.len() >= 20, "{stable:?}");
    let before_rows = rows(&c, &stable);
    let before_events = rows(&c, &["event".into()]);
    drop(c);
    let rec = f.json("recover", &[]);
    assert_eq!(rec["recovery_run"]["state"], "completed");
    let c = f.ro();
    assert_eq!(
        c.query_row("SELECT version FROM schema_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        8
    );
    let after_objects = objects(&c);
    for o in &before_objects {
        assert!(
            after_objects.contains(o),
            "migration changed or removed {o:?}"
        );
    }
    let added: Vec<_> = after_objects
        .iter()
        .filter(|o| !before_objects.contains(o))
        .map(|o| o.1.clone())
        .collect();
    assert!(
        added.iter().all(|n| n.contains("recovery")
            || n.contains("ledger_restore")
            || n.contains("ledger_snapshot")
            // Startup continues through 008, which adds only its own tables.
            || ["host_boot", "time_floor", "time_anomaly", "evidence_gap"]
                .iter()
                .any(|t| n.starts_with(t))),
        "007 added only recovery objects: {added:?}"
    );
    assert_eq!(
        rows(&c, &stable),
        before_rows,
        "migration or startup changed historical rows"
    );
    let after_events = rows(&c, &["event".into()]);
    assert_eq!(
        &after_events[..before_events.len()],
        &before_events[..],
        "events are append-only"
    );
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
}

// ======================================================================= further regressions

#[test]
fn a_backup_is_never_a_usable_state_root_and_any_copy_of_it_is_inspect_only() {
    let f = F::new();
    // Record an un-launched intent, then back up.
    f.json("symbol-intent", &["w", "a"]);
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let listing = || {
        let mut v: Vec<(String, String)> = fs::read_dir(&backup)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name().to_string_lossy().to_string(), sha(&e.path()))
            })
            .collect();
        v.sort();
        v
    };
    let before = listing();
    assert!(
        !backup.join("ledger.db").exists(),
        "a backup holds no ledger.db"
    );
    // Every command pointed at the backup directory is refused without touching it.
    let b = tempfile::tempdir().unwrap();
    for (action, args) in [
        ("run-fake", vec!["w", "a", "prepare_symbol", "1000"]),
        ("reconcile", vec![]),
        ("recover", vec![]),
        ("dispatch", vec!["x"]),
        ("list", vec![]),
        ("attempts", vec![]),
    ] {
        let out = f
            .cmd_on(action, &backup)
            .args(&args)
            .env("ASHER_TEST_BARRIER", "none")
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{action} on a backup");
        assert_eq!(listing(), before, "{action} changed the backup");
    }
    assert!(
        !b.path().join("wrapper_identity.json").exists(),
        "no process"
    );
    // A copy of the snapshot used as a ledger carries the snapshot marker: inspect-only.
    let t = tempfile::tempdir().unwrap();
    let copy = t.path().join("state");
    fs::create_dir(&copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(backup.join("snapshot.db"), copy.join("ledger.db")).unwrap();
    fs::set_permissions(copy.join("ledger.db"), fs::Permissions::from_mode(0o600)).unwrap();
    let out = f
        .cmd_on("run-fake", &copy)
        .args(["w", "a", "prepare_symbol", "1000"])
        .env("ASHER_TEST_BARRIER", "none")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "restored_inspect_only"
    );
    assert!(
        !b.path().join("wrapper_identity.json").exists(),
        "no process"
    );
    assert!(
        !copy.join("coordinator.lock").exists() && !copy.join("ledger.db-wal").exists(),
        "the refusal created no file"
    );
    let rec = f.json_at("recover", &copy, &[]);
    assert_eq!(rec["recovery_run"]["restored_ledger"], true);
    assert_eq!(F::diagnose_at(&copy)["status"], "degraded");
    // The verified backup still restores.
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
}

#[test]
fn an_interrupted_restore_never_creates_the_selected_target() {
    let f = F::new();
    f.json("symbol-intent", &["w", "a"]);
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    let mut c = f.cmd_on("restore", &backup);
    c.arg(&target);
    let (child, _b) = f.paused(c, "during_restore", None);
    kill(child);
    assert!(
        !target.exists(),
        "the selected target appears only when complete"
    );
    let staging: Vec<PathBuf> = fs::read_dir(f.temp.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".restored.restoring-")
        })
        .collect();
    assert_eq!(staging.len(), 1, "the partial copy is left, clearly named");
    // Even the partial copy is inspect-only: it carries the snapshot marker.
    let b = tempfile::tempdir().unwrap();
    let out = f
        .cmd_on("run-fake", &staging[0])
        .args(["w", "a", "prepare_symbol", "1000"])
        .env("ASHER_TEST_BARRIER", "none")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert!(!b.path().join("wrapper_identity.json").exists());
    // A fresh restore to the same selected target then succeeds.
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    assert!(target.join("ledger.db").exists());
}

#[test]
fn an_empty_main_file_beside_a_wal_is_never_opened() {
    let f = F::new();
    crashed_writer(
        &f.root.join("ledger.db"),
        "UPDATE coordinator_epoch SET committed_heartbeat_at=committed_heartbeat_at+1",
    );
    fs::write(f.root.join("ledger.db"), b"").unwrap();
    let _ = fs::remove_file(f.root.join("ledger.db-shm"));
    let wal = sha(&f.root.join("ledger.db-wal"));
    assert_ne!(wal, "ABSENT");
    let d = f.diagnose();
    assert_eq!(d["status"], "unreadable", "{d}");
    assert_eq!(sha(&f.root.join("ledger.db-wal")), wal, "diagnose");
    for (action, args) in [
        ("reconcile", vec![]),
        ("recover", vec![]),
        ("list", vec![]),
        ("readback", vec!["T1", "w"]),
        ("attempts", vec![]),
        ("backup", vec!["/nonexistent-never-created"]),
    ] {
        let _ = f.run(action, &args);
        assert_eq!(
            sha(&f.root.join("ledger.db-wal")),
            wal,
            "{action} lost the WAL"
        );
        assert_eq!(fs::metadata(f.root.join("ledger.db")).unwrap().len(), 0);
    }
    let out = f.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(fault(&out)["startup_refused"]["ledger"], "unreadable");
}

fn synthetic_fault(key: &str) -> asher_bridge::storage::StorageFault {
    use asher_bridge::storage::{Boundary, Durability, FaultKind, Stage, StorageFault};
    StorageFault {
        boundary: Boundary::T1,
        operation: Boundary::T1.operation(),
        step: String::new(),
        committed_before: Vec::new(),
        stage: Stage::Commit,
        durability: Durability::Unknown,
        kind: FaultKind::IoErr,
        sqlite_code: Some(1034),
        stable_id_kind: "request_key",
        stable_id: key.into(),
        detail: "synthetic".into(),
        retry_consequential_work: false,
    }
}

/// One emergency line as the CLI writes it, from a process that has since exited (pid 0).
fn emergency_line(key: &str) -> String {
    serde_json::json!({"v": 1, "incident": uuid::Uuid::new_v4().to_string(), "at": 0,
                       "pid": 0, "fault": synthetic_fault(key)})
    .to_string()
}

#[test]
fn every_emergency_record_within_the_bound_is_imported_and_none_twice() {
    let f = F::new();
    // Fill the log to its byte bound with incidents for keys that never committed.
    let log = f.root.join("emergency.jsonl");
    let mut text = String::new();
    let mut n = 0;
    loop {
        let line = emergency_line(&format!("k{n}"));
        if (text.len() + line.len() + 1) as u64 > asher_bridge::recovery::EMERGENCY_MAX_BYTES {
            break;
        }
        text.push_str(&line);
        text.push('\n');
        n += 1;
    }
    assert!(
        n > 500,
        "{n} records: more than the former 500-line import cap"
    );
    // The last record is torn (no final newline, truncated): it is examined as malformed.
    text.truncate(text.len() - 2);
    fs::write(&log, &text).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    let rec = f.json("recover", &[]);
    assert_eq!(rec["recovery_run"]["state"], "completed");
    let accounted = |f: &F| {
        f.count(
            "SELECT count(*) FROM recovery_item WHERE subject_kind='storage_incident' AND classification='accounted'",
        )
    };
    assert_eq!(
        accounted(&f) as usize,
        n - 1,
        "every intact record is imported"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_item WHERE subject_kind='storage_incident' AND classification='unclassifiable'"),
        1,
        "the torn record is examined as malformed"
    );
    f.json("recover", &[]);
    assert_eq!(accounted(&f) as usize, n - 1, "none twice");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        text,
        "import never rewrites the log"
    );
    let d = f.diagnose();
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 0, "{d}");
    // A record appended after a torn one stays identifiable under its own incident.
    let g = F::new();
    let glog = g.root.join("emergency.jsonl");
    fs::write(&glog, emergency_line("a")).unwrap(); // torn: no final newline
    fs::set_permissions(&glog, fs::Permissions::from_mode(0o600)).unwrap();
    let id = asher_bridge::recovery::record_incident(&g.root, &synthetic_fault("b")).unwrap();
    g.json("recover", &[]);
    assert_eq!(
        g.count(&format!(
            "SELECT count(*) FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id='{id}'"
        )),
        1,
        "the record after a torn one is imported under its own incident"
    );
}

#[test]
fn the_startup_breaker_orders_runs_by_insertion_not_wall_clock() {
    let f = F::new();
    // A completed run stamped 30 minutes in the future (the clock later moves back).
    let out = f
        .cmd("reconcile")
        .env("ASHER_TEST_CLOCK_OFFSET_MS", "1800000")
        .output()
        .unwrap();
    assert!(out.status.success());
    for _ in 0..3 {
        let (child, _b) = f.paused(f.cmd("reconcile"), "during_recovery_run", None);
        kill(child);
    }
    let epoch = f.count("SELECT epoch FROM coordinator_epoch");
    let out = f.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "degraded_startup_blocked"
    );
    assert_eq!(f.count("SELECT epoch FROM coordinator_epoch"), epoch);
    // One explicit pass completes and ordinary startup resumes despite the future stamp.
    assert_eq!(f.json("recover", &[])["recovery_run"]["state"], "completed");
    assert!(f.run("reconcile", &[]).status.success());
}

#[test]
fn restore_never_resolves_cases_the_backup_inherited() {
    let f = F::new();
    f.json("run-fake", &["w", "a", "escape", "1000"]);
    f.json("recover", &[]);
    assert_eq!(
        f.count(
            "SELECT count(*) FROM recovery_case WHERE subject_key='attempt:a' AND state='open'"
        ),
        1
    );
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    f.json_at("recover", &target, &[]);
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let states: Vec<String> = ro
        .prepare("SELECT subject_key||'='||state FROM recovery_case WHERE subject_id='a' ORDER BY subject_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        states,
        ["attempt:a=open", "restore:attempt:a=open"],
        "the inherited case stays open beside the restore case"
    );
}

#[test]
fn restored_ledgers_never_close_notify_or_decide() {
    let f = F::new();
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    for (action, args) in [
        ("sweep", vec!["--backend", "sink"]),
        ("closure-summary", vec!["w", "cancelled"]),
        (
            "command-template",
            vec!["cancel", "work:x", "c1", "1", "{}"],
        ),
        ("submit", vec![]),
    ] {
        let out = f.cmd_on(action, &target).args(&args).output().unwrap();
        if action != "submit" {
            assert_eq!(out.status.code(), Some(6), "{action}");
            assert_eq!(
                fault(&out)["startup_refused"]["ledger"],
                "restored_inspect_only",
                "{action}"
            );
        }
    }
    let out = F::with_stdin(
        f.cmd_on("submit", &target),
        &serde_json::to_vec(&request("k2")).unwrap(),
    );
    assert_eq!(out.status.code(), Some(6), "no new work is accepted");
    assert_eq!(
        fs::read_dir(f.sink_dir()).unwrap().count(),
        0,
        "no notification"
    );
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let closed: i64 = ro
        .query_row("SELECT count(*) FROM work WHERE mode='closed'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(closed, 0);
}

#[test]
fn the_database_refuses_false_completion_and_resolution_claims() {
    let f = F::new();
    f.json("run-fake", &["w", "a", "escape", "1000"]);
    f.json("recover", &[]);
    let c = f.db();
    assert!(
        c.execute(
            "INSERT INTO recovery_run(id,coordinator_epoch,trigger,state,started_at,finished_at,examined,summary_json) VALUES('x',1,'explicit','completed',1,2,5,'{}')",
            [],
        )
        .is_err(),
        "a run cannot be inserted as completed"
    );
    let finished: String = c
        .query_row(
            "SELECT id FROM recovery_run WHERE state='completed' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        c.execute(
            "UPDATE recovery_case SET state='resolved',resolved_at=1,resolved_run_id=?1,resolution='x' WHERE subject_key='attempt:a'",
            [&finished],
        )
        .is_err(),
        "only a running pass resolves a case"
    );
    assert!(
        c.execute(
            "INSERT INTO recovery_case(id,subject_key,subject_kind,subject_id,reason,state,responsible,required_action,opened_run_id,opened_at,observation_json,resolved_run_id,resolved_at,resolution) VALUES('y','k','work','w','r','resolved','o','a',?1,1,'{}',?1,1,'x')",
            [&finished],
        )
        .is_err(),
        "a case cannot be inserted resolved"
    );
}

#[test]
fn a_busy_result_commit_after_the_ref_update_is_reported_committed_never_repeated() {
    let f = F::new();
    let effect = f.approved();
    let mut c = f.cmd("dispatch");
    c.arg(&effect);
    let (child, bar) = f.paused(c, "after_ref_cas", None);
    let lock = f.db();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let out = release(child, &bar);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    // T6 committed and the update ran; the executor-result commit then hit BUSY. The call did
    // commit, so it is never reported as "not committed".
    let j = assert_fault(&out, "T7", "committed", "busy", 5);
    assert_eq!(j["storage_fault"]["step"], "executor_result");
    assert_eq!(
        j["storage_fault"]["committed_before"],
        serde_json::json!(["T6"])
    );
    assert!(j["emergency_record"]["incident"].is_string());
    assert_eq!(f.cas_executions(), 1);
    assert_eq!(
        f.json("readback", &["T7", &effect, "executor_result"])["observed"],
        "absent"
    );
    assert_eq!(f.json("readback", &["T6", &effect])["observed"], "present");
    f.json("recover", &[]);
    assert_eq!(f.effect_state(&effect), "confirmed");
    assert_eq!((f.cas_executions(), f.ref_mutations(&effect)), (1, 1));
    assert_eq!(f.json("dispatch", &[&effect])["dispatched"], false);
    assert_eq!(f.cas_executions(), 1);
}

#[test]
fn a_refused_startup_of_a_cleanly_closed_ledger_creates_no_file() {
    let f = F::new();
    // Make it incompatible through a connection that then closes normally (no WAL remains).
    let c = f.db();
    c.execute("UPDATE schema_meta SET version=99", []).unwrap();
    drop(c);
    let names = || {
        let mut v: Vec<String> = fs::read_dir(&f.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    };
    let before = (names(), f.hashes());
    assert!(!f.root.join("ledger.db-wal").exists());
    for (action, args) in [
        ("reconcile", vec![]),
        ("recover", vec![]),
        ("attempts", vec![]),
        ("list", vec![]),
        ("get", vec!["w"]),
    ] {
        assert!(!f.run(action, &args).status.success(), "{action}");
        assert_eq!(
            (names(), f.hashes()),
            before,
            "{action} created or changed a file"
        );
    }
    let out = f.run("reconcile", &[]);
    assert_eq!(fault(&out)["startup_refused"]["startup_began"], false);
}

// ======================================================================= regression group 1

/// An idle read-only connection held by this test process (it never checkpoints). While any
/// connection holds the ledger, SQLite keeps using the shared WAL index, so a commit whose WAL
/// sync failed stays invisible to new readers until the last connection closes.
fn holder(f: &F) -> Connection {
    let c = f.ro();
    c.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )
    .unwrap();
    let _: i64 = c
        .query_row("SELECT count(*) FROM schema_meta", [], |r| r.get(0))
        .unwrap();
    c
}
/// The latest recovery item recorded for one imported emergency record.
fn incident_item(f: &F, incident: &str) -> (String, Value) {
    let (class, obs): (String, String) = f
        .ro()
        .query_row(
            "SELECT classification,observation_json FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1 ORDER BY rowid DESC LIMIT 1",
            [incident],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (class, serde_json::from_str(&obs).unwrap())
}
/// Run a command and fail the test if it does not finish within the limit.
fn bounded(mut c: Command, limit: Duration) -> Output {
    let mut ch = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + limit;
    while ch.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = ch.kill();
            let _ = ch.wait();
            panic!("command did not finish within {limit:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    ch.wait_with_output().unwrap()
}

#[test]
fn a_readback_that_misses_an_uncertain_commit_is_never_evidence_and_recover_settles_it() {
    let f = F::bare();
    let h = holder(&f);
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_sync:ioerr");
    let out = F::with_stdin(c, &serde_json::to_vec(&request("k1")).unwrap());
    let j = assert_fault(&out, "T1", "unknown", "io_err", 4);
    let incident = j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string();
    // The faulting process has ended, but another connection still holds the ledger.
    let early = f.readback("T1", "k1");
    assert_eq!(early["observed"], "absent");
    assert_eq!(early["establishes_absence"], false, "{early}");
    drop(h);
    // Nothing wrote in between, yet the commit appears: the earlier absence was not evidence.
    let late = f.readback("T1", "k1");
    assert_eq!(late["observed"], "present", "{late}");
    f.json("recover", &[]);
    let (class, obs) = incident_item(&f, &incident);
    assert_eq!(
        (class.as_str(), &obs["readback"]),
        ("accounted", &json!("present"))
    );
    let r: Value = serde_json::from_slice(&f.submit("k1").stdout).unwrap();
    assert_eq!(r["disposition"], "existing");
    assert_eq!(work_rows(&f, "k1"), 1);
}

#[test]
fn a_dispatch_retried_after_an_invisible_claim_runs_the_action_at_most_once() {
    let f = F::new();
    let effect = f.approved();
    let h = holder(&f);
    let out = f
        .cmd("dispatch")
        .arg(&effect)
        .env("ASHER_TEST_STORAGE_FAULT", "T6:wal_sync:ioerr")
        .output()
        .unwrap();
    assert_fault(&out, "T6", "unknown", "io_err", 4);
    assert_eq!(f.readback("T6", &effect)["establishes_absence"], false);
    // The owner retries while the other connection is still open, then again after it closes,
    // and recovery runs in between: the fixture ref is updated at most once.
    let _ = f.run("dispatch", &[&effect]);
    drop(h);
    for _ in 0..2 {
        f.json("recover", &[]);
        f.json("reconcile-effects", &[]);
        let _ = f.run("dispatch", &[&effect]);
    }
    assert!(f.cas_executions() <= 1, "{}", f.cas_executions());
    assert!(f.ref_mutations(&effect) <= 1);
    assert_eq!(f.cas_executions(), f.ref_mutations(&effect));
}

#[test]
fn t4_a_review_that_committed_and_was_then_retired_is_never_recorded_as_not_committed() {
    let f = F::new();
    f.validated();
    let h = holder(&f);
    let out = f
        .cmd("review-create")
        .arg("a")
        .env("ASHER_TEST_STORAGE_FAULT", "T4:wal_sync:ioerr")
        .output()
        .unwrap();
    let j = assert_fault(&out, "T4", "unknown", "io_err", 4);
    let minted = j["storage_fault"]["stable_id"]
        .as_str()
        .unwrap()
        .to_string();
    let incident = j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string();
    drop(h);
    // The owner revokes the permit; the next startup retires the review it now finds, before
    // the emergency record is imported.
    assert!(f.run("revoke", &["p"]).status.success());
    f.json("recover", &[]);
    let state = f.text(&format!("SELECT state FROM review WHERE id='{minted}'"));
    assert!(
        !matches!(state.as_str(), "pending" | "authorized"),
        "the review was retired: {state}"
    );
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM event WHERE kind='review.created' AND entity_id='{minted}'"
        )),
        1
    );
    let (class, obs) = incident_item(&f, &incident);
    assert_eq!(class, "accounted");
    assert_eq!(obs["readback"], "present", "{obs}");
    assert_eq!(obs["detail"]["review_id"], minted.as_str());
}

#[test]
fn t4_an_earlier_review_never_answers_for_a_later_creation_that_did_not_commit() {
    let f = F::new();
    let effect = f.approved();
    let (child, _b) = f.paused(
        {
            let mut c = f.cmd("dispatch");
            c.arg(&effect);
            c
        },
        "after_effect_claim",
        None,
    );
    kill(child);
    f.json("reconcile-effects", &[]);
    assert_eq!(f.effect_state(&effect), "not_applied");
    let consumed = f.text("SELECT id FROM review");
    assert_eq!(f.text("SELECT state FROM review"), "consumed");
    let out = f
        .cmd("review-create")
        .arg("a")
        .env("ASHER_TEST_STORAGE_FAULT", "T4:wal_write:full")
        .output()
        .unwrap();
    let j = assert_fault(&out, "T4", "unknown", "full", 4);
    let minted = j["storage_fault"]["stable_id"]
        .as_str()
        .unwrap()
        .to_string();
    let incident = j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(minted, consumed);
    assert_eq!(f.count("SELECT count(*) FROM review"), 1);
    assert_eq!(f.readback("T4", &minted)["observed"], "absent");
    f.json("recover", &[]);
    let (class, obs) = incident_item(&f, &incident);
    assert_eq!(
        (class.as_str(), &obs["readback"]),
        ("accounted", &json!("absent"))
    );
    // The one later, unfaulted creation makes the one new review.
    let rid = f.review();
    assert!(rid != consumed && rid != minted);
    assert_eq!(f.count("SELECT count(*) FROM review"), 2);
}

/// Not a test of its own: when re-executed with ASHER_RECOVERY_JOURNAL_DB set, this process
/// switches the database to rollback-journal mode, spills part of an uncommitted transaction
/// into the main file and exits without committing, leaving a genuine hot journal.
#[test]
fn helper_crashed_rollback_writer() {
    let Some(db) = std::env::var_os("ASHER_RECOVERY_JOURNAL_DB") else {
        return;
    };
    let c = Connection::open(db).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA cache_size=2; BEGIN IMMEDIATE;
         UPDATE coordinator_epoch SET epoch=epoch+1000;
         CREATE TABLE spill(x);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000)
          INSERT INTO spill SELECT randomblob(900) FROM n;",
    )
    .unwrap();
    unsafe { libc::_exit(0) }
}

#[test]
fn a_rollback_journal_beside_the_ledger_is_never_played_back() {
    let f = F::new();
    let epoch = f.count("SELECT epoch FROM coordinator_epoch");
    let db = f.root.join("ledger.db");
    let journal = f.root.join("ledger.db-journal");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_crashed_rollback_writer",
            "--test-threads=1",
        ])
        .env("ASHER_RECOVERY_JOURNAL_DB", &db)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(fs::metadata(&journal).unwrap().len() > 0);
    // It is a genuine hot journal: plain SQLite plays it back on a copy.
    let copy = f.temp.path().join("copy");
    fs::create_dir(&copy).unwrap();
    fs::copy(&db, copy.join("ledger.db")).unwrap();
    fs::copy(&journal, copy.join("ledger.db-journal")).unwrap();
    let played: i64 = Connection::open(copy.join("ledger.db"))
        .unwrap()
        .query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))
        .unwrap();
    assert_eq!(played, epoch);
    assert!(!copy.join("ledger.db-journal").exists());
    // The ledger itself is refused by every path, and neither file changes.
    let before = (sha(&db), sha(&journal));
    let wal_before = f.root.join("ledger.db-wal").exists();
    for (action, args) in [
        ("reconcile", vec![]),
        ("recover", vec![]),
        ("list", vec![]),
        ("get", vec!["w"]),
        ("readback", vec!["T1", "w"]),
        ("attempts", vec![]),
        ("revoke", vec!["p"]),
        ("backup", vec!["/nonexistent-never-created"]),
    ] {
        let out = f.run(action, &args);
        if action == "readback" {
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(v["observed"], "unavailable", "{v}");
        } else {
            assert!(!out.status.success(), "{action} must refuse");
        }
        assert_eq!(
            (sha(&db), sha(&journal)),
            before,
            "{action} changed the ledger"
        );
    }
    assert_eq!(f.root.join("ledger.db-wal").exists(), wal_before);
    let out = f.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    let r = &fault(&out)["startup_refused"];
    assert_eq!(
        (&r["ledger"], &r["startup_began"]),
        (&json!("unreadable"), &json!(false))
    );
    let d = f.diagnose();
    assert_eq!(d["status"], "unreadable", "{d}");
    assert_eq!(d["files"]["ledger.db-journal"]["present"], true);
    assert_eq!(d["ledger"]["open"], "not attempted");
    assert_eq!((sha(&db), sha(&journal)), before, "diagnose");
}

#[test]
fn a_journal_or_wal_that_is_not_a_regular_file_never_blocks_or_passes_diagnosis() {
    use std::os::unix::ffi::OsStrExt;
    for (name, fifo, status) in [
        ("ledger.db-journal", true, "unreadable"),
        ("ledger.db-journal", false, "unreadable"),
        ("ledger.db-wal", true, "unavailable"),
    ] {
        let f = F::new();
        let p = f.root.join(name);
        if fifo {
            let cs = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(cs.as_ptr(), 0o600) }, 0);
        } else {
            fs::create_dir(&p).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let started = Instant::now();
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg("diagnose").arg(&f.root);
        let out = bounded(c, Duration::from_secs(15));
        assert!(out.status.success());
        let d: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(d["status"], status, "{name} fifo={fifo}: {d}");
        assert_ne!(d["next"], "ordinary startup can proceed");
        for (action, args) in [
            ("reconcile", vec![]),
            ("recover", vec![]),
            ("list", vec![]),
            ("attempts", vec![]),
            ("revoke", vec!["p"]),
        ] {
            let mut c = f.cmd(action);
            c.args(&args);
            let out = bounded(c, Duration::from_secs(15));
            assert!(!out.status.success(), "{action} with {name} fifo={fifo}");
        }
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}

#[test]
fn lock_free_paths_never_checkpoint_a_blocked_ledger_or_write_a_derived_one() {
    // A breaker-blocked ledger whose killed passes left their WAL.
    let f = F::new();
    for _ in 0..3 {
        let (child, _b) = f.paused(f.cmd("reconcile"), "during_recovery_run", None);
        kill(child);
    }
    let out = f.run("reconcile", &[]);
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "degraded_startup_blocked"
    );
    let files = f.hashes();
    assert_ne!(files.1, "ABSENT", "the killed passes left their WAL");
    assert!(f.run("attempts", &[]).status.success());
    assert_eq!(
        f.hashes(),
        files,
        "attempts never checkpoints or removes the WAL"
    );
    // Revocation stays available (it narrows authority and runs no startup), does not reopen
    // ordinary startup, and does not checkpoint the blocked ledger when it closes.
    assert!(f.run("revoke", &["p"]).status.success());
    assert!(f.root.join("ledger.db-wal").exists());
    assert_eq!(sha(&f.root.join("ledger.db")), files.0);
    assert_eq!(f.run("reconcile", &[]).status.code(), Some(6));

    // A ledger restored from a backup taken before the source claimed an effect.
    let g = F::new();
    let effect = g.approved();
    let backup = g.temp.path().join("bk");
    g.json("backup", &[backup.to_str().unwrap()]);
    let (child, _b) = g.paused(
        {
            let mut c = g.cmd("dispatch");
            c.arg(&effect);
            c
        },
        "after_effect_claim",
        None,
    );
    kill(child);
    let target = g.temp.path().join("restored");
    g.json_at("restore", &backup, &[target.to_str().unwrap()]);
    let before = file_hashes(&target);
    let out = g.cmd_on("revoke", &target).arg("p").output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("revoke refused") && err.contains("derived from a backup"),
        "{err}"
    );
    assert_eq!(
        file_hashes(&target),
        before,
        "revoke wrote a restored ledger"
    );
    // Read-only inspection of it stays available and writes nothing (an ordinary read-only
    // open may add an empty WAL index beside it, never content).
    assert!(
        g.cmd_on("attempts", &target)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        sha(&target.join("ledger.db")),
        before.0,
        "attempts wrote a restored ledger"
    );
    assert!(fs::metadata(target.join("ledger.db-wal")).map_or(0, |m| m.len()) == 0);
    // The live source still revokes, and truthfully reports the claim as too late.
    let v = g.json("revoke", &["p"]);
    assert_eq!(v["detail"]["too_late_to_guarantee_prevention"], true, "{v}");
    // A hand copy of the snapshot is refused the same way.
    let copy = g.temp.path().join("copy");
    fs::create_dir(&copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(backup.join("snapshot.db"), copy.join("ledger.db")).unwrap();
    fs::set_permissions(copy.join("ledger.db"), fs::Permissions::from_mode(0o600)).unwrap();
    let copied = file_hashes(&copy);
    assert!(
        !g.cmd_on("revoke", &copy)
            .arg("p")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(file_hashes(&copy), copied);
}

#[test]
fn diagnose_names_startup_preconditions_that_fail_every_startup() {
    let cases: [(&str, &str); 3] = [
        (
            "UPDATE coordinator_epoch SET epoch=epoch-1",
            "an owner session already holds epoch",
        ),
        (
            "UPDATE coordinator_epoch SET epoch=9223372036854775807",
            "epoch is at its maximum",
        ),
        (
            "DROP TABLE IF EXISTS evidence_gap; DROP TABLE IF EXISTS time_anomaly; DROP TABLE IF EXISTS time_floor; DROP TABLE IF EXISTS host_boot; DROP TABLE ledger_snapshot; DROP TABLE ledger_restore; DROP TABLE recovery_item;
             DROP TABLE recovery_case; DROP TABLE recovery_run; UPDATE schema_meta SET version=6;
             CREATE TABLE recovery_run(x);",
            "table recovery_run already exists",
        ),
    ];
    for (sql, needle) in cases {
        let f = F::new();
        f.db().execute_batch(sql).unwrap();
        let db = sha(&f.root.join("ledger.db"));
        let d = f.diagnose();
        assert_eq!(d["status"], "degraded", "{sql}: {d}");
        assert_eq!(d["accessible"], true);
        assert!(
            d["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains(needle)),
            "{sql}: {d}"
        );
        for action in ["reconcile", "recover"] {
            let out = f.run(action, &[]);
            assert_eq!(out.status.code(), Some(6), "{action}: {sql}");
            let r = &fault(&out)["startup_refused"];
            assert_eq!(r["ledger"], "degraded_startup_failed", "{action}: {r}");
            assert_eq!(r["startup_began"], false);
            assert!(
                r["preserved"]
                    .as_str()
                    .unwrap()
                    .contains("nothing was written")
            );
        }
        assert_eq!(
            sha(&f.root.join("ledger.db")),
            db,
            "{sql}: nothing was written"
        );
    }
    // Adverse types in recovery history no longer fail admission or the status read.
    let f = F::new();
    f.db()
        .execute_batch(
            "DROP TRIGGER recovery_run_identity_immutable; DROP TRIGGER recovery_run_finishes_once;
             UPDATE recovery_run SET started_at='yesterday' WHERE rowid=(SELECT max(rowid) FROM recovery_run);",
        )
        .unwrap();
    assert!(f.run("reconcile", &[]).status.success());
    let rec = f.json("recover", &[]);
    assert!(rec["status"]["recent_runs"].is_array(), "{rec}");
    assert_eq!(f.diagnose()["status"], "healthy");
}

#[test]
fn a_restored_ledger_never_resolves_an_inherited_truncated_emergency_log_case() {
    let f = F::new();
    let pad = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(4000));
    let log = f.root.join("emergency.jsonl");
    fs::write(&log, pad.repeat(70)).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    f.json("recover", &[]);
    let key = "storage_incident:emergency-log-truncated";
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM recovery_case WHERE subject_key='{key}' AND state='open'"
        )),
        1
    );
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    f.json_at("recover", &target, &[]);
    let state: String =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row(
                "SELECT state FROM recovery_case WHERE subject_key=?1",
                [key],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        state, "open",
        "an inherited case is never resolved by inference"
    );
}

// ======================================================================= regression group 2

/// A schema-5 ledger modelled from a current one (the same model `tests/review.rs` uses as
/// `GATE5_TEARDOWN`); the caller then sets the version.
const SCHEMA5_MODEL: &str = "
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
const SCHEMA6_MODEL: &str =
    "DROP TABLE IF EXISTS evidence_gap; DROP TABLE IF EXISTS time_anomaly; DROP TABLE IF EXISTS time_floor; DROP TABLE IF EXISTS host_boot; DROP TABLE ledger_snapshot; DROP TABLE ledger_restore; DROP TABLE recovery_item;
     DROP TABLE recovery_case; DROP TABLE recovery_run; UPDATE schema_meta SET version=6;";

#[test]
fn diagnose_dry_runs_the_startup_steps_and_names_any_precondition_that_fails_them() {
    let schema5 = format!("{SCHEMA5_MODEL} UPDATE schema_meta SET version=5;");
    // Each stray object makes a pending migration, and so every ordinary startup, fail.
    let cases = [
        (
            format!("{SCHEMA6_MODEL} CREATE INDEX recovery_case_by_work ON work(id);"),
            "recovery_case_by_work already exists",
        ),
        (
            format!(
                "{SCHEMA6_MODEL} CREATE TRIGGER recovery_item_immutable BEFORE UPDATE ON permit BEGIN SELECT 1; END;"
            ),
            "recovery_item_immutable already exists",
        ),
        (
            format!("{schema5} CREATE TABLE owner_command(x);"),
            "owner_command already exists",
        ),
    ];
    for (sql, needle) in &cases {
        let f = F::new();
        f.db().execute_batch(sql).unwrap();
        let db = sha(&f.root.join("ledger.db"));
        let d = f.diagnose();
        assert_eq!(d["status"], "degraded", "{needle}: {d}");
        assert_ne!(d["next"], "ordinary startup can proceed");
        assert!(
            d["reasons"].as_array().unwrap().iter().any(|r| {
                let r = r.as_str().unwrap();
                r.contains("ordinary startup cannot begin") && r.contains(needle)
            }),
            "{needle}: {d}"
        );
        for action in ["reconcile", "recover"] {
            let out = f.run(action, &[]);
            assert_eq!(out.status.code(), Some(6), "{action}: {needle}");
            let r = &fault(&out)["startup_refused"];
            assert_eq!(r["ledger"], "degraded_startup_failed", "{r}");
            assert_eq!(r["startup_began"], false);
        }
        assert_eq!(
            sha(&f.root.join("ledger.db")),
            db,
            "{needle}: nothing written"
        );
    }
    // Controls: the same models without a stray object pass the dry run and start normally.
    for sql in [SCHEMA6_MODEL.to_string(), schema5] {
        let f = F::new();
        f.db().execute_batch(&sql).unwrap();
        let d = f.diagnose();
        assert_eq!(d["ledger"]["startup_dry_run"], "ok", "{d}");
        assert!(f.run("reconcile", &[]).status.success());
        assert_eq!(f.count("SELECT version FROM schema_meta"), 8);
    }
    // Adverse recovery history: a 'started' run whose id is a BLOB no longer fails startup;
    // it is recorded as interrupted like any run that never finished.
    let f = F::new();
    f.db()
        .execute(
            "INSERT INTO recovery_run(rowid,id,coordinator_epoch,trigger,state,started_at) VALUES(0,X'01',1,'startup','started',0)",
            [],
        )
        .unwrap();
    assert_eq!(f.diagnose()["ledger"]["startup_dry_run"], "ok");
    assert!(f.run("reconcile", &[]).status.success());
    assert_eq!(
        f.text("SELECT state FROM recovery_run WHERE rowid=0"),
        "interrupted"
    );
}

#[test]
fn an_uncertain_startup_or_migration_commit_is_never_reported_as_nothing_written() {
    for (schema6, rule) in [
        (false, "EPOCH:wal_sync:ioerr"),
        (false, "EPOCH:wal_write:full"),
        (true, "MIGRATE:wal_sync:ioerr"),
    ] {
        let f = F::new();
        if schema6 {
            f.db().execute_batch(SCHEMA6_MODEL).unwrap();
        }
        let out = f
            .cmd("reconcile")
            .env("ASHER_TEST_STORAGE_FAULT", rule)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(6), "{rule}");
        let r = &fault(&out)["startup_refused"];
        assert_eq!(r["startup_began"], "unknown", "{rule}: {r}");
        let preserved = r["preserved"].as_str().unwrap();
        assert!(
            preserved.contains("outcome is unknown"),
            "{rule}: {preserved}"
        );
        assert!(
            !preserved.contains("nothing was written"),
            "{rule}: {preserved}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("outcome is unknown"),
            "{rule}"
        );
        // Whatever landed, the next startup accounts for it and proceeds; nothing is left
        // claiming to run.
        assert!(f.run("reconcile", &[]).status.success(), "{rule}");
        assert_eq!(f.count("SELECT version FROM schema_meta"), 8);
        assert_eq!(
            f.count("SELECT count(*) FROM recovery_run WHERE state='started'"),
            0,
            "{rule}"
        );
    }
}

#[test]
fn a_ledger_derived_from_a_backup_never_settles_an_emergency_record_by_absence() {
    let f = F::bare();
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    // After the backup, an uncertain T1 commit that did commit on the source.
    let h = holder(&f);
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_sync:ioerr");
    let out = F::with_stdin(c, &serde_json::to_vec(&request("k1")).unwrap());
    let j = assert_fault(&out, "T1", "unknown", "io_err", 4);
    let incident = j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string();
    drop(h);
    assert_eq!(f.readback("T1", "k1")["observed"], "present");
    // The owner restores the older backup and puts the emergency log beside it to inspect.
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    let log = target.join("emergency.jsonl");
    fs::copy(f.root.join("emergency.jsonl"), &log).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    f.json_at("recover", &target, &[]);
    let ro =
        Connection::open_with_flags(target.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let (class, case): (String, Option<String>) = ro
        .query_row(
            "SELECT classification,case_id FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1 ORDER BY rowid DESC LIMIT 1",
            [&incident],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        class, "unresolved",
        "absence in a backup establishes nothing"
    );
    let state: String = ro
        .query_row(
            "SELECT state FROM recovery_case WHERE id=?1",
            [case.unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "open");
    // The live source settles the same record on its own evidence.
    f.json("recover", &[]);
    let (class, obs) = incident_item(&f, &incident);
    assert_eq!(
        (class.as_str(), &obs["readback"]),
        ("accounted", &json!("present"))
    );
}

#[test]
fn a_failed_or_interrupted_backup_never_leaves_a_snapshot_or_a_usable_ledger() {
    let f = F::new();
    // A storage failure after the copy, at the commit of the snapshot marker.
    let dest = f.temp.path().join("bk-full");
    let out = f
        .cmd("backup")
        .arg(&dest)
        .env("ASHER_TEST_STORAGE_FAULT", "BACKUP:wal_write:full")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no verified snapshot exists"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dest.exists(),
        "the partial copy and its empty directory are removed"
    );
    // Killed right after the copy, before the marker: nothing carries the snapshot's name or
    // a manifest, so the leftover is neither restorable nor mistaken for a backup.
    let dest = f.temp.path().join("bk-kill");
    let mut c = f.cmd("backup");
    c.arg(&dest);
    let (child, _b) = f.paused(c, "after_backup_copy", None);
    kill(child);
    let names: Vec<String> = fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(!names.is_empty());
    assert!(
        names.iter().all(|n| n.starts_with(".snapshot-")),
        "{names:?}"
    );
    let out = f
        .cmd_on("restore", &dest)
        .arg(f.temp.path().join("never"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!f.temp.path().join("never").exists());
    // A backup that completes is renamed into place, marked and verified.
    let dest = f.temp.path().join("bk-ok");
    f.json("backup", &[dest.to_str().unwrap()]);
    let mut names: Vec<String> = fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["backup-manifest.json", "snapshot.db"]);
    let marked: i64 =
        Connection::open_with_flags(dest.join("snapshot.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT count(*) FROM ledger_snapshot", [], |r| r.get(0))
            .unwrap();
    assert_eq!(marked, 1);
}

/// Make one entry of the UNIQUE(principal, request_key) index of `work` disagree with its row
/// (the table itself is untouched): structurally valid, so quick_check still passes.
fn corrupt_unique_index_entry(db: &Path) {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let indexes: Vec<(String, i64)> = c
        .prepare("SELECT name,rootpage FROM sqlite_master WHERE type='index' AND tbl_name='work'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let root = indexes
        .iter()
        .find(|(name, _)| {
            let cols: Vec<String> = c
                .prepare(&format!("PRAGMA index_info(\"{name}\")"))
                .unwrap()
                .query_map([], |r| r.get(2))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            cols == ["principal", "request_key"]
        })
        .expect("unique index over (principal, request_key)")
        .1;
    drop(c);
    let mut b = fs::read(db).unwrap();
    let ps = match u16::from_be_bytes([b[16], b[17]]) {
        1 => 65536,
        n => n as usize,
    };
    let page = (root as usize - 1) * ps..root as usize * ps;
    let at = b[page.clone()]
        .windows(11)
        .position(|w| w == b"local-owner")
        .expect("index entry")
        + page.start;
    b[at + 10] = b'x';
    fs::write(db, b).unwrap();
}

#[test]
fn backup_restore_and_diagnose_run_the_full_integrity_check() {
    let f = F::new();
    let good = f.temp.path().join("bk-good");
    f.json("backup", &[good.to_str().unwrap()]);
    let db = f.root.join("ledger.db");
    corrupt_unique_index_entry(&db);
    // Independent oracle: the quick check passes, the full check does not.
    let oracle = |p: &Path| -> (String, String) {
        let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        (
            c.query_row("PRAGMA quick_check", [], |r| r.get(0)).unwrap(),
            c.query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
        )
    };
    let (quick, full) = oracle(&db);
    assert_eq!(quick, "ok");
    assert_ne!(full, "ok");
    let d = f.diagnose();
    assert_eq!(d["status"], "corrupt", "{d}");
    // Ordinary startup refuses it too, so the damaged UNIQUE index can never admit a second
    // acceptance of the same request key.
    let before = sha(&db);
    let out = f.submit("w");
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(fault(&out)["startup_refused"]["ledger"], "corrupt");
    assert_eq!(
        f.count("SELECT count(*) FROM work NOT INDEXED WHERE request_key='w'"),
        1
    );
    assert_eq!(sha(&db), before);
    let dest = f.temp.path().join("bk-bad");
    let out = f.run("backup", &[dest.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("integrity"));
    assert!(!dest.exists());
    // Restore checks the snapshot itself: one whose index disagrees with its rows is refused
    // even when its manifest digest is made to match (a same-user edit, used here only to
    // isolate restore's own verification).
    let snap = good.join("snapshot.db");
    corrupt_unique_index_entry(&snap);
    assert_eq!(oracle(&snap).0, "ok");
    let mpath = good.join("backup-manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mpath).unwrap()).unwrap();
    m["ledger_sha256"] = json!(sha(&snap));
    fs::write(&mpath, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    let target = f.temp.path().join("restored");
    let out = f.cmd_on("restore", &good).arg(&target).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("integrity"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!target.exists());
}

#[test]
fn a_wal_without_its_main_file_is_unreadable_preserved_and_never_a_restore_parent() {
    let f = F::new();
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let db = f.root.join("ledger.db");
    crashed_writer(
        &db,
        "UPDATE coordinator_epoch SET committed_heartbeat_at=committed_heartbeat_at+1",
    );
    // The main file is moved aside; its WAL stays.
    fs::rename(&db, f.temp.path().join("moved-ledger.db")).unwrap();
    let wal = sha(&f.root.join("ledger.db-wal"));
    let out = f.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(fault(&out)["startup_refused"]["ledger"], "unreadable");
    assert_eq!(f.diagnose()["status"], "unreadable");
    assert!(!db.exists(), "nothing was created");
    assert_eq!(sha(&f.root.join("ledger.db-wal")), wal);
    let inside = f.root.join("restored");
    let out = f.cmd_on("restore", &backup).arg(&inside).output().unwrap();
    assert!(!out.status.success());
    assert!(!inside.exists());
}

// ======================================================================= regression group 3

const MIB: u64 = 1024 * 1024;
/// A schema-6 ledger padded past the in-memory dry-run bound, with or without a stray index
/// that makes migration 007, and so every ordinary startup, fail.
fn padded_schema6(stray_index: bool) -> F {
    let f = F::new();
    let mut sql = SCHEMA6_MODEL.to_string();
    if stray_index {
        sql.push_str(" CREATE INDEX recovery_case_by_work ON work(id);");
    }
    sql.push_str(
        " CREATE TABLE zz_pad(b BLOB); INSERT INTO zz_pad VALUES(zeroblob(70*1024*1024));",
    );
    f.db().execute_batch(&sql).unwrap();
    assert!(
        fs::metadata(f.root.join("ledger.db")).unwrap().len() > 64 * MIB,
        "the ledger must exceed the dry-run bound"
    );
    f
}
fn names(d: &Value, needle: &str) -> bool {
    d["ledger"]["checks_not_completed"]
        .as_array()
        .unwrap_or_else(|| panic!("checks_not_completed missing: {d}"))
        .iter()
        .any(|c| c.as_str().unwrap().contains(needle))
}

#[test]
fn diagnose_never_reports_healthy_when_the_startup_dry_run_did_not_run_over_its_size_bound() {
    let f = padded_schema6(true);
    let db = sha(&f.root.join("ledger.db"));
    let d = f.diagnose();
    assert_eq!(d["status"], "inconclusive", "{d}");
    assert_ne!(d["status"], "healthy");
    assert_ne!(d["next"], "ordinary startup can proceed");
    assert!(
        d["ledger"]["startup_dry_run"]
            .as_str()
            .unwrap()
            .starts_with("not run"),
        "{d}"
    );
    assert!(names(&d, "startup dry run"), "{d}");
    let out = f.run("reconcile", &[]);
    assert_eq!(out.status.code(), Some(6));
    let r = &fault(&out)["startup_refused"];
    assert_eq!(r["ledger"], "degraded_startup_failed", "{r}");
    assert_eq!(r["startup_began"], false);
    assert_eq!(sha(&f.root.join("ledger.db")), db, "nothing written");
    // Control: without the stray index the ledger is still not called healthy (the dry run
    // did not run), and ordinary startup migrates it.
    let f = padded_schema6(false);
    let d = f.diagnose();
    assert_eq!(d["status"], "inconclusive", "{d}");
    assert!(names(&d, "startup dry run"), "{d}");
    let out = f.run("reconcile", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.count("SELECT version FROM schema_meta"), 8);
}

#[test]
fn diagnose_never_reports_healthy_when_the_startup_dry_run_is_cut_off_by_its_budget() {
    let f = F::new();
    f.db()
        .execute_batch(
            "CREATE TRIGGER zz_spin AFTER INSERT ON owner_session BEGIN SELECT count(*) FROM (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n) SELECT x FROM n); END;",
        )
        .unwrap();
    let db = sha(&f.root.join("ledger.db"));
    let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
    c.arg("diagnose").arg(&f.root);
    let out = bounded(c, Duration::from_secs(20));
    assert!(out.status.success(), "diagnose must always report");
    let d: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(d["status"], "inconclusive", "{d}");
    assert_ne!(d["next"], "ordinary startup can proceed");
    assert!(
        d["ledger"]["startup_dry_run"]
            .as_str()
            .unwrap()
            .starts_with("not completed"),
        "{d}"
    );
    assert!(names(&d, "startup dry run"), "{d}");
    assert!(names(&d, "recovery checks"), "{d}");
    assert!(d["elapsed_ms"].as_u64().unwrap() < 10000, "{d}");
    assert_eq!(sha(&f.root.join("ledger.db")), db, "nothing written");
    // `reconcile` and `recover` on this ledger run without end (a known, separately accepted
    // limitation), so they are not run here.
}

#[test]
fn diagnose_never_reports_a_restored_ledger_healthy_when_its_checks_ran_out_of_budget() {
    let f = F::new();
    let backup = f.temp.path().join("bk");
    f.json("backup", &[backup.to_str().unwrap()]);
    let target = f.temp.path().join("restored");
    f.json_at("restore", &backup, &[target.to_str().unwrap()]);
    // Control: with its budget intact the restored ledger is degraded, for its marker.
    let d = F::diagnose_at(&target);
    assert_eq!(d["status"], "degraded", "{d}");
    assert!(
        d["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("derived from a backup")),
        "{d}"
    );
    // With the budget gone after the quick check, the checks that would find the marker do
    // not run: that is never healthy.
    let out = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .arg("diagnose")
        .arg(&target)
        .env("ASHER_TEST_DIAGNOSE_DELAY_MS", "5500")
        .output()
        .unwrap();
    assert!(out.status.success(), "diagnose must always report");
    let d: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_ne!(d["status"], "healthy", "{d}");
    assert!(
        d["status"] == "inconclusive" || d["status"] == "degraded",
        "{d}"
    );
    assert_ne!(d["next"], "ordinary startup can proceed");
    assert!(names(&d, "full integrity check"), "{d}");
    assert!(names(&d, "startup dry run"), "{d}");
    assert!(names(&d, "recovery checks"), "{d}");
    let out = f.cmd_on("reconcile", &target).output().unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(
        fault(&out)["startup_refused"]["ledger"],
        "restored_inspect_only"
    );
}

/// Not a test of its own: when re-executed with ASHER_RECOVERY_HOLD_SHM/_READY set, this
/// process takes an exclusive POSIX lock on bytes 124..127 of the named shared-memory file
/// (the WAL-index read-mark locks 1..4), signals readiness and sleeps until killed. While a
/// separate process holds them, SQLite cannot begin a new read transaction once the WAL holds
/// frames, and fails it with SQLITE_PROTOCOL.
#[test]
fn helper_wal_read_mark_holder() {
    let (Some(shm), Some(ready)) = (
        std::env::var_os("ASHER_RECOVERY_HOLD_SHM"),
        std::env::var_os("ASHER_RECOVERY_HOLD_READY"),
    ) else {
        return;
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(shm)
        .unwrap();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 124;
    lock.l_len = 4;
    let rc = unsafe {
        libc::fcntl(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::F_SETLK,
            &lock as *const libc::flock,
        )
    };
    assert_eq!(rc, 0, "F_SETLK: {}", std::io::Error::last_os_error());
    fs::write(ready, b"").unwrap();
    let end = Instant::now() + Duration::from_secs(120);
    while Instant::now() < end {
        thread::sleep(Duration::from_millis(100));
    }
    drop(file);
}

#[test]
fn a_migration_whose_schema_read_back_fails_is_never_reported_as_nothing_written() {
    let f = F::new();
    f.db().execute_batch(SCHEMA6_MODEL).unwrap();
    // Paused right after migration 007 committed: the WAL holds its frames.
    let (child, b) = f.paused(f.cmd("reconcile"), "after_migration_commit", None);
    let ready = b.path().join("holder-ready");
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_wal_read_mark_holder", "--test-threads=1"])
        .env("ASHER_RECOVERY_HOLD_SHM", f.root.join("ledger.db-shm"))
        .env("ASHER_RECOVERY_HOLD_READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        if holder.try_wait().unwrap().is_some() || Instant::now() > end {
            let _ = holder.kill();
            let mut err = String::new();
            if let Some(mut e) = holder.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            let _ = holder.wait();
            panic!("read-mark holder not ready: {err}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    // The coordinator's next read of the schema version fails under the held read marks.
    let out = release(child, &b);
    let _ = holder.kill();
    let _ = holder.wait();
    assert_eq!(
        out.status.code(),
        Some(6),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = &fault(&out)["startup_refused"];
    assert_eq!(r["startup_began"], false, "{r}");
    let preserved = r["preserved"].as_str().unwrap();
    assert!(!preserved.contains("nothing was written"), "{preserved}");
    assert!(preserved.contains("is unknown"), "{preserved}");
    assert!(preserved.contains("could not be read back"), "{preserved}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("nothing was written"));
    // Oracle, read once the holder is gone: the migration did commit, so "nothing was
    // written" would have been false.
    assert_eq!(f.count("SELECT version FROM schema_meta"), 7);
    let out = f.run("reconcile", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_run WHERE state='started'"),
        0
    );
}

// ---- The emergency-record check is truthful about what it could not verify.

/// One product-written incident whose commit outcome is unknown (T1 WAL-sync I/O error on
/// `submit`), beside a bare ledger. Nothing has imported it, so it is pending.
fn pending_incident(f: &F, key: &str) -> String {
    let mut c = f.submit_cmd();
    c.env("ASHER_TEST_STORAGE_FAULT", "T1:wal_sync:ioerr");
    let out = F::with_stdin(c, &serde_json::to_vec(&request(key)).unwrap());
    let j = assert_fault(&out, "T1", "unknown", "io_err", 4);
    j["emergency_record"]["incident"]
        .as_str()
        .unwrap()
        .to_string()
}

fn emergency_check_not_completed(d: &Value) -> bool {
    d["ledger"]["checks_not_completed"]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x.as_str().unwrap().contains("emergency")))
}

#[test]
fn diagnose_never_reports_healthy_when_the_emergency_log_cannot_be_read() {
    let f = F::bare();
    pending_incident(&f, "k1");
    let control = f.diagnose();
    assert_eq!(control["status"], "degraded", "{control}");
    assert_eq!(control["ledger"]["emergency_records_not_settled"], 1);
    let log = f.root.join("emergency.jsonl");
    fs::set_permissions(&log, fs::Permissions::from_mode(0o000)).unwrap();
    let (db, wal) = f.hashes();
    let d = f.diagnose();
    assert_eq!(d["status"], "inconclusive", "{d}");
    assert!(emergency_check_not_completed(&d), "{d}");
    assert!(
        d["ledger"]["emergency_records_not_settled"].is_null(),
        "a failed read establishes no count: {d}"
    );
    assert!(d["emergency_log"]["error"].is_string());
    assert_eq!(f.hashes(), (db, wal), "diagnose wrote nothing");
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn diagnose_never_reports_healthy_when_the_emergency_log_exceeds_its_bound() {
    let f = F::bare();
    let settled = pending_incident(&f, "k1");
    assert_eq!(f.json("recover", &[])["recovery_run"]["state"], "completed");
    let pending = pending_incident(&f, "k2");
    let log = f.root.join("emergency.jsonl");
    let text = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains(&settled) && lines[1].contains(&pending));
    // Only product-written lines, reordered: the settled record repeated past the bound, then
    // the pending record beyond it, where a bounded read never reaches it.
    let mut big = String::new();
    while (big.len() as u64) <= asher_bridge::recovery::EMERGENCY_MAX_BYTES {
        big.push_str(lines[0]);
        big.push('\n');
    }
    big.push_str(lines[1]);
    big.push('\n');
    fs::write(&log, &big).unwrap();
    let d = f.diagnose();
    assert_eq!(d["emergency_log"]["truncated"], true);
    assert_eq!(d["status"], "inconclusive", "{d}");
    assert!(emergency_check_not_completed(&d), "{d}");
    assert!(
        d["ledger"]["emergency_records_not_settled"].is_null(),
        "{d}"
    );
}

#[test]
fn diagnose_counts_a_torn_emergency_record_as_pending_until_a_recovery_pass_examines_it() {
    let f = F::bare();
    pending_incident(&f, "k1");
    let log = f.root.join("emergency.jsonl");
    let text = fs::read_to_string(&log).unwrap();
    fs::write(&log, &text[..text.len() / 2]).unwrap();
    let d = f.diagnose();
    assert_eq!(d["emergency_log"]["malformed"], 1);
    assert_eq!(d["status"], "degraded", "{d}");
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 1, "{d}");
    // recover examines the torn line and records it under a line-bound identity; the owner's
    // open case, not a vanished obligation, is what remains.
    assert_eq!(f.json("recover", &[])["recovery_run"]["state"], "completed");
    assert_eq!(
        f.count("SELECT count(*) FROM recovery_item WHERE subject_kind='storage_incident' AND classification='unclassifiable'"),
        1
    );
    let d = f.diagnose();
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 0, "{d}");
}

#[test]
fn diagnose_counts_an_emergency_record_without_a_string_incident_as_pending() {
    let f = F::bare();
    let id = pending_incident(&f, "k1");
    let log = f.root.join("emergency.jsonl");
    let text = fs::read_to_string(&log).unwrap();
    let text = text.replace(&format!("\"incident\":\"{id}\""), "\"incident\":12345");
    assert!(text.contains("\"incident\":12345"));
    fs::write(&log, &text).unwrap();
    let d = f.diagnose();
    assert_eq!(d["emergency_log"]["records"], 1);
    assert_eq!(d["status"], "degraded", "{d}");
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 1, "{d}");
    assert_eq!(f.json("recover", &[])["recovery_run"]["state"], "completed");
    let d = f.diagnose();
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 0, "{d}");
}

#[test]
fn diagnose_counts_emergency_records_beside_a_ledger_below_schema_7_as_pending() {
    let f = F::bare();
    pending_incident(&f, "k1");
    f.db().execute_batch(SCHEMA6_MODEL).unwrap();
    assert_eq!(f.count("SELECT version FROM schema_meta"), 6);
    let (db, wal) = f.hashes();
    let d = f.diagnose();
    assert_eq!(d["status"], "degraded", "{d}");
    assert_eq!(d["ledger"]["emergency_records_not_settled"], 1, "{d}");
    assert!(
        d["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("emergency record")),
        "{d}"
    );
    assert_eq!(f.hashes(), (db, wal), "diagnose wrote nothing");
    // The same ledger with no emergency log at all has nothing to check and stays healthy.
    let g = F::bare();
    g.db().execute_batch(SCHEMA6_MODEL).unwrap();
    assert!(!g.root.join("emergency.jsonl").exists());
    let d = g.diagnose();
    assert_eq!(d["status"], "healthy", "{d}");
}

/// An emergency record's PID is never liveness evidence. A record that carries the
/// faulting process identity is judged as an identity: one from an earlier boot (SIMULATED by a
/// recorded boot id that is not the running one) or whose PID now names a different process
/// cannot hold the ledger, even though a process with that PID is alive. A legacy record with
/// only a PID that exists stays "unknown" (conservative, as in M1).
#[test]
fn emergency_record_pid_is_never_liveness_evidence() {
    let f = F::new();
    let me = asher_bridge::process_scope::Identity::current().unwrap();
    let line = |key: &str, identity: Option<Value>| -> (String, String) {
        let incident = uuid::Uuid::new_v4().to_string();
        let mut v = serde_json::json!({"v": 1, "incident": incident, "at": 0,
                                       "pid": me.pid, "fault": synthetic_fault(key)});
        if let Some(i) = identity {
            v["identity"] = i;
        }
        (incident, v.to_string())
    };
    let mut earlier = serde_json::to_value(&me).unwrap();
    earlier["boot"] = "sim-earlier-boot".into();
    let mut reused = serde_json::to_value(&me).unwrap();
    reused["start"] = "1".into();
    let (legacy, l1) = line("never-a", None);
    let (stale, l2) = line("never-b", Some(earlier));
    let (other, l3) = line("never-c", Some(reused));
    let log = f.root.join("emergency.jsonl");
    fs::write(&log, format!("{l1}\n{l2}\n{l3}\n")).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    f.json("recover", &[]);
    let item = |id: &str| -> (String, Value) {
        let (c, o): (String, String) = f
            .db()
            .query_row(
                "SELECT classification,observation_json FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        (c, serde_json::from_str(&o).unwrap())
    };
    let (c, o) = item(&legacy);
    assert_eq!(
        (c.as_str(), &o["faulting_process"]),
        ("unresolved", &Value::from("unknown")),
        "{o}"
    );
    let (c, o) = item(&stale);
    assert_eq!(
        (c.as_str(), &o["faulting_process"]),
        ("accounted", &Value::from("earlier_boot")),
        "{o}"
    );
    let (c, o) = item(&other);
    assert_eq!(
        (c.as_str(), &o["faulting_process"]),
        ("accounted", &Value::from("absent")),
        "{o}"
    );
}

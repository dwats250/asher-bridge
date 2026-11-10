//! Task 3 public-boundary tests; fixtures and mutations are disposable.
use asher_bridge::{
    Owner, Reader,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
struct Fixture {
    _temp: TempDir,
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
            now_ms().unwrap() + 60000,
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
        Self { _temp: t, root }
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action).arg(&self.root);
        c
    }
    fn run(&self, action: &str, args: &[&str]) -> std::process::Output {
        self.cmd(action).args(args).output().unwrap()
    }
    fn json(&self, action: &str, args: &[&str]) -> Value {
        let out = self.run(action, args);
        assert!(
            out.status.success(),
            "{}: {}",
            action,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.join("ledger.db")).unwrap()
    }
    fn fixture_path(&self) -> PathBuf {
        self.db()
            .query_row("SELECT fixture_path FROM attempt WHERE id='a'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .into()
    }
    fn intent(&self) {
        self.json("symbol-intent", &["w", "a"]);
    }
    fn prepared(&self) {
        self.intent();
        self.json("reproduce", &["a"]);
        self.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
    }
    fn seal(&self) -> Value {
        self.json("seal", &["a"])
    }
    fn claim(&self, body: &Value) -> std::process::Output {
        let mut c = self
            .cmd("ingest-claim")
            .arg("a")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(body).unwrap())
            .unwrap();
        c.wait_with_output().unwrap()
    }
    fn assert_open(&self) {
        let r = Reader::open(&self.root).unwrap();
        r.assert_accounted().unwrap();
        for w in r.list().unwrap() {
            assert_eq!(w.mode, "active");
            assert!(!w.next_owner.is_empty());
            assert!(!w.next_action.is_empty());
        }
    }
}
fn claim() -> Value {
    json!({"source_epoch":"agent-epoch-1","source_seq":1,"artifact_id":null,"observed_at":now_ms().unwrap(),"coverage":"complete","gaps":[],"payload":{"claimed_success":true}})
}
#[test]
fn zero_exit_missing_evidence() {
    let f = Fixture::new();
    f.json("run-fake", &["w", "a", "complete", "1000"]);
    let out = f.claim(&claim());
    assert!(out.status.success());
    let status = f.json("task3-status", &["a"]);
    assert_eq!(status["ready"], false);
    assert_eq!(status["work_closed"], false);
    assert_eq!(
        f.db()
            .query_row(
                "SELECT count(*) FROM evidence WHERE authority='trusted_validator'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    f.assert_open();
}
#[test]
fn evidence_freshness_and_gaps() {
    let f = Fixture::new();
    f.intent();
    let mut body = claim();
    body["source_seq"] = json!(2);
    body["observed_at"] = json!(0);
    body["coverage"] = json!("partial");
    body["gaps"] = json!(["capture interval unavailable"]);
    assert!(f.claim(&body).status.success());
    let rows = f.json("evidence-status", &["a"]);
    assert_eq!(rows[0]["fresh"], false);
    assert_eq!(rows[0]["sequence_complete"], false);
    assert_eq!(rows[0]["trusted"], false);
    assert_eq!(rows[0]["coverage"], "partial");
    assert_eq!(f.json("task3-status", &["a"])["ready"], false);
    f.assert_open();
}
#[test]
fn missed_capture_scope() {
    let f = Fixture::new();
    f.prepared();
    let initial = f.fixture_path().join("initial");
    let candidate = f.fixture_path().join("candidate");
    assert!(
        fs::read_to_string(initial.join("symbols.csv"))
            .unwrap()
            .contains("BAD SYMBOL")
    );
    assert_eq!(
        fs::read_to_string(candidate.join("symbols.csv")).unwrap(),
        "symbol,scope\nFIXTURE_OK,core\n"
    );
    assert_eq!(
        fs::read(initial.join("capture.json")).unwrap(),
        fs::read(candidate.join("capture.json")).unwrap()
    );
    let artifact = f.seal();
    let result = f.json("validate", &[artifact["id"].as_str().unwrap()]);
    assert_eq!(result["symbol_validated"], true);
    assert_eq!(result["capture_restored"], false);
    let status = f.json("task3-status", &["a"]);
    assert_eq!(status["ready"], true);
    assert_eq!(status["work_closed"], false);
    let c = f.db();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM evidence WHERE kind='initial_defect'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM evidence WHERE kind='candidate_validation'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    f.assert_open();
}
#[test]
fn artifact_loss() {
    let f = Fixture::new();
    f.prepared();
    let artifact = f.seal();
    let id = artifact["id"].as_str().unwrap();
    f.json("validate", &[id]);
    fs::remove_file(PathBuf::from(artifact["path"].as_str().unwrap()).join("symbols.csv")).unwrap();
    assert_eq!(f.json("task3-status", &["a"])["ready"], false);
    assert_eq!(
        f.db()
            .query_row("SELECT state FROM artifact WHERE id=?1", [id], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "invalid"
    );
    f.assert_open();
}
#[test]
fn conflicting_evidence_replay() {
    let f = Fixture::new();
    f.intent();
    let body = claim();
    let first = f.claim(&body);
    assert!(first.status.success());
    let again = f.claim(&body);
    assert!(again.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap()["id"],
        serde_json::from_slice::<Value>(&again.stdout).unwrap()["id"]
    );
    for mutation in [
        json!({"payload":{"claimed_success":false}}),
        json!({"observed_at":0}),
        json!({"coverage":"gap","gaps":["different"]}),
    ] {
        let mut changed = body.clone();
        for (k, v) in mutation.as_object().unwrap() {
            changed[k] = v.clone();
        }
        assert!(!f.claim(&changed).status.success());
    }
    assert_eq!(
        f.db()
            .query_row("SELECT count(*) FROM evidence", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        f.db()
            .query_row(
                "SELECT count(*) FROM event WHERE kind='evidence.conflict'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    f.assert_open();
}
#[test]
fn source_role_forgery() {
    let f = Fixture::new();
    f.intent();
    for extra in [
        json!({"source":"validator:a"}),
        json!({"authority":"trusted_validator"}),
        json!({"kind":"candidate_validation"}),
    ] {
        let mut body = claim();
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        assert!(!f.claim(&body).status.success());
    }
    assert_eq!(
        f.db()
            .query_row("SELECT count(*) FROM evidence", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.db()
            .query_row(
                "SELECT count(*) FROM event WHERE kind='evidence.rejected'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    f.assert_open();
}
#[test]
fn path_manifest_rejection() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    for layout in [
        "symlink",
        "hardlink",
        "submodule",
        "special",
        "extra",
        "traversal",
    ] {
        let f = Fixture::new();
        f.prepared();
        let dir = f.fixture_path().join("candidate");
        let outside = f._temp.path().join("outside");
        fs::write(&outside, b"must not be read/copied").unwrap();
        let mut socket = None;
        match layout {
            "symlink" => {
                fs::remove_file(dir.join("symbols.csv")).unwrap();
                symlink(&outside, dir.join("symbols.csv")).unwrap();
            }
            "hardlink" => {
                fs::remove_file(dir.join("symbols.csv")).unwrap();
                fs::hard_link(&outside, dir.join("symbols.csv")).unwrap();
            }
            "submodule" => fs::write(dir.join(".gitmodules"), b"[submodule]\n").unwrap(),
            "special" => {
                fs::remove_file(dir.join("symbols.csv")).unwrap();
                socket = Some(UnixListener::bind(dir.join("symbols.csv")).unwrap());
            }
            "extra" => fs::write(dir.join("unmanifested.txt"), b"extra").unwrap(),
            "traversal" => {
                let out = f.run("seal", &["../outside"]);
                assert!(!out.status.success());
                continue;
            }
            _ => unreachable!(),
        }
        assert!(!f.run("seal", &["a"]).status.success(), "accepted {layout}");
        assert_eq!(fs::read(&outside).unwrap(), b"must not be read/copied");
        drop(socket);
        f.assert_open();
    }
}
#[cfg(feature = "fault-injection")]
#[test]
fn mutation_during_validation() {
    let f = Fixture::new();
    f.prepared();
    let artifact = f.seal();
    let b = tempfile::tempdir().unwrap();
    let mut c = f
        .cmd("validate")
        .arg(artifact["id"].as_str().unwrap())
        .env("ASHER_TEST_BARRIER", "after_validation_checks")
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while !b.path().join("after_validation_checks").exists() {
        assert!(c.try_wait().unwrap().is_none());
        if Instant::now() > end {
            c.kill().unwrap();
            c.wait().unwrap();
            panic!("barrier timeout");
        }
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(
        PathBuf::from(artifact["path"].as_str().unwrap()).join("symbols.csv"),
        b"symbol,scope\nTAMPERED,core\n",
    )
    .unwrap();
    fs::write(b.path().join("release"), b"").unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert_eq!(f.json("task3-status", &["a"])["ready"], false);
    assert_eq!(
        f.db()
            .query_row(
                "SELECT count(*) FROM evidence WHERE kind='candidate_validation'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    f.assert_open();
}
#[test]
fn unresolved_writers_block_sealing() {
    let f = Fixture::new();
    f.intent();
    assert!(!f.run("seal", &["a"]).status.success());
    let g = Fixture::new();
    g.json("run-fake", &["w", "a", "escape", "1000"]);
    assert!(!g.run("seal", &["a"]).status.success());
    f.assert_open();
    g.assert_open();
}
#[test]
fn failed_ingest_has_no_acknowledgment() {
    let f = Fixture::new();
    f.intent();
    let c = f.db();
    c.execute_batch("CREATE TRIGGER deny_evidence BEFORE INSERT ON evidence BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let out = f.claim(&claim());
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert_eq!(
        c.query_row("SELECT count(*) FROM evidence", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.assert_open();
}

// ---- Additional Task 3 boundary tests (observable bytes, kernel identities, SQL facts) ----

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn work_due(f: &Fixture) -> (i64, i64, String) {
    f.db()
        .query_row(
            "SELECT due_at,version,next_action FROM work WHERE request_key='w'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}
fn validated(f: &Fixture) -> Value {
    f.prepared();
    let artifact = f.seal();
    f.json("validate", &[artifact["id"].as_str().unwrap()]);
    assert_eq!(f.json("task3-status", &["a"])["ready"], true);
    artifact
}
/// Start a command paused at a named test barrier; returns the child and barrier dir.
#[cfg(feature = "fault-injection")]
fn paused(mut c: Command, name: &str) -> (std::process::Child, TempDir) {
    let b = tempfile::tempdir().unwrap();
    let mut child = c
        .env("ASHER_TEST_BARRIER", name)
        .env("ASHER_TEST_BARRIER_DIR", b.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
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

#[test]
fn schema_two_migration_preserves_records() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("ledger.db");
    let c = Connection::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    c.execute_batch(include_str!("../migrations/001.sql"))
        .unwrap();
    c.execute_batch(include_str!("../migrations/002.sql"))
        .unwrap();
    c.execute_batch(
        "INSERT INTO permit VALUES('p',1,'local-owner','hash','{}',1,999999,NULL);
         INSERT INTO work VALUES('old','local-owner','key','hash','{}','p','active','blocked',3,'local-owner','inspect_supervision_uncertainty',100,0);
         INSERT INTO event VALUES(1,'old','local-owner','work.accepted','old',1,'{}');
         INSERT INTO attempt VALUES('a','old','p','spec','{\"mode\":\"hold\",\"runtime_ms\":1000,\"output_limit\":16384}','/nonexistent',1,1,1,'unknown','unresolved supervision or escaped descendant; notification delivery unavailable',0,100);
         INSERT INTO supervisor_command VALUES('launch:a','a','spec',1,999999,'1','old-boot',5);
         INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','unknown',6,'{}');",
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
    assert_eq!(count(&c, "SELECT version FROM schema_meta"), 8);
    assert_eq!(
        dump(&c),
        before,
        "migration and reconciliation changed historical rows"
    );
    assert_eq!(count(&c, "SELECT count(*) FROM artifact"), 0);
    assert_eq!(count(&c, "SELECT count(*) FROM evidence"), 0);
    // Immutable protections survive.
    assert!(
        c.execute("UPDATE supervisor_command SET fingerprint='x'", [])
            .is_err()
    );
    assert!(c.execute("DELETE FROM supervisor_fact", []).is_err());
    assert!(c.execute("DELETE FROM event", []).is_err());
    assert!(c.execute("UPDATE attempt SET spec_hash='x'", []).is_err());
    assert!(c.execute("DELETE FROM work", []).is_err());
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
    drop(q);
    // Unsupported newer schema is refused without mutation.
    c.execute("UPDATE schema_meta SET version=9", []).unwrap();
    let epoch = count(&c, "SELECT epoch FROM coordinator_epoch");
    assert!(matches!(
        Owner::open(&root),
        Err(asher_bridge::Error::Schema(9))
    ));
    assert_eq!(count(&c, "SELECT epoch FROM coordinator_epoch"), epoch);
    assert_eq!(count(&c, "SELECT version FROM schema_meta"), 9);
}

#[test]
fn launch_requires_frozen_initial_reproduction() {
    let f = Fixture::new();
    f.intent();
    assert!(
        !f.run("run-fake", &["w", "a", "prepare_symbol", "1000"])
            .status
            .success()
    );
    assert_eq!(count(&f.db(), "SELECT count(*) FROM supervisor_command"), 0);
    let r = f.json("reproduce", &["a"]);
    assert_eq!(r["reproduced"], true);
    let path = PathBuf::from(r["artifact"]["path"].as_str().unwrap());
    assert_eq!(
        fs::read(path.join("symbols.csv")).unwrap(),
        b"symbol,scope\nBAD SYMBOL,core\n"
    );
    // Replay returns the same frozen artifact and evidence; no duplicate trusted rows.
    let again = f.json("reproduce", &["a"]);
    assert_eq!(again["artifact"]["id"], r["artifact"]["id"]);
    assert_eq!(again["evidence_id"], r["evidence_id"]);
    f.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
    // Reproduction after launch cannot be created anew, and a modified initial/ blocks sealing.
    fs::write(
        f.fixture_path().join("initial/symbols.csv"),
        b"symbol,scope\nFIXTURE_OK,core\n",
    )
    .unwrap();
    assert!(!f.run("seal", &["a"]).status.success());
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM artifact WHERE role='candidate'"
        ),
        0
    );
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM evidence WHERE kind='initial_defect'"
        ),
        1
    );
    f.assert_open();
}

#[test]
fn canonical_manifest_identity() {
    use sha2::{Digest, Sha256};
    let (f, g) = (Fixture::new(), Fixture::new());
    let (a, b) = (validated(&f), validated(&g));
    assert_eq!(
        a["manifest_hash"], b["manifest_hash"],
        "same bytes/modes must have one identity"
    );
    assert_ne!(a["id"], b["id"]);
    let stored: String = f
        .db()
        .query_row(
            "SELECT manifest_json FROM artifact WHERE role='candidate'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(stored.as_bytes())),
        a["manifest_hash"].as_str().unwrap()
    );
    let m: Value = serde_json::from_str(&stored).unwrap();
    let entries = m["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["capture.json", "symbols.csv"],
        "complete, sorted, nothing excluded"
    );
    let path = PathBuf::from(a["path"].as_str().unwrap());
    for e in entries {
        let bytes = fs::read(path.join(e["path"].as_str().unwrap())).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            e["sha256"].as_str().unwrap()
        );
        assert_eq!(e["size"], bytes.len());
        assert_eq!(e["mode"], 0o600);
        assert_eq!(
            bytes,
            fs::read(
                f.fixture_path()
                    .join("candidate")
                    .join(e["path"].as_str().unwrap())
            )
            .unwrap()
        );
    }
    // Snapshot is outside the candidate tree and excludes supervisor capture.
    assert!(!path.starts_with(f.fixture_path()));
    assert!(!path.join("captured.log").exists());
    f.assert_open();
}

#[test]
fn snapshot_replacement_and_mode_changes_invalidate() {
    use std::os::unix::fs::PermissionsExt;
    for change in ["chmod", "replace_dir", "rewrite_same_bytes", "relink"] {
        let f = Fixture::new();
        let artifact = validated(&f);
        let path = PathBuf::from(artifact["path"].as_str().unwrap());
        let file = path.join("symbols.csv");
        let bytes = fs::read(&file).unwrap();
        match change {
            "chmod" => fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap(),
            "replace_dir" => {
                let copy = path.with_extension("copy");
                fs::create_dir(&copy).unwrap();
                fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).unwrap();
                for n in ["symbols.csv", "capture.json"] {
                    fs::copy(path.join(n), copy.join(n)).unwrap();
                }
                fs::remove_dir_all(&path).unwrap();
                fs::rename(&copy, &path).unwrap();
            }
            "rewrite_same_bytes" => {
                thread::sleep(Duration::from_millis(20));
                fs::write(&file, &bytes).unwrap();
            }
            "relink" => {
                let link = f._temp.path().join("link");
                fs::hard_link(&file, &link).unwrap();
                fs::remove_file(&link).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            fs::read(&file).unwrap(),
            bytes,
            "{change}: content restored"
        );
        let status = f.json("task3-status", &["a"]);
        assert_eq!(status["ready"], false, "{change}");
        let state: String = f
            .db()
            .query_row(
                "SELECT state FROM artifact WHERE role='candidate'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "invalid", "{change}");
        // Invalidation is durable and one-way, and evidence on it is no longer trusted.
        let rows = f.json("evidence-status", &["a"]);
        assert!(
            rows.as_array()
                .unwrap()
                .iter()
                .filter(|r| r["kind"] == "candidate_validation")
                .all(|r| r["trusted"] == false)
        );
        assert!(
            f.db()
                .execute("UPDATE artifact SET state='valid',invalid_reason=NULL", [])
                .is_err()
        );
        assert!(
            !f.run("validate", &[artifact["id"].as_str().unwrap()])
                .status
                .success()
        );
        f.assert_open();
    }
}

#[test]
fn capture_tamper_is_not_restoration() {
    let f = Fixture::new();
    f.prepared();
    // Same-user edit after cleanup claims complete capture. Mode preserved.
    fs::write(
        f.fixture_path().join("candidate/capture.json"),
        br#"{"version":1,"interval":{"from":"a","to":"b"},"complete":true,"missing":[]}"#,
    )
    .unwrap();
    let artifact = f.seal();
    let r = f.json("validate", &[artifact["id"].as_str().unwrap()]);
    assert_eq!(r["symbol_validated"], true);
    assert_eq!(r["capture_restored"], false);
    assert_eq!(r["capture_unchanged"], false);
    let status = f.json("task3-status", &["a"]);
    assert_eq!(status["ready"], false);
    assert_eq!(status["capture_restored"], false);
    f.assert_open();
}

#[test]
fn failed_candidate_is_recorded_not_ready() {
    let f = Fixture::new();
    f.prepared();
    fs::write(
        f.fixture_path().join("candidate/symbols.csv"),
        b"symbol,scope\nBAD SYMBOL,core\n",
    )
    .unwrap();
    let artifact = f.seal();
    let r = f.json("validate", &[artifact["id"].as_str().unwrap()]);
    assert_eq!(r["symbol_validated"], false);
    assert_eq!(f.json("task3-status", &["a"])["ready"], false);
    assert_eq!(work_due(&f).2, "inspect_failed_candidate_validation");
    // Replay does not create a second result or a passing one.
    let again = f.json("validate", &[artifact["id"].as_str().unwrap()]);
    assert_eq!(again["symbol_validated"], false);
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM evidence WHERE kind='candidate_validation'"
        ),
        1
    );
    f.assert_open();
}

#[test]
fn reopen_and_status_preserve_deadlines() {
    let f = Fixture::new();
    validated(&f);
    let before = work_due(&f);
    assert_eq!(before.2, "create_exact_review");
    for _ in 0..3 {
        f.json("task3-status", &["a"]);
        f.json("evidence-status", &["a"]);
        f.run("reconcile", &[]);
        f.json("run-fake", &["w", "a", "prepare_symbol", "1000"]);
    }
    assert_eq!(
        work_due(&f),
        before,
        "Task 2 projection or status postponed/overwrote Task 3 responsibility"
    );
    // Repeated refusals do not postpone a rejection deadline either.
    let g = Fixture::new();
    g.prepared();
    fs::write(g.fixture_path().join("candidate/extra"), b"x").unwrap();
    assert!(!g.run("seal", &["a"]).status.success());
    let first = work_due(&g);
    assert_eq!(first.2, "inspect_candidate_layout");
    fs::remove_file(g.fixture_path().join("candidate/extra")).unwrap();
    for _ in 0..2 {
        assert!(
            !g.run("seal", &["a"]).status.success(),
            "rejection must be terminal"
        );
    }
    assert_eq!(work_due(&g), first);
    assert_eq!(
        count(
            &g.db(),
            "SELECT count(*) FROM artifact WHERE role='candidate'"
        ),
        0
    );
    f.assert_open();
    g.assert_open();
}

#[test]
fn wrong_association_and_claims_never_trusted() {
    let f = Fixture::new();
    f.prepared();
    let own = f.seal();
    // A second work/attempt owns a different artifact.
    let mut o = Owner::open(&f.root).unwrap();
    o.submit(&SubmitWork {
        protocol_version: 1,
        request_key: "w2".into(),
        fixture_id: "fixture:symbol".into(),
        success_criteria: "second".into(),
        source_evidence: vec!["fixture:m".into()],
        permit_id: "p".into(),
    })
    .unwrap();
    o.symbol_intent("w2", "b").unwrap();
    let other = o.reproduce("b").unwrap();
    drop(o);
    let before = count(
        &f.db(),
        "SELECT count(*) FROM event WHERE kind='evidence.rejected'",
    );
    for artifact in [
        other["artifact"]["id"].clone(),
        json!("missing-artifact"),
        json!("../x"),
    ] {
        let mut body = claim();
        body["artifact_id"] = artifact;
        assert!(!f.claim(&body).status.success());
    }
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM event WHERE kind='evidence.rejected'"
        ),
        before + 3
    );
    // Own association accepted but untrusted; reordered fields replay to the same row.
    let body = format!(
        r#"{{"payload":{{"claimed_success":true}},"gaps":[],"coverage":"complete","observed_at":{},"artifact_id":"{}","source_seq":1,"source_epoch":"e"}}"#,
        now_ms().unwrap(),
        own["id"].as_str().unwrap()
    );
    let send = |b: &str| {
        let mut c = f
            .cmd("ingest-claim")
            .arg("a")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(b.as_bytes()).unwrap();
        c.wait_with_output().unwrap()
    };
    let one: Value = serde_json::from_slice(&send(&body).stdout).unwrap();
    let mut v: Value = serde_json::from_str(&body).unwrap();
    v["payload"] = json!({"claimed_success":true});
    let two: Value = serde_json::from_slice(&f.claim(&v).stdout).unwrap();
    assert_eq!(one["id"], two["id"]);
    assert_eq!(two["disposition"], "existing");
    assert_eq!(one["trusted"], false);
    let rows = f.json("evidence-status", &["a"]);
    let claim_row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "agent_claim")
        .unwrap();
    assert_eq!(claim_row["trusted"], false);
    assert_eq!(claim_row["authority"], "agent_claim");
    assert_eq!(claim_row["source"], "agent:a");
    // Oversize and non-object bodies are audited rejections without rows.
    let n = count(&f.db(), "SELECT count(*) FROM evidence");
    assert!(!send(&"x".repeat(70000)).status.success());
    assert!(!send("[1]").status.success());
    // Duplicate keys are refused, not resolved last-one-wins.
    assert!(
        !send(&body.replacen("\"source_seq\":1", "\"source_seq\":1,\"source_seq\":2", 1))
            .status
            .success()
    );
    assert_eq!(count(&f.db(), "SELECT count(*) FROM evidence"), n);
    // Trusted evidence cannot be bound to the wrong artifact role even by direct SQL.
    assert!(f.db().execute(
        "INSERT INTO evidence SELECT 'forged',work_id,attempt_id,artifact_id,authority,source,source_epoch,99,'candidate_validation',observed_at,received_at,coverage,gaps_json,payload_hash,payload_json,envelope_hash,coordinator_epoch FROM evidence WHERE kind='initial_defect' AND attempt_id='a'",
        []
    ).is_err());
    f.assert_open();
}

#[test]
fn stale_or_conflicting_evidence_blocks_readiness() {
    let f = Fixture::new();
    validated(&f);
    // A later conflicting replay on this attempt degrades readiness until inspection.
    assert!(f.claim(&claim()).status.success());
    let mut changed = claim();
    changed["payload"] = json!({"claimed_success":false});
    assert!(!f.claim(&changed).status.success());
    let s = f.json("task3-status", &["a"]);
    assert_eq!(s["ready"], false);
    assert!(s["blockers"].to_string().contains("conflict"));
    // Stale trusted evidence (simulated by direct corruption of the disposable ledger).
    let g = Fixture::new();
    validated(&g);
    let c = g.db();
    c.execute_batch("DROP TRIGGER evidence_no_update; UPDATE evidence SET observed_at=observed_at-7200000 WHERE kind='candidate_validation';").unwrap();
    let s = g.json("task3-status", &["a"]);
    assert_eq!(s["ready"], false);
    assert!(s["blockers"].to_string().contains("stale"));
    f.assert_open();
    g.assert_open();
}

#[cfg(feature = "fault-injection")]
#[test]
fn claim_crash_and_response_loss_boundaries() {
    let f = Fixture::new();
    f.intent();
    let body = serde_json::to_vec(&claim()).unwrap();
    // The body is read before ownership is taken, so it is written before the barrier.
    let spawn = |name: &str| {
        let b = tempfile::tempdir().unwrap();
        let mut child = f
            .cmd("ingest-claim")
            .arg("a")
            .env("ASHER_TEST_BARRIER", name)
            .env("ASHER_TEST_BARRIER_DIR", b.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&body).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while !b.path().join(name).exists() {
            assert!(
                Instant::now() < end && child.try_wait().unwrap().is_none(),
                "barrier {name}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.stdout.is_empty(), "no ACK may escape a killed ingest");
    };
    spawn("before_evidence_commit");
    assert_eq!(count(&f.db(), "SELECT count(*) FROM evidence"), 0);
    spawn("after_evidence_commit");
    assert_eq!(count(&f.db(), "SELECT count(*) FROM evidence"), 1);
    let id: String = f
        .db()
        .query_row("SELECT id FROM evidence", [], |r| r.get(0))
        .unwrap();
    let replay = f.claim(&serde_json::from_slice(&body).unwrap());
    assert!(replay.status.success());
    let v: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(v["id"], id.as_str());
    assert_eq!(v["disposition"], "existing");
    assert_eq!(count(&f.db(), "SELECT count(*) FROM evidence"), 1);
    f.assert_open();
}

#[cfg(feature = "fault-injection")]
#[test]
fn seal_crash_leaves_harmless_orphan_and_response_loss_replays() {
    let f = Fixture::new();
    f.prepared();
    let seal = || {
        let mut c = f.cmd("seal");
        c.arg("a");
        c
    };
    let (mut child, _b) = paused(seal(), "before_seal_commit");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM artifact WHERE role='candidate'"
        ),
        0
    );
    let store = f.root.join("artifacts");
    let on_disk = fs::read_dir(&store).unwrap().count();
    assert_eq!(
        on_disk, 2,
        "initial + unreferenced orphan candidate snapshot"
    );
    let (mut child, _b2) = paused(seal(), "after_seal_commit");
    child.kill().unwrap();
    child.wait().unwrap();
    let id: String = f
        .db()
        .query_row("SELECT id FROM artifact WHERE role='candidate'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let replay = f.seal();
    assert_eq!(replay["id"], id.as_str());
    assert_eq!(replay["existing"], true);
    f.json("validate", &[&id]);
    assert_eq!(
        f.json("task3-status", &["a"])["ready"],
        true,
        "orphan must be harmless"
    );
    f.assert_open();
}

#[cfg(feature = "fault-injection")]
#[test]
fn live_writers_block_sealing() {
    use asher_bridge::process_scope::Identity;
    // 1. A coordinator holding the ledger mid-run excludes sealing entirely.
    let f = Fixture::new();
    f.intent();
    f.json("reproduce", &["a"]);
    let mut run = f.cmd("run-fake");
    run.args(["w", "a", "prepare_symbol", "1000"]);
    let (child, b) = paused(run, "before_release");
    let out = f.run("seal", &["a"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("coordinator already owned"));
    // 2. Kill the supervisor before release: wrapper must die; lifecycle unknown; no seal.
    let (pid, start): (i32, String) = f
        .db()
        .query_row(
            "SELECT supervisor_pid,supervisor_start FROM supervisor_command",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let wrapper: Identity =
        serde_json::from_slice(&fs::read(b.path().join("wrapper_identity.json")).unwrap()).unwrap();
    let sup = Identity::read(pid).unwrap();
    assert_eq!(sup.start, start);
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = child.wait_with_output();
    let end = Instant::now() + Duration::from_secs(5);
    while wrapper.live() {
        assert!(Instant::now() < end, "wrapper survived supervisor loss");
        thread::sleep(Duration::from_millis(5));
    }
    f.run("reconcile", &[]);
    assert!(!f.run("seal", &["a"]).status.success());
    assert_eq!(
        count(
            &f.db(),
            "SELECT count(*) FROM artifact WHERE role='candidate'"
        ),
        0
    );
    f.assert_open();
    // 3. A known identity recorded for the attempt that is still live blocks sealing even
    //    when the latest durable fact claims cleanup; it unblocks only once the kernel
    //    process is actually gone.
    let g = Fixture::new();
    g.prepared();
    let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let ident = Identity::read(sleeper.id() as i32).unwrap();
    let c = g.db();
    c.execute("INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','descendant_observed',1,?1)", [json!({"identity":ident,"escaped":false}).to_string()]).unwrap();
    // The later cleanup claim repeats the supervisor's own record: one that named a different
    // capture (or none) would itself be an evidence gap, which is not what this case is about.
    c.execute("INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) SELECT 'a','cleanup_completed',2,payload_json FROM supervisor_fact WHERE attempt_id='a' AND kind='cleanup_completed' ORDER BY seq LIMIT 1", []).unwrap();
    let out = g.run("seal", &["a"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("may still be live"));
    assert_eq!(
        count(&c, "SELECT count(*) FROM artifact WHERE role='candidate'"),
        0
    );
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert!(!ident.live());
    g.seal();
    g.assert_open();
}

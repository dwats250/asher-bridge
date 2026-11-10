use asher_bridge::{
    Error, Owner, Reader,
    model::{Disposition, Receipt},
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn request() -> SubmitWork {
    SubmitWork {
        protocol_version: 1,
        request_key: "check-symbol-1".into(),
        fixture_id: "fixture:synthetic".into(),
        success_criteria: "Record fixture investigation; no execution in Task 1".into(),
        source_evidence: vec!["fixture:initial-failure".into()],
        permit_id: "permit-1".into(),
    }
}
fn fixture() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let mut owner = Owner::initialize(&root).unwrap();
    owner
        .add_permit(
            "permit-1",
            &PermitScope {
                fixture_id: "fixture:synthetic".into(),
                action: "prepare_fixture".into(),
                max_attempts: 1,
                max_runtime_ms: 60000,
                network: false,
            },
            now_ms().unwrap() + 3600000,
        )
        .unwrap();
    drop(owner);
    (temp, root)
}
fn sql(root: &Path) -> Connection {
    let c = Connection::open(root.join("ledger.db")).unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    c
}
fn run_submit(root: &Path, req: &SubmitWork) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .args(["submit", root.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(req).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn durable_acceptance_schema_exists() {
    let (_t, root) = fixture();
    let c = sql(&root);
    let count: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='work'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
#[test]
fn accepted_work_and_event_survive_real_process_restart() {
    let (_t, root) = fixture();
    let req = request();
    let first = run_submit(&root, &req);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let receipt: Receipt = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(receipt.disposition, Disposition::Accepted);
    let second = run_submit(&root, &req);
    assert!(second.status.success());
    let replay: Receipt = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(replay.disposition, Disposition::Existing);
    assert_eq!(receipt.work, replay.work);
    let c = sql(&root);
    assert_eq!(
        c.query_row("SELECT count(*) FROM work", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM event WHERE kind='work.accepted'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    Reader::open(&root).unwrap().assert_accounted().unwrap();
}
#[test]
fn conflicting_payload_preserves_original_and_records_conflict() {
    let (_t, root) = fixture();
    let mut o = Owner::open(&root).unwrap();
    let mut req = request();
    let first = o.submit(&req).unwrap();
    req.success_criteria = "changed".into();
    assert!(matches!(o.submit(&req), Err(Error::Conflict)));
    assert_eq!(
        Reader::open(&root)
            .unwrap()
            .get(&req.request_key)
            .unwrap()
            .unwrap(),
        first.work
    );
    assert_eq!(
        sql(&root)
            .query_row(
                "SELECT count(*) FROM event WHERE kind='request.conflict'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
#[test]
fn equivalent_json_order_replays_same_identity() {
    let (_t, root) = fixture();
    let req = request();
    let first = run_submit(&root, &req);
    assert!(first.status.success());
    let json = format!(
        r#"{{"permit_id":"permit-1","source_evidence":["fixture:initial-failure"],"success_criteria":{},"fixture_id":"fixture:synthetic","request_key":"check-symbol-1","protocol_version":1}}"#,
        serde_json::to_string(&req.success_criteria).unwrap()
    );
    let parsed: SubmitWork = serde_json::from_str(&json).unwrap();
    let replay = run_submit(&root, &parsed);
    assert!(replay.status.success());
    assert_eq!(
        serde_json::from_slice::<Receipt>(&first.stdout)
            .unwrap()
            .work
            .id,
        serde_json::from_slice::<Receipt>(&replay.stdout)
            .unwrap()
            .work
            .id
    );
}
#[test]
fn permit_checks_and_replay_after_revocation() {
    let (_t, root) = fixture();
    let mut o = Owner::open(&root).unwrap();
    let mut req = request();
    req.fixture_id = "fixture:other".into();
    assert!(matches!(o.submit(&req), Err(Error::Permit)));
    req = request();
    let receipt = o.submit(&req).unwrap();
    o.revoke_permit("permit-1").unwrap();
    assert_eq!(o.submit(&req).unwrap().work, receipt.work);
    req.request_key = "new-work".into();
    assert!(matches!(o.submit(&req), Err(Error::Permit)));
}
#[test]
fn owner_lock_excludes_second_process_but_allows_readers() {
    let (_t, root) = fixture();
    let _o = Owner::open(&root).unwrap();
    assert!(matches!(Owner::open(&root), Err(Error::Owned)));
    let result = run_submit(&root, &request());
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("already owned"));
    assert!(Reader::open(&root).is_ok());
}
#[test]
fn unknown_schema_and_corruption_never_reinitialize() {
    let (_t, root) = fixture();
    sql(&root)
        .execute("UPDATE schema_meta SET version=99", [])
        .unwrap();
    assert!(matches!(Owner::open(&root), Err(Error::Schema(99))));
    assert_eq!(
        sql(&root)
            .query_row("SELECT version FROM schema_meta", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        99
    );
    let (_t2, root2) = fixture();
    let db = root2.join("ledger.db");
    fs::write(&db, b"invalid SQLite contents").unwrap();
    let before = fs::read(&db).unwrap();
    assert!(Owner::open(&root2).is_err());
    assert_eq!(fs::read(&db).unwrap(), before);
    assert!(Owner::initialize(&root2).is_err());
}
#[test]
fn immutable_history_and_invalid_accounting_rejected() {
    let (_t, root) = fixture();
    Owner::open(&root).unwrap().submit(&request()).unwrap();
    let c = sql(&root);
    for statement in [
        "UPDATE work SET mode='closed'",
        "UPDATE work SET phase='done'",
        "UPDATE work SET next_owner=''",
        "UPDATE work SET next_action=''",
        "UPDATE work SET due_at=NULL",
        "UPDATE work SET request_hash='replaced'",
        "DELETE FROM work",
        "DELETE FROM event",
        "UPDATE event SET actor='forged'",
    ] {
        assert!(
            c.execute(statement, []).is_err(),
            "accepted invalid mutation: {statement}"
        );
    }
    Reader::open(&root).unwrap().assert_accounted().unwrap();
}
#[test]
fn missing_or_foreign_permit_and_unknown_fields_rejected() {
    let (_t, root) = fixture();
    let mut req = request();
    req.permit_id = "absent".into();
    assert!(matches!(
        Owner::open(&root).unwrap().submit(&req),
        Err(Error::Permit)
    ));
    let mut v = serde_json::to_value(request()).unwrap();
    v["shell"] = serde_json::json!("arbitrary command");
    assert!(serde_json::from_value::<SubmitWork>(v).is_err());
    req = request();
    req.protocol_version = 2;
    assert!(Owner::open(&root).unwrap().submit(&req).is_err());
    let c = sql(&root);
    assert_eq!(
        c.query_row("SELECT count(*) FROM work", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn unsafe_paths_refused_and_no_missing_ledger_creation() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (t, root) = fixture();
    let alias = t.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(Owner::open(&alias).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Owner::open(&root).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(root.join("ledger.db")).unwrap();
    assert!(Owner::open(&root).is_err());
    assert!(!root.join("ledger.db").exists());
}
#[test]
fn due_work_is_explicitly_visible_as_overdue() {
    let (_t, root) = fixture();
    let receipt = Owner::open(&root).unwrap().submit(&request()).unwrap();
    assert!(receipt.work.overdue(receipt.work.due_at + 1));
    assert!(!receipt.work.overdue(receipt.work.accepted_at));
}
#[test]
fn foreign_key_enforcement_on_acceptance_connection() {
    let (_t, root) = fixture();
    let c = sql(&root);
    assert!(c.execute("INSERT INTO work VALUES('id','local-owner','key','hash','{}','absent','active','queued',1,'coordinator','prepare_fixture',100,0)",[]).is_err());
}

#[cfg(feature = "fault-injection")]
fn kill_at(root: &Path, barrier: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .args(["submit", root.to_str().unwrap()])
        .env("ASHER_TEST_BARRIER", barrier)
        .env("ASHER_TEST_BARRIER_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request()).unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join(barrier).exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before {barrier}: {status}");
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("barrier timeout {barrier}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "accepted receipt escaped before barrier"
    );
}
#[cfg(feature = "fault-injection")]
#[test]
fn acceptance_crash_boundaries() {
    for barrier in ["before_accept_commit", "after_accept_commit"] {
        let (_t, root) = fixture();
        kill_at(&root, barrier);
        let r = Reader::open(&root).unwrap();
        let existing = r.get("check-symbol-1").unwrap();
        assert_eq!(existing.is_some(), barrier == "after_accept_commit");
        drop(r);
        let retry = run_submit(&root, &request());
        assert!(
            retry.status.success(),
            "{}",
            String::from_utf8_lossy(&retry.stderr)
        );
        let receipt: Receipt = serde_json::from_slice(&retry.stdout).unwrap();
        assert_eq!(
            receipt.disposition,
            if barrier == "after_accept_commit" {
                Disposition::Existing
            } else {
                Disposition::Accepted
            }
        );
        if let Some(original) = existing {
            assert_eq!(original, receipt.work);
        }
        assert_eq!(
            sql(&root)
                .query_row("SELECT count(*) FROM work", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            sql(&root)
                .query_row(
                    "SELECT count(*) FROM event WHERE kind='work.accepted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        Reader::open(&root).unwrap().assert_accounted().unwrap();
    }
}
#[test]
fn sql_failure_before_acceptance_leaves_no_false_receipt() {
    let (_t, root) = fixture();
    let c = sql(&root);
    c.execute_batch("CREATE TRIGGER deny_accept BEFORE INSERT ON event WHEN NEW.kind='work.accepted' BEGIN SELECT RAISE(ABORT,'injected event failure'); END;").unwrap();
    let result = run_submit(&root, &request());
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        c.query_row("SELECT count(*) FROM work", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    c.execute_batch("DROP TRIGGER deny_accept").unwrap();
    assert!(run_submit(&root, &request()).status.success());
}
#[test]
fn sqlite_busy_has_bounded_failure_and_no_acceptance() {
    let (_t, root) = fixture();
    let c = sql(&root);
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let before = Instant::now();
    let result = run_submit(&root, &request());
    assert!(!result.status.success());
    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(result.stdout.is_empty());
    c.execute_batch("ROLLBACK").unwrap();
    assert!(Reader::open(&root).unwrap().list().unwrap().is_empty());
}

#[test]
fn initialize_rejects_symlinked_parent_before_writing() {
    use std::os::unix::fs::symlink;
    let t = tempfile::tempdir().unwrap();
    let actual = t.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let alias = t.path().join("alias");
    symlink(&actual, &alias).unwrap();
    assert!(Owner::initialize(&alias.join("state")).is_err());
    assert!(!actual.join("state").exists());
}
#[test]
fn expired_permit_not_accepted_after_waiting_for_write_lock() {
    let (_t, root) = fixture();
    let mut owner = Owner::open(&root).unwrap();
    let expiry = now_ms().unwrap() + 300;
    owner
        .add_permit(
            "short",
            &PermitScope {
                fixture_id: "fixture:synthetic".into(),
                action: "prepare_fixture".into(),
                max_attempts: 1,
                max_runtime_ms: 60000,
                network: false,
            },
            expiry,
        )
        .unwrap();
    let c = sql(&root);
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut req = request();
    req.permit_id = "short".into();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        send.send(()).unwrap();
        owner.submit(&req)
    });
    receive.recv_timeout(Duration::from_secs(1)).unwrap();
    while now_ms().unwrap() <= expiry {
        thread::sleep(Duration::from_millis(2));
    }
    c.execute_batch("ROLLBACK").unwrap();
    assert!(matches!(worker.join().unwrap(), Err(Error::Permit)));
    assert!(Reader::open(&root).unwrap().list().unwrap().is_empty());
}

#[test]
fn owner_drop_releases_lock_even_during_another_childs_pre_exec() {
    use std::{
        io::Read,
        os::unix::{net::UnixStream, process::CommandExt},
    };
    let (_t, root) = fixture();
    let owner = Owner::open(&root).unwrap();
    let (mut parent, mut child_stream) = UnixStream::pair().unwrap();
    parent
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    child_stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let worker = thread::spawn(move || {
        let mut cmd = Command::new("/bin/true");
        // Only Unix socket I/O in the post-fork hook, no allocation or shared locks.
        unsafe {
            cmd.pre_exec(move || {
                child_stream.write_all(&[1])?;
                let mut release = [0];
                child_stream.read_exact(&mut release)?;
                Ok(())
            });
        }
        cmd.spawn().unwrap().wait().unwrap()
    });
    let mut ready = [0];
    parent.read_exact(&mut ready).unwrap();
    drop(owner);
    let reopened = Owner::open(&root);
    parent.write_all(&[1]).unwrap();
    assert!(worker.join().unwrap().success());
    assert!(
        reopened.is_ok(),
        "temporary fork inheritance must not retain completed ownership"
    );
}

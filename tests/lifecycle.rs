//! Task 2 contract tests. Faults are synchronized by named markers, never timed races.
use asher_bridge::{
    Owner, Reader,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
// Independent kernel oracle: deliberately does not call the product liveness helper.
fn running_identity(i: &asher_bridge::process_scope::Identity) -> bool {
    let stat = match fs::read_to_string(format!("/proc/{}/stat", i.pid)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => panic!("cannot establish process absence: {e}"),
    };
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    fields[19] == i.start && boot.trim() == i.boot && fields[0] != "Z" && fields[0] != "X"
}
struct Fixture {
    _tmp: TempDir,
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
                fixture_id: "fixture:test".into(),
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
            fixture_id: "fixture:test".into(),
            success_criteria: "fake lifecycle only".into(),
            source_evidence: vec!["fixture:synthetic".into()],
            permit_id: "p".into(),
        })
        .unwrap();
        Self { _tmp: t, root }
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action).arg(&self.root);
        c
    }
    fn run(&self, action: &str, args: &[&str]) -> std::process::Output {
        self.cmd(action).args(args).output().unwrap()
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.join("ledger.db")).unwrap()
    }
    fn state(&self) -> String {
        self.db()
            .query_row("SELECT state FROM attempt WHERE id='a'", [], |r| r.get(0))
            .unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn facts(&self) -> Vec<String> {
        self.db()
            .prepare("SELECT kind FROM supervisor_fact ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn check(&self) {
        Reader::open(&self.root)
            .unwrap()
            .assert_accounted()
            .unwrap();
        for i in self.identities() {
            assert!(
                !running_identity(&i),
                "test-owned identity remains live after cleanup: {i:?}"
            );
        }
    }
    fn identities(&self) -> Vec<asher_bridge::process_scope::Identity> {
        let db = self.db();
        let Ok(mut q) = db.prepare("SELECT payload_json FROM supervisor_fact") else {
            return vec![];
        };
        q.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| {
                let v: serde_json::Value = serde_json::from_str(&r.ok()?).ok()?;
                serde_json::from_value(v.get("identity")?.clone()).ok()
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for i in self.identities() {
            i.signal(libc::SIGKILL);
        }
        let db = self.db();
        if let Ok(mut q) = db.prepare(
            "SELECT supervisor_pid,supervisor_start,supervisor_boot FROM supervisor_command",
        ) {
            let rows = q.query_map([], |r| {
                Ok((
                    r.get::<_, i32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            });
            if let Ok(rows) = rows {
                for (pid, start, boot) in rows.flatten() {
                    if let Ok(i) = asher_bridge::process_scope::Identity::read(pid) {
                        if i.start == start && i.boot == boot {
                            i.signal(libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
}
fn ok(o: std::process::Output) {
    assert!(
        o.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
#[cfg(feature = "fault-injection")]
fn paused(f: &Fixture, barrier: &str, mode: &str) -> (Child, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = f
        .cmd("run-fake")
        .args(["w", "a", mode, "2000"])
        .env("ASHER_TEST_BARRIER", barrier)
        .env("ASHER_TEST_BARRIER_DIR", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !dir.path().join(barrier).exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "exited before barrier {barrier}"
        );
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("barrier timeout {barrier}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    (child, dir)
}
fn wait(child: &mut Child) {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("bounded lifecycle timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}
#[cfg(feature = "fault-injection")]
fn supervisor_pid(f: &Fixture) -> i32 {
    f.db()
        .query_row(
            "SELECT supervisor_pid FROM supervisor_command LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
#[cfg(feature = "fault-injection")]
fn kill_supervisor(f: &Fixture) {
    let pid = supervisor_pid(f);
    // PID belongs to this test's live supervisor, verified against its stored start identity.
    let start: String = f
        .db()
        .query_row(
            "SELECT supervisor_start FROM supervisor_command LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(proc_start(pid), Some(start));
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGKILL), 0);
    }
}
#[cfg(feature = "fault-injection")]
fn proc_start(pid: i32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(s.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
}
#[test]
fn duplicate_commands() {
    let f = Fixture::new();
    ok(f.run("run-fake", &["w", "a", "complete", "1000"]));
    assert_eq!(f.state(), "cleaned");
    ok(f.run("run-fake", &["w", "a", "complete", "1000"]));
    assert!(
        !f.run("run-fake", &["w", "a", "hold", "1000"])
            .status
            .success()
    );
    assert!(
        !f.run("run-fake", &["w", "b", "complete", "1000"])
            .status
            .success()
    );
    assert_eq!(f.count("attempt"), 1);
    assert_eq!(f.count("supervisor_command"), 1);
    assert_eq!(
        f.facts().iter().filter(|s| s.as_str() == "started").count(),
        1
    );
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn launch_intent_replay() {
    let f = Fixture::new();
    let (mut c, _b) = paused(&f, "after_launch_intent", "complete");
    c.kill().unwrap();
    c.wait().unwrap();
    assert_eq!(f.state(), "intent");
    assert_eq!(f.count("supervisor_command"), 0);
    ok(f.run("run-fake", &["w", "a", "complete", "2000"]));
    assert_eq!(f.state(), "cleaned");
    assert_eq!(f.count("attempt"), 1);
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn child_before_ack() {
    for b in ["after_wrapper_create", "after_release"] {
        let f = Fixture::new();
        let (mut c, barrier) = paused(&f, b, "hold");
        let identity: asher_bridge::process_scope::Identity = serde_json::from_slice(
            &fs::read(barrier.path().join("wrapper_identity.json")).unwrap(),
        )
        .unwrap();
        kill_supervisor(&f);
        wait(&mut c);
        assert!(
            !running_identity(&identity),
            "blocked/released wrapper survived supervisor death"
        );
        ok(f.run("reconcile", &[]));
        assert_eq!(f.state(), "unknown");
        ok(f.run("run-fake", &["w", "a", "hold", "2000"]));
        assert_eq!(f.count("supervisor_command"), 1);
        assert_eq!(f.state(), "unknown");
        assert!(
            !f.run("run-fake", &["w", "b", "complete", "1000"])
                .status
                .success()
        );
        f.check();
    }
}
#[cfg(feature = "fault-injection")]
#[test]
fn partial_start_cleanup() {
    for point in ["reader_setup", "writer_setup", "worker_setup"] {
        let f = Fixture::new();
        let o = f
            .cmd("run-fake")
            .args(["w", "a", "complete", "1000"])
            .env("ASHER_TEST_FAIL", point)
            .output()
            .unwrap();
        ok(o);
        assert_eq!(f.state(), "cleaned");
        let facts = f.facts();
        assert!(!facts.contains(&"started".into()));
        assert!(facts.contains(&"stop_confirmed".into()));
        assert!(facts.contains(&"cleanup_completed".into()));
        f.check();
    }
}
#[cfg(feature = "fault-injection")]
#[test]
fn coordinator_restart() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "coordinator_observing", "hold");
    c.kill().unwrap();
    c.wait().unwrap();
    fs::write(b.path().join("release"), b"").unwrap();
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        ok(f.run("reconcile", &[]));
        if f.state() == "cleaned" {
            break;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.count("attempt"), 1);
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn supervisor_restart() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "coordinator_observing", "hold");
    kill_supervisor(&f);
    fs::write(b.path().join("release"), b"").unwrap();
    wait(&mut c);
    ok(f.run("run-fake", &["w", "a", "hold", "2000"]));
    assert_eq!(f.state(), "unknown");
    assert_eq!(f.count("supervisor_command"), 1);
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn both_roles_restart() {
    let f = Fixture::new();
    let (mut c, _b) = paused(&f, "coordinator_observing", "hold");
    kill_supervisor(&f);
    c.kill().unwrap();
    c.wait().unwrap();
    ok(f.run("reconcile", &[]));
    ok(f.run("run-fake", &["w", "a", "hold", "2000"]));
    assert_eq!(f.state(), "unknown");
    assert_eq!(f.count("supervisor_command"), 1);
    f.check();
}
#[test]
fn parent_exit_descendants() {
    let f = Fixture::new();
    ok(f.run("run-fake", &["w", "a", "descendant", "1000"]));
    assert_eq!(f.state(), "cleaned");
    let v = f.facts();
    assert!(
        v.iter().position(|s| s == "parent_exit_observed").unwrap()
            < v.iter().position(|s| s == "stop_confirmed").unwrap()
    );
    f.check();
}
#[test]
fn escaped_descendant() {
    let f = Fixture::new();
    ok(f.run("run-fake", &["w", "a", "escape", "1000"]));
    assert_eq!(f.state(), "unknown");
    assert!(!f.facts().contains(&"cleanup_completed".into()));
    let raw: String = f
        .db()
        .query_row(
            "SELECT payload_json FROM supervisor_fact WHERE kind='descendant_observed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()["escaped"],
        true
    );
    let out = f.run("attempts", &[]);
    ok(f.run("check", &[]));
    assert!(String::from_utf8_lossy(&out.stdout).contains("unknown"));
    assert!(
        !f.run("run-fake", &["w", "b", "complete", "1000"])
            .status
            .success()
    );
    f.check();
}
#[test]
fn ignored_interrupt() {
    let f = Fixture::new();
    ok(f.run("run-fake", &["w", "a", "ignore", "100"]));
    assert_eq!(f.state(), "cleaned");
    assert!(f.facts().contains(&"force_stop_sent".into()));
    f.check();
}

#[test]
fn version_one_migration_preserves_records() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("state");
    fs::create_dir(&root).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("ledger.db");
    let c = Connection::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    c.execute_batch(include_str!("../migrations/001.sql"))
        .unwrap();
    c.execute(
        "INSERT INTO permit VALUES('p',1,'local-owner','hash','{}',1,999999,NULL)",
        [],
    )
    .unwrap();
    c.execute("INSERT INTO work VALUES('old','local-owner','key','hash','{}','p','active','queued',1,'coordinator','prepare_fixture',100,0)",[]).unwrap();
    c.execute(
        "INSERT INTO event VALUES(1,'old','local-owner','work.accepted','old',1,'{}')",
        [],
    )
    .unwrap();
    drop(c);
    assert_eq!(Reader::open(&root).unwrap().list().unwrap()[0].id, "old");
    drop(Owner::open(&root).unwrap());
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        c.query_row("SELECT version FROM schema_meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(
        c.query_row("SELECT work_id FROM event", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "old"
    );
    assert_eq!(Reader::open(&root).unwrap().list().unwrap()[0].id, "old");
    assert!(c.execute("DELETE FROM work", []).is_err());
    assert!(
        c.execute("UPDATE work SET request_hash='changed'", [])
            .is_err()
    );
    let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(q.query([]).unwrap().next().unwrap().is_none());
}
#[cfg(feature = "fault-injection")]
#[test]
fn sqlite_failure_still_cleans_without_durable_ack() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "before_release", "hold");
    let db = f.db();
    db.execute_batch("CREATE TRIGGER fail_fact BEFORE INSERT ON supervisor_fact BEGIN SELECT RAISE(ABORT,'injected unavailable ledger'); END;").unwrap();
    fs::write(b.path().join("release"), b"").unwrap();
    wait(&mut c);
    assert_eq!(f.state(), "unknown");
    assert!(!f.facts().contains(&"cleanup_completed".into()));
    let payload: String = db
        .query_row(
            "SELECT payload_json FROM supervisor_fact WHERE kind='starting'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let identity: asher_bridge::process_scope::Identity = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()["identity"].clone(),
    )
    .unwrap();
    assert!(
        !running_identity(&identity),
        "cleanup must not depend on SQLite"
    );
    db.execute_batch("DROP TRIGGER fail_fact").unwrap();
    ok(f.run("reconcile", &[]));
    assert_eq!(f.state(), "unknown");
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn permit_revoked_before_release() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "before_release", "complete");
    f.db()
        .execute("UPDATE permit SET revoked_at=?1", [now_ms().unwrap()])
        .unwrap();
    fs::write(b.path().join("release"), b"").unwrap();
    wait(&mut c);
    assert!(!f.facts().contains(&"started".into()));
    assert_eq!(f.state(), "cleaned");
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn owner_gone_before_release() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "before_release", "complete");
    c.kill().unwrap();
    c.wait().unwrap();
    fs::write(b.path().join("release"), b"").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        ok(f.run("reconcile", &[]));
        if f.state() == "cleaned" {
            break;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!f.facts().contains(&"started".into()));
    f.check();
}
#[test]
fn output_and_runtime_are_bounded() {
    for mode in ["hold", "flood"] {
        let f = Fixture::new();
        let start = Instant::now();
        ok(f.run("run-fake", &["w", "a", mode, "100"]));
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(f.state(), "cleaned");
        let path: String = f
            .db()
            .query_row("SELECT fixture_path FROM attempt", [], |r| r.get(0))
            .unwrap();
        assert!(
            fs::metadata(PathBuf::from(path).join("captured.log"))
                .unwrap()
                .len()
                <= 16384
        );
        f.check();
    }
}
#[test]
fn supervisor_rejects_untrusted_stdin() {
    let f = Fixture::new();
    let out = Command::new(env!("CARGO_BIN_EXE_supervisor"))
        .arg(&f.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(f.count("supervisor_command"), 0);
}
#[cfg(feature = "fault-injection")]
#[test]
fn acceptance_is_not_workload_started() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "after_supervisor_accept", "complete");
    c.kill().unwrap();
    c.wait().unwrap();
    ok(f.run("reconcile", &[]));
    assert_eq!(
        f.state(),
        "accepted",
        "a live supervisor is not proof of workload release"
    );
    fs::write(b.path().join("release"), b"").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        ok(f.run("reconcile", &[]));
        if f.state() == "unknown" {
            break;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!f.facts().contains(&"started".into()));
    f.check();
}
#[test]
fn parent_exit_with_open_output_is_bounded() {
    let f = Fixture::new();
    let start = Instant::now();
    ok(f.run("run-fake", &["w", "a", "pipe_holder", "1000"]));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(matches!(f.state().as_str(), "cleaned" | "unknown"));
    f.check();
}

#[test]
fn supervisor_protocol_replay_and_role_fencing() {
    use std::io::{Read, Write};
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    let f = Fixture::new();
    let mut owner = Owner::open(&f.root).unwrap();
    let a = owner
        .intent(
            "w",
            "a",
            &asher_bridge::execution::Spec {
                mode: asher_bridge::execution::FakeMode::Complete,
                runtime_ms: 1000,
                output_limit: 4096,
            },
        )
        .unwrap();
    let message = serde_json::json!({"version":1,"role":"coordinator","attempt_id":"a","command_id":"launch:a","fingerprint":a.spec_hash,"epoch":owner.epoch});
    let exchange = |msg: &serde_json::Value| {
        let (mut local, remote) = UnixStream::pair().unwrap();
        local
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_supervisor"));
        command
            .arg(&f.root)
            .stdin(Stdio::from(OwnedFd::from(remote)))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        drop(command);
        writeln!(local, "{msg}").unwrap();
        let mut reply = String::new();
        local.read_to_string(&mut reply).unwrap();
        (child.wait().unwrap(), reply)
    };
    for bad in [
        serde_json::json!({"role":"agent"}),
        serde_json::json!({"epoch":owner.epoch-1}),
        serde_json::json!({"fingerprint":"changed"}),
        serde_json::json!({"shell":"forbidden"}),
    ] {
        let mut msg = message.clone();
        for (key, value) in bad.as_object().unwrap() {
            msg[key] = value.clone();
        }
        let (status, reply) = exchange(&msg);
        assert!(!status.success());
        assert!(!reply.contains("accepted"));
        assert_eq!(f.count("supervisor_command"), 0);
    }
    let (status, reply) = exchange(&message);
    assert!(status.success());
    assert!(reply.contains("accepted"));
    let (status, reply) = exchange(&message);
    assert!(status.success());
    assert!(reply.contains("existing"));
    assert!(reply.contains("cleanup_completed"));
    assert_eq!(f.count("supervisor_command"), 1);
    assert_eq!(
        f.facts().iter().filter(|s| s.as_str() == "started").count(),
        1
    );
    owner.reconcile().unwrap();
    assert_eq!(f.state(), "cleaned");
    f.check();
    drop(owner);
    let (status, _) = exchange(&message);
    assert!(
        !status.success(),
        "same PID without held ownership must be rejected"
    );
}
#[cfg(feature = "fault-injection")]
#[test]
fn cleanup_keeps_termination_reason_visible() {
    let f = Fixture::new();
    ok(f.cmd("run-fake")
        .args(["w", "a", "complete", "1000"])
        .env("ASHER_TEST_FAIL", "reader_setup")
        .output()
        .unwrap());
    let output = f.run("attempts", &[]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("injected partial startup failure"));
    f.check();
}
#[cfg(feature = "fault-injection")]
#[test]
fn unchanged_unknown_deadline_is_not_postponed() {
    let f = Fixture::new();
    let (mut c, b) = paused(&f, "coordinator_observing", "hold");
    kill_supervisor(&f);
    fs::write(b.path().join("release"), b"").unwrap();
    wait(&mut c);
    assert_eq!(f.state(), "unknown");
    f.db()
        .execute("UPDATE work SET due_at=accepted_at", [])
        .unwrap();
    let before = Reader::open(&f.root).unwrap().list().unwrap()[0].clone();
    ok(f.run("reconcile", &[]));
    ok(f.run("reconcile", &[]));
    let after = Reader::open(&f.root).unwrap().list().unwrap()[0].clone();
    assert_eq!(
        after.due_at, before.due_at,
        "reopening must not postpone unresolved obligations"
    );
    assert_eq!(after.version, before.version);
    f.check();
}

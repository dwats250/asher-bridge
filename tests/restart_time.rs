//! Restart truthfulness. Every host reboot here is SIMULATED: the fault-injection boot
//! seam (`ASHER_TEST_BOOT_ID_FILE`) is pointed at a file the test rewrites, and the processes a
//! reboot would destroy are killed by verified identity. No host is rebooted. Faults are
//! synchronized by named barriers, never by timed races.
#![cfg(feature = "fault-injection")]
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fx {
    _t: TempDir,
    root: PathBuf,
    boot: PathBuf,
}
impl Fx {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("state");
        let boot = t.path().join("boot_id");
        fs::write(&boot, "sim-boot-1\n").unwrap();
        let f = Self { _t: t, root, boot };
        ok(f.run("init", &[]));
        ok(f.run("permit", &["p", "fixture:test"]));
        let req = serde_json::json!({"protocol_version":1,"request_key":"w","fixture_id":"fixture:test",
            "success_criteria":"fake lifecycle only","source_evidence":["fixture:synthetic"],"permit_id":"p"});
        ok(f.cmd("submit").stdin(Stdio::piped()).spawn_with_stdin(&req));
        f
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action)
            .arg(&self.root)
            .env("ASHER_TEST_BOOT_ID_FILE", &self.boot)
            .env("ASHER_TEST_CLOCK_OFFSET_FILE", self.wall_file())
            .env("ASHER_TEST_BOOTTIME_OFFSET_FILE", self.boottime_file());
        c
    }
    fn wall_file(&self) -> PathBuf {
        self._t.path().join("wall_offset_ms")
    }
    fn boottime_file(&self) -> PathBuf {
        self._t.path().join("boottime_offset_ms")
    }
    fn offset(p: &Path) -> i64 {
        fs::read_to_string(p)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }
    /// SIMULATED wall-clock step (forward or backward) with no boot-local time passing.
    fn step_wall(&self, ms: i64) {
        let w = Self::offset(&self.wall_file()) + ms;
        fs::write(self.wall_file(), w.to_string()).unwrap();
    }
    /// SIMULATED suspend: wall time and CLOCK_BOOTTIME both advance; CLOCK_MONOTONIC does not.
    fn suspend(&self, ms: i64) {
        self.step_wall(ms);
        let b = Self::offset(&self.boottime_file()) + ms;
        fs::write(self.boottime_file(), b.to_string()).unwrap();
    }
    /// SIMULATED boot-local time passing while the wall clock is held back (an unobserved
    /// rollback of the same size).
    fn boottime_passes(&self, ms: i64) {
        let b = Self::offset(&self.boottime_file()) + ms;
        fs::write(self.boottime_file(), b.to_string()).unwrap();
    }
    fn anomalies(&self, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT count(*) FROM time_anomaly WHERE kind=?1",
                [kind],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn run(&self, action: &str, args: &[&str]) -> Output {
        self.cmd(action).args(args).output().unwrap()
    }
    /// Simulated reboot: the next process reads a different boot identity.
    fn reboot(&self, to: &str) {
        fs::write(&self.boot, format!("{to}\n")).unwrap();
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
    fn facts(&self) -> Vec<String> {
        self.db()
            .prepare("SELECT kind FROM supervisor_fact ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    /// Every process this test's ledger names (supervisor and recorded identities), with its
    /// start time, so a kill only ever targets the exact process the ledger recorded.
    fn recorded(&self, extra: &[PathBuf]) -> Vec<(i32, String)> {
        let db = self.db();
        let mut out = Vec::new();
        if let Some((pid, start)) = db
            .query_row(
                "SELECT supervisor_pid,supervisor_start FROM supervisor_command",
                [],
                |r| Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .unwrap()
        {
            out.push((pid, start));
        }
        let mut q = db
            .prepare("SELECT payload_json FROM supervisor_fact")
            .unwrap();
        let rows: Vec<String> = q
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut ids: Vec<Value> = rows
            .iter()
            .filter_map(|r| {
                serde_json::from_str::<Value>(r)
                    .ok()?
                    .get("identity")
                    .cloned()
            })
            .collect();
        for p in extra {
            if let Ok(b) = fs::read(p) {
                ids.push(serde_json::from_slice(&b).unwrap());
            }
        }
        for i in ids {
            out.push((
                i["pid"].as_i64().unwrap() as i32,
                i["start"].as_str().unwrap().to_string(),
            ));
        }
        out
    }
}
trait SpawnStdin {
    fn spawn_with_stdin(&mut self, body: &Value) -> Output;
}
impl SpawnStdin for Command {
    fn spawn_with_stdin(&mut self, body: &Value) -> Output {
        use std::io::Write;
        let mut c = self
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
        c.wait_with_output().unwrap()
    }
}
fn ok(o: Output) {
    assert!(
        o.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
fn proc_start(pid: i32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(s.rsplit_once(')')?.1.split_whitespace().nth(19)?.into())
}
/// Simulated power loss: SIGKILL each recorded process whose kernel start time still matches
/// the recorded one (never a reused PID), then the paused orchestrator.
fn power_off(f: &Fx, mut orchestrator: Child, barrier: &TempDir) {
    let extra = [barrier.path().join("wrapper_identity.json")];
    for (pid, start) in f.recorded(&extra) {
        if proc_start(pid).as_deref() == Some(start.as_str()) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    // Bounded: the killed processes disappear (or become zombies of a dead parent).
    let end = Instant::now() + Duration::from_secs(10);
    for (pid, start) in f.recorded(&extra) {
        while proc_start(pid).as_deref() == Some(start.as_str())
            && !fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .rsplit_once(')')
                .is_some_and(|(_, t)| t.trim_start().starts_with('Z'))
        {
            assert!(
                Instant::now() < end,
                "recorded process {pid} survived power-off"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
fn paused(f: &Fx, barrier: &str, mode: &str) -> (Child, TempDir) {
    paused_with(f, barrier, mode, "5000")
}
fn paused_with(f: &Fx, barrier: &str, mode: &str, runtime_ms: &str) -> (Child, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = f
        .cmd("run-fake")
        .args(["w", "a", mode, runtime_ms])
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

#[test]
fn boot_seam_is_the_only_boot_source() {
    // Only src/host.rs may name the kernel boot identity path.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut readers = Vec::new();
    let mut stack = vec![src];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if fs::read_to_string(&p)
                .unwrap()
                .contains("/proc/sys/kernel/random/boot_id")
            {
                readers.push(p.file_name().unwrap().to_string_lossy().to_string());
            }
        }
    }
    assert_eq!(readers, vec!["host.rs".to_string()], "boot id readers");
    // Under the seam, the owner session records the simulated boot.
    let f = Fx::new();
    assert_eq!(
        f.text("SELECT boot FROM owner_session ORDER BY epoch DESC LIMIT 1"),
        "sim-boot-1"
    );
}

#[test]
fn startup_records_host_boot_change() {
    let f = Fx::new();
    // First startup records the boot it ran under; same-boot restarts add nothing.
    assert_eq!(f.count("SELECT count(*) FROM host_boot"), 1);
    assert_eq!(f.text("SELECT current_boot FROM host_boot"), "sim-boot-1");
    ok(f.run("reconcile", &[]));
    assert_eq!(f.count("SELECT count(*) FROM host_boot"), 1);
    f.reboot("sim-boot-2");
    ok(f.run("reconcile", &[]));
    assert_eq!(f.count("SELECT count(*) FROM host_boot"), 2);
    let (prev, cur, epoch): (String, String, i64) = f
        .db()
        .query_row(
            "SELECT previous_boot,current_boot,detected_epoch FROM host_boot ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((prev.as_str(), cur.as_str()), ("sim-boot-1", "sim-boot-2"));
    assert_eq!(epoch, f.count("SELECT epoch FROM coordinator_epoch"));
    assert_eq!(
        f.count("SELECT count(*) FROM event WHERE kind='host.rebooted'"),
        1
    );
    // Append-only.
    assert!(f.db().execute("DELETE FROM host_boot", []).is_err());
    assert!(
        f.db()
            .execute("UPDATE host_boot SET current_boot='x'", [])
            .is_err()
    );
    // Diagnose shows it read-only.
    let o = f.run("diagnose", &[]);
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(d["ledger"]["host"]["current_boot"], "sim-boot-2", "{d}");
    assert_eq!(d["ledger"]["host"]["boot_changes"], 1, "{d}");
    assert_eq!(f.count("SELECT count(*) FROM host_boot"), 2);
}

/// One stage per lifecycle state a reboot can interrupt. Stages without their own barrier
/// (stop_requested, stopped) are reached by appending the facts the supervisor would have
/// written before the power loss.
#[test]
fn reboot_at_each_lifecycle_stage_is_a_gap_not_an_exit() {
    let stages: &[(&str, &str, &str, &[&str])] = &[
        ("intent", "after_launch_intent", "hold", &[]),
        ("accepted", "after_supervisor_accept", "hold", &[]),
        ("starting", "before_release", "hold", &[]),
        ("running", "coordinator_observing", "hold", &[]),
        ("exited", "after_parent_exit", "descendant", &[]),
        (
            "stop_requested",
            "coordinator_observing",
            "hold",
            &["stop_requested"],
        ),
        (
            "stopped",
            "coordinator_observing",
            "hold",
            &["stop_requested", "stop_confirmed"],
        ),
    ];
    for (stage, barrier, mode, appended) in stages {
        let f = Fx::new();
        let (child, b) = paused(&f, barrier, mode);
        power_off(&f, child, &b);
        for kind in *appended {
            let payload = if *kind == "stop_requested" {
                r#"{"reason":"runtime limit"}"#
            } else {
                r#"{"census_empty":true}"#
            };
            f.db()
                .execute(
                    "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a',?1,1,?2)",
                    [kind, &payload],
                )
                .unwrap();
        }
        let facts = f.facts();
        let commands = f.count("SELECT count(*) FROM supervisor_command");
        f.reboot("sim-boot-2");
        ok(f.run("reconcile", &[]));
        ok(f.run("reconcile", &[]));
        let (state, reason): (String, String) = f
            .db()
            .query_row("SELECT state,reason FROM attempt WHERE id='a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        // Never a fabricated exit or cleanup fact, never a second launch.
        assert_eq!(f.facts(), facts, "{stage}: facts changed");
        assert_eq!(
            f.count("SELECT count(*) FROM supervisor_command"),
            commands,
            "{stage}"
        );
        if *stage == "intent" {
            assert_eq!(commands, 0);
            assert_eq!(state, "intent", "{stage}: {reason}");
        } else {
            assert_eq!(state, "unknown", "{stage}: {reason}");
            assert!(
                reason.contains("host rebooted since launch")
                    && reason.contains("sim-boot-1")
                    && reason.contains("sim-boot-2")
                    && reason.contains("exit unobserved"),
                "{stage}: {reason}"
            );
        }
        assert_eq!(f.count("SELECT count(*) FROM host_boot WHERE previous_boot='sim-boot-1' AND current_boot='sim-boot-2'"), 1, "{stage}");
        if *stage == "intent" {
            assert_eq!(f.count("SELECT count(*) FROM evidence_gap"), 0, "{stage}");
            continue;
        }
        assert_eq!(
            f.text(
                "SELECT next_action FROM work WHERE id=(SELECT work_id FROM attempt WHERE id='a')"
            ),
            "inspect_supervision_uncertainty",
            "{stage}"
        );
        // One durable gap per attempt, not one per reconcile.
        assert_eq!(
            f.count(
                "SELECT count(*) FROM evidence_gap WHERE attempt_id='a' AND kind='host_rebooted'"
            ),
            1,
            "{stage}"
        );
        // A later run-fake is not a relaunch.
        let _ = f.run("run-fake", &["w", "a", "hold", "1000"]);
        assert_eq!(
            f.count("SELECT count(*) FROM supervisor_command"),
            commands,
            "{stage}: relaunched"
        );
        assert_eq!(f.facts(), facts, "{stage}: facts changed after run-fake");
    }
}

/// An attempt that completed cleanup before the reboot keeps its M1 classification.
#[test]
fn reboot_after_cleanup_keeps_the_cleaned_classification() {
    let f = Fx::new();
    ok(f.run("run-fake", &["w", "a", "complete", "1000"]));
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "cleaned");
    f.reboot("sim-boot-2");
    ok(f.run("reconcile", &[]));
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "cleaned");
    assert_eq!(f.count("SELECT count(*) FROM evidence_gap"), 0);
}

/// PID reuse across a reboot: the recorded PID now names a live process of the new boot. The
/// earlier-boot identity is still classified stale (gap), never live and never this process.
#[test]
fn pid_reuse_across_boots_is_not_the_recorded_process() {
    let f = Fx::new();
    let (child, b) = paused(&f, "coordinator_observing", "hold");
    power_off(&f, child, &b);
    // A live process stands in for "a new-boot process that got the recorded PID": its stat is
    // presented under the recorded supervisor PID with the recorded start time.
    let mut other = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let (pid, start): (i32, String) = f
        .db()
        .query_row(
            "SELECT supervisor_pid,supervisor_start FROM supervisor_command",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let stats = tempfile::tempdir().unwrap();
    let real = fs::read_to_string(format!("/proc/{}/stat", other.id())).unwrap();
    let tail = real.rsplit_once(')').unwrap().1;
    let mut fields: Vec<&str> = tail.split_whitespace().collect();
    fields[19] = &start;
    fs::write(
        stats.path().join(pid.to_string()),
        format!("{pid} (supervisor) {}", fields.join(" ")),
    )
    .unwrap();
    f.reboot("sim-boot-2");
    let o = f
        .cmd("reconcile")
        .env("ASHER_TEST_PROC_STAT_DIR", stats.path())
        .output()
        .unwrap();
    let _ = other.kill();
    let _ = other.wait();
    ok(o);
    let reason = f.text("SELECT reason FROM attempt WHERE id='a'");
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "unknown");
    assert!(reason.contains("host rebooted since launch"), "{reason}");
}

/// Migration 008 is additive from every historical schema version: disposable ledgers are
/// built at v1..v7 from the checked-in migrations, seeded, and opened by this build. Every
/// pre-existing schema object keeps its exact definition and every pre-existing row survives.
#[test]
fn migration_008_is_additive_from_every_historical_version() {
    use std::os::unix::fs::PermissionsExt;
    let steps = [
        include_str!("../migrations/001.sql"),
        include_str!("../migrations/002.sql"),
        include_str!("../migrations/003.sql"),
        include_str!("../migrations/004.sql"),
        include_str!("../migrations/005.sql"),
        include_str!("../migrations/006.sql"),
        include_str!("../migrations/007.sql"),
    ];
    let objects = |c: &Connection| -> Vec<(String, String)> {
        c.prepare("SELECT name,COALESCE(sql,'') FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let rows = |c: &Connection, t: &str| -> Vec<String> {
        let mut q = c
            .prepare(&format!("SELECT * FROM {t} ORDER BY rowid"))
            .unwrap();
        let n = q.column_count();
        q.query_map([], |r| {
            Ok((0..n)
                .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                .collect::<Vec<_>>()
                .join("|"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    let v7_objects = {
        let c = Connection::open_in_memory().unwrap();
        for s in &steps {
            c.execute_batch(s).unwrap();
        }
        objects(&c)
    };
    for version in 1..=7usize {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("ledger.db");
        let c = Connection::open(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for s in &steps[..version] {
            c.execute_batch(s).unwrap();
        }
        c.execute_batch(
            "INSERT INTO permit VALUES('p',1,'local-owner','hash','{}',1,4102444800000,NULL);
             INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(NULL,'local-owner','fixture.seed','seed',5,'{}');",
        )
        .unwrap();
        let before_objects = objects(&c);
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('schema_meta','coordinator_epoch')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let before_rows: Vec<(String, Vec<String>)> =
            tables.iter().map(|t| (t.clone(), rows(&c, t))).collect();
        drop(c);
        let boot = t.path().join("boot");
        fs::write(&boot, "sim-boot-1").unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
            .args(["reconcile"])
            .arg(&root)
            .env("ASHER_TEST_BOOT_ID_FILE", &boot)
            .output()
            .unwrap();
        ok(o);
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            c.query_row("SELECT version FROM schema_meta", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            8,
            "from v{version}"
        );
        let after_objects = objects(&c);
        // Every schema object of the historical chain 001..007 keeps its exact definition;
        // 008 only adds. (Objects older steps rebuild are the M1 chain's own behaviour.)
        for o in &v7_objects {
            assert!(
                after_objects.contains(o),
                "v{version}: object changed: {o:?}"
            );
        }
        if version == 7 {
            assert!(before_objects.iter().all(|o| after_objects.contains(o)));
        }
        // Every pre-existing row survives byte-for-byte (startup may only append).
        for (t, before) in &before_rows {
            let after = rows(&c, t);
            for r in before {
                assert!(after.contains(r), "v{version}: {t} row changed: {r}");
            }
        }
        for t in ["host_boot", "time_floor", "time_anomaly", "evidence_gap"] {
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1,
                "v{version}: {t}"
            );
        }
        assert_eq!(
            c.query_row("SELECT current_boot FROM host_boot", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "sim-boot-1"
        );
        let mut q = c.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(q.query([]).unwrap().next().unwrap().is_none());
    }
}

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

/// Waits (bounded) for a condition another process establishes; never a timed race window.
/// The bound is below the 15 s barrier hold, so a paused coordinator is still paused.
fn until(what: &str, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while !cond() {
        if Instant::now() > end {
            eprintln!("timed out waiting for {what}");
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}
fn release(b: &TempDir) {
    fs::write(b.path().join("release"), b"").unwrap();
}
fn finished(child: &mut Child) -> bool {
    until("orchestrator exit", || child.try_wait().unwrap().is_some())
}
fn kill_exact(pid: i32, start: &str) {
    if proc_start(pid).as_deref() == Some(start) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// SIMULATED clock rollback (fault-injection wall offset; the host clock is never changed).
/// A permit that expired never becomes valid again: not after the expiry was observed and the
/// wall clock then stepped back, not when boot-local time shows the time passed although the
/// wall clock did not, and not across a simulated reboot.
#[test]
fn clock_and_budget_rollback_never_renews_a_permit() {
    // Observed forward, then rolled back.
    let f = Fx::new();
    f.step_wall(2 * HOUR);
    ok(f.run("reconcile", &[]));
    f.step_wall(-2 * HOUR);
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(
        !o.status.success(),
        "a rolled-back clock renewed the permit"
    );
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("permit"),
        "{o:?}"
    );
    assert_eq!(f.count("SELECT count(*) FROM attempt"), 0);
    assert!(f.anomalies("regression") >= 1);
    // Boot-local time passed (2h) but the wall clock did not move: an unobserved rollback.
    let f = Fx::new();
    f.boottime_passes(2 * HOUR);
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(
        !o.status.success(),
        "boot-local evidence of expiry was ignored"
    );
    assert_eq!(f.count("SELECT count(*) FROM attempt"), 0);
    assert!(f.anomalies("regression") >= 1);
    // Across a simulated reboot boot-local time is not comparable; the durable floor still
    // holds the latest accepted wall time.
    let f = Fx::new();
    f.step_wall(2 * HOUR);
    ok(f.run("reconcile", &[]));
    f.reboot("sim-boot-2");
    f.step_wall(-2 * HOUR);
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(
        !o.status.success(),
        "a reboot plus rollback renewed the permit"
    );
    assert_eq!(f.count("SELECT count(*) FROM attempt"), 0);
    assert!(f.anomalies("regression") >= 1);
    // The floor is never lowered.
    assert!(
        f.db()
            .execute("UPDATE time_floor SET wall_ms=wall_ms-1", [])
            .is_err()
    );
}

/// SIMULATED unreadable wall clock while a supervised workload runs: authority is treated as
/// expired by the existing permit path, the supervisor stops its own verified scope, the
/// lifecycle facts are still recorded, and the anomaly is durable.
#[test]
fn clock_and_budget_unreadable_wall_is_treated_as_expired() {
    let f = Fx::new();
    let (mut child, b) = paused_with(&f, "coordinator_observing", "hold", "60000");
    fs::write(f.wall_file(), "unreadable").unwrap();
    let stopped = until("supervisor terminal fact", || {
        f.facts()
            .iter()
            .any(|k| k == "cleanup_completed" || k == "unknown")
    });
    fs::write(f.wall_file(), "0").unwrap();
    if !stopped {
        power_off(&f, child, &b);
        panic!("an unreadable wall clock did not end authority");
    }
    release(&b);
    assert!(finished(&mut child));
    let reason = f.text(
        "SELECT payload_json FROM supervisor_fact WHERE kind='stop_requested' ORDER BY seq LIMIT 1",
    );
    assert!(reason.contains("permit"), "{reason}");
    assert!(f.anomalies("unreadable") >= 1);
}

/// SIMULATED suspend and forward step. Suspended time is explained by CLOCK_BOOTTIME and is
/// not an anomaly; an unexplained forward wall step is a jump; a step back is a regression.
/// Both are durable and shown by a read-only diagnose.
#[test]
fn clock_and_budget_regression_and_jump_are_durable_and_in_diagnose() {
    let f = Fx::new();
    f.suspend(10 * MIN);
    ok(f.run("reconcile", &[]));
    assert_eq!(f.count("SELECT count(*) FROM time_anomaly"), 0);
    f.step_wall(10 * MIN);
    ok(f.run("reconcile", &[]));
    assert_eq!(f.anomalies("jump"), 1);
    f.step_wall(-30 * MIN);
    ok(f.run("reconcile", &[]));
    assert_eq!(f.anomalies("regression"), 1);
    let before = f.count("SELECT count(*) FROM time_anomaly");
    let floor = f.count("SELECT wall_ms FROM time_floor");
    let o = f.run("diagnose", &[]);
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    let t = &d["ledger"]["time"];
    assert_eq!(t["anomalies"]["regression"], 1, "{d}");
    assert_eq!(t["anomalies"]["jump"], 1, "{d}");
    assert_eq!(t["floor"]["wall_ms"], floor, "{d}");
    assert_eq!(t["authority"], "uncertain", "{d}");
    assert_eq!(f.count("SELECT count(*) FROM time_anomaly"), before);
    assert_eq!(f.count("SELECT wall_ms FROM time_floor"), floor);
}

/// SIMULATED suspend during a running workload: the runtime budget is charged with the
/// suspended time (CLOCK_BOOTTIME), so the supervisor stops its verified scope at once
/// instead of after 60 s of CLOCK_MONOTONIC.
#[test]
fn clock_and_budget_simulated_suspend_counts_against_runtime() {
    let f = Fx::new();
    let (mut child, b) = paused_with(&f, "coordinator_observing", "hold", "60000");
    f.suspend(2 * MIN);
    let stopped = until("supervisor terminal fact", || {
        f.facts()
            .iter()
            .any(|k| k == "cleanup_completed" || k == "unknown")
    });
    if !stopped {
        power_off(&f, child, &b);
        panic!("suspended time was not charged to the runtime budget");
    }
    release(&b);
    assert!(finished(&mut child));
    let reason = f.text(
        "SELECT payload_json FROM supervisor_fact WHERE kind='stop_requested' ORDER BY seq LIMIT 1",
    );
    assert!(reason.contains("runtime deadline reached"), "{reason}");
    // A suspend is not a clock anomaly.
    assert_eq!(f.count("SELECT count(*) FROM time_anomaly"), 0);
}

/// Without a verified current-boot supervised identity, an exceeded runtime budget is
/// recorded and routed to the owner; recovery never signals. Same boot (supervisor gone, the
/// workload left running) and after a simulated reboot.
#[test]
fn exceeded_budget_without_current_boot_identity_records_and_raises_attention_without_signal() {
    // Same boot: the supervisor and coordinator die; the workload keeps running.
    let f = Fx::new();
    let (mut child, b) = paused_with(&f, "coordinator_observing", "hold", "60000");
    let all = f.recorded(&[b.path().join("wrapper_identity.json")]);
    let (spid, sstart) = all[0].clone();
    kill_exact(spid, &sstart);
    let _ = child.kill();
    let _ = child.wait();
    assert!(until("supervisor gone", || proc_start(spid).as_deref()
        != Some(sstart.as_str())
        || fs::read_to_string(format!("/proc/{spid}/stat"))
            .unwrap_or_default()
            .rsplit_once(')')
            .is_some_and(|(_, t)| t.trim_start().starts_with('Z'))));
    // A test-owned process recorded as a current-boot descendant of the attempt: it lives
    // until the test kills it, so "not signalled" is observable without racing the real
    // workload's own exit after its supervisor died.
    let mut bystander = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let bpid = bystander.id() as i32;
    let bstart = proc_start(bpid).unwrap();
    let boot = fs::read_to_string(&f.boot).unwrap().trim().to_string();
    let group = fs::read_to_string(format!("/proc/{bpid}/stat")).unwrap();
    let group: i32 = group
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap();
    f.db()
        .execute(
            "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','descendant_observed',1,?1)",
            [serde_json::json!({"identity":{"pid":bpid,"start":bstart,"boot":boot,"group":group},"escaped":false}).to_string()],
        )
        .unwrap();
    let facts = f.facts();
    f.step_wall(2 * MIN);
    ok(f.run("reconcile", &[]));
    let reason = f.text("SELECT reason FROM attempt WHERE id='a'");
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "unknown");
    assert!(
        reason.contains("runtime budget exceeded") && reason.contains("not signalled"),
        "{reason}"
    );
    assert!(
        f.count("SELECT count(*) FROM attention WHERE state<>'resolved' AND work_id=(SELECT work_id FROM attempt WHERE id='a')")
            >= 1
    );
    // Nothing was signalled or fabricated: the live workload is untouched.
    assert_eq!(f.facts(), facts);
    assert_eq!(
        proc_start(bpid).as_deref(),
        Some(bstart.as_str()),
        "signalled"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
    for (pid, start) in &all[1..] {
        kill_exact(*pid, start);
    }
    // After a simulated reboot.
    let f = Fx::new();
    let (child, b) = paused_with(&f, "coordinator_observing", "hold", "60000");
    power_off(&f, child, &b);
    f.reboot("sim-boot-2");
    f.step_wall(2 * MIN);
    ok(f.run("reconcile", &[]));
    let reason = f.text("SELECT reason FROM attempt WHERE id='a'");
    assert!(
        reason.contains("host rebooted since launch") && reason.contains("runtime budget exceeded"),
        "{reason}"
    );
    assert!(
        f.count("SELECT count(*) FROM attention WHERE state<>'resolved' AND work_id=(SELECT work_id FROM attempt WHERE id='a')")
            >= 1
    );
}

// ------------------------------------------------------------------ adversarial cases
// All clock changes and reboots
// are SIMULATED through the fault-injection seams.

/// A clock rollback of less than the 1 s regression tolerance, after the permit's expiry was
/// already observed (and the floor advanced past it), must not make the permit valid again.
#[test]
fn r1_subsecond_rollback_never_renews_an_observed_expiry() {
    let f = Fx::new();
    // Permit expires at issue + 1 h. Observe a time just past it: the floor moves there.
    f.step_wall(HOUR + 100);
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(!o.status.success(), "expired permit authorized");
    let floor = f.count("SELECT wall_ms FROM time_floor");
    let expiry = f.count("SELECT expires_at FROM permit WHERE id='p'");
    assert!(
        floor >= expiry,
        "floor {floor} did not pass expiry {expiry}"
    );
    // Step the wall clock back 900 ms (below the 1 s tolerance), no boot-local time added.
    f.step_wall(-900);
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(
        !o.status.success(),
        "a sub-second rollback renewed a permit whose expiry the floor had already passed: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert_eq!(f.count("SELECT count(*) FROM attempt"), 0);
}

/// An unreadable wall clock at startup: authority must be refused and the anomaly durable.
/// Records what startup does (the claim under test: startup "never fails on time").
#[test]
fn r1_unreadable_wall_at_startup() {
    let f = Fx::new();
    fs::write(f.wall_file(), "unreadable").unwrap();
    let o = f.run("reconcile", &[]);
    eprintln!(
        "reconcile with unreadable wall: status={:?} stderr={}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    let o = f.run("run-fake", &["w", "a", "complete", "1000"]);
    assert!(!o.status.success(), "unreadable wall authorized a launch");
    assert_eq!(f.count("SELECT count(*) FROM attempt"), 0);
    let d = f.run("diagnose", &[]);
    eprintln!("diagnose: status={:?}", d.status);
    fs::write(f.wall_file(), "0").unwrap();
    assert!(
        f.anomalies("unreadable") >= 1,
        "unreadable wall clock at startup left no durable anomaly"
    );
}

/// A current-boot process recorded under the attempt before a simulated reboot is never
/// signalled by reconcile, recover or sweep after the reboot, even though a process with that
/// exact PID and start time is alive (the simulated reboot cannot kill it).
#[test]
fn r1_identity_from_earlier_boot_is_never_signalled_by_any_command() {
    let f = Fx::new();
    let (child, b) = paused(&f, "coordinator_observing", "hold");
    let mut bystander = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let bpid = bystander.id() as i32;
    let bstart = proc_start(bpid).unwrap();
    let group: i32 = fs::read_to_string(format!("/proc/{bpid}/stat"))
        .unwrap()
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap();
    f.db()
        .execute(
            "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','descendant_observed',1,?1)",
            [serde_json::json!({"identity":{"pid":bpid,"start":bstart,"boot":"sim-boot-1","group":group},"escaped":false}).to_string()],
        )
        .unwrap();
    // Power off everything except the bystander (not recorded in `recorded` order? kill exact).
    let extra = [b.path().join("wrapper_identity.json")];
    for (pid, start) in f.recorded(&extra) {
        if pid != bpid {
            kill_exact(pid, &start);
        }
    }
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    f.reboot("sim-boot-2");
    f.step_wall(2 * MIN);
    for cmd in ["reconcile", "recover", "status"] {
        let _ = f.run(cmd, &[]);
        assert_eq!(
            proc_start(bpid).as_deref(),
            Some(bstart.as_str()),
            "{cmd} signalled an identity recorded under an earlier boot"
        );
    }
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "unknown");
    assert!(
        f.text("SELECT reason FROM attempt WHERE id='a'")
            .contains("host rebooted")
    );
    assert_eq!(
        f.count("SELECT count(*) FROM evidence_gap WHERE kind='host_rebooted' AND attempt_id='a'"),
        1
    );
    assert!(
        !f.facts()
            .iter()
            .any(|k| k == "exited" || k == "cleanup_completed"),
        "fabricated exit"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
}

/// diagnose under a regression writes nothing: database, WAL and SHM bytes unchanged.
#[test]
fn r1_diagnose_under_regression_writes_no_byte() {
    let f = Fx::new();
    f.step_wall(2 * HOUR);
    ok(f.run("reconcile", &[]));
    f.step_wall(-3 * HOUR);
    f.reboot("sim-boot-9");
    let snap = || -> Vec<Option<Vec<u8>>> {
        ["ledger.db", "ledger.db-wal", "ledger.db-shm"]
            .iter()
            .map(|n| fs::read(f.root.join(n)).ok())
            .collect()
    };
    let before = snap();
    let o = f.run("diagnose", &[]);
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(d["ledger"]["time"]["authority"], "uncertain", "{d}");
    assert!(snap() == before, "diagnose changed ledger bytes");
}

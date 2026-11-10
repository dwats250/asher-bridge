//! Bounded, non-mutating diagnostics for a ledger whose ordinary startup cannot safely complete.
//!
//! `diagnose` never takes the coordinator lock, never migrates, never advances the epoch,
//! never writes `ledger.db` or `ledger.db-wal`, and never creates the lock file. It does not
//! open a ledger that SQLite would damage or block on by opening (a non-regular main file,
//! WAL or shared-memory file; an empty main file beside a WAL, which SQLite would discard; or
//! any rollback journal beside it, which SQLite would play back into the main file). Those are
//! examined from directory metadata only. It opens the database read-only (immutable when
//! there is no WAL and no coordinator holds the lock, so not even SQLite's shared-memory index
//! is created), interrupts its own queries from the deadline on and caps every list. It
//! distinguishes:
//!
//! * `absent`: no ledger file;
//! * `unavailable`: the file cannot be opened or read right now (permissions, I/O, locked, not
//!   a regular file);
//! * `unreadable`: it is not safely readable as a SQLite database (header damage, empty file,
//!   a rollback journal beside it, or a WAL without its main file);
//! * `corrupt`: readable, but the quick, full (while budget remains) or foreign-key integrity
//!   check fails;
//! * `incompatible`: not an Asher ledger, or a schema newer than this build;
//! * `degraded`: readable and verified, but ordinary startup is blocked, refused or cannot
//!   begin (startup breaker, restored or snapshot ledger, unsafe or backup directory, a ledger
//!   that cannot be written, unreadable recovery records, an epoch that cannot advance, or any
//!   failure of the startup steps before the epoch commit — migration, admission, the epoch
//!   transaction — found by running them on a private in-memory copy), the newest recovery run
//!   did not finish, or emergency records await import (every line of the emergency log, well
//!   formed or not, until a recovery pass has examined it; on a ledger below schema 7 nothing
//!   can have examined them);
//! * `inconclusive`: the ledger opened and its quick and foreign-key checks passed and nothing
//!   wrong was found, but at least one check that `healthy` relies on (the full integrity
//!   check, the startup dry run, the recovery checks or the emergency-record check) did not
//!   run or did not finish (size bound or time budget, or an emergency log that cannot be read
//!   or exceeds its bound), so whether ordinary startup can proceed is not established;
//!   `checks_not_completed` lists them; a failed read never stands for an empty log;
//! * `healthy`: every such check completed and none of the above.
use crate::{
    recovery::{self, EMERGENCY_LOG},
    store::{self, CURRENT_SCHEMA},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Wall-clock budget for the whole diagnosis.
pub const BUDGET_MS: u64 = 5000;
/// The bounded read of the emergency log: its lines (parsed, or the text of an unparseable
/// line), whether the file exceeded the bound, or why it could not be read.
type EmergencyLog = Result<(Vec<Result<Value, String>>, bool), String>;
const LIST: usize = 20;
const HASH_LIMIT: u64 = 64 * 1024 * 1024;

/// Largest ledger (main file plus WAL) copied into memory for the startup dry run.
const DRY_RUN_LIMIT: u64 = 64 * 1024 * 1024;

/// Test-only: sleep before the full integrity check so that a test can make the budget run
/// out after the quick check deterministically (fault-injection builds only).
#[cfg(feature = "fault-injection")]
fn test_delay_before_full_check() {
    let ms = std::env::var("ASHER_TEST_DIAGNOSE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
#[cfg(not(feature = "fault-injection"))]
fn test_delay_before_full_check() {}

enum DryRun {
    /// A startup step fails: every ordinary startup would fail the same way.
    Fails(String),
    /// The dry run could not finish (budget, memory, environment); this establishes nothing.
    Unfinished(String),
}
/// Run the startup steps up to the epoch commit on a private in-memory copy of the ledger.
fn dry_run(conn: &Connection, deadline: Instant) -> Result<(), DryRun> {
    let mut mem = Connection::open_in_memory().map_err(|e| DryRun::Unfinished(e.to_string()))?;
    let _dog = Watchdog::new(&mem, deadline);
    crate::restore::copy(conn, &mem).map_err(|e| DryRun::Unfinished(e.to_string()))?;
    match store::begin_startup(&mut mem, recovery::Trigger::Startup) {
        Ok(_) => Ok(()),
        // Already reported from the ledger itself, and not a precondition failure.
        Err(crate::Error::StartupBlocked(_) | crate::Error::Restored(_)) => Ok(()),
        Err(_) if Instant::now() >= deadline => Err(DryRun::Unfinished("budget exhausted".into())),
        Err(crate::Error::Io(e)) => Err(DryRun::Unfinished(e.to_string())),
        Err(e) => Err(DryRun::Fails(store::cause(&e))),
    }
}

/// Interrupts the connection's queries from the deadline on, until dropped.
struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new(conn: &Connection, deadline: Instant) -> Self {
        let handle = conn.get_interrupt_handle();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    handle.interrupt();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn file_facts(p: &Path, hash: bool) -> Value {
    match fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"present": false}),
        Err(e) => json!({"present": "unknown", "error": e.to_string()}),
        Ok(m) => {
            let mut v = json!({
                "present": true, "regular_file": m.is_file(), "symlink": m.file_type().is_symlink(),
                "bytes": m.len(), "mode": format!("{:o}", m.mode() & 0o7777), "links": m.nlink(),
            });
            if hash && m.is_file() && m.len() <= HASH_LIMIT {
                let mut b = Vec::new();
                let read = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(p)
                    .and_then(|f| f.take(HASH_LIMIT).read_to_end(&mut b));
                v["sha256"] = match read {
                    Ok(_) => json!(crate::artifact::sha256_hex(&b)),
                    Err(e) => json!(format!("unreadable: {e}")),
                };
            }
            v
        }
    }
}

/// Whether a process holds a lock on the coordinator lock file, from /proc/locks only.
fn lock_state(root: &Path) -> Value {
    let p = root.join("coordinator.lock");
    let m = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return json!({"state": "absent"}),
        Err(e) => return json!({"state": "unknown", "error": e.to_string()}),
    };
    let dev = m.dev();
    let want = format!(
        "{:02x}:{:02x}:{}",
        libc::major(dev),
        libc::minor(dev),
        m.ino()
    );
    let text = match fs::read_to_string("/proc/locks") {
        Ok(t) => t,
        Err(e) => return json!({"state": "unknown", "error": e.to_string()}),
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // "1: FLOCK  ADVISORY  WRITE 1234 08:02:5678 0 EOF" (blocked waiters have "->").
        if f.len() >= 6 && f[1] == "FLOCK" && f[5] == want {
            return json!({"state": "held", "kind": f[3], "pid": f[4].parse::<i64>().ok()});
        }
    }
    json!({"state": "free"})
}

/// Classification of an error raised while opening or reading the database.
fn class_of(e: &rusqlite::Error, readable: bool) -> &'static str {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => match f.code {
            rusqlite::ErrorCode::NotADatabase => "unreadable",
            rusqlite::ErrorCode::DatabaseCorrupt if readable => "corrupt",
            rusqlite::ErrorCode::DatabaseCorrupt => "unreadable",
            _ => "unavailable",
        },
        _ => "unavailable",
    }
}

pub fn diagnose(root: &Path) -> Value {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(BUDGET_MS);
    let root_ok = store::validate_root(root);
    let lock = lock_state(root);
    let mut out = json!({
        "diagnosis_version": 1,
        "mutating": false,
        "guarantee": "read-only: no lock taken, no migration, no epoch change, no write to ledger.db or its WAL",
        "root": {"path": root, "valid": root_ok.is_ok(), "error": root_ok.as_ref().err().map(|e| e.to_string())},
        "files": {
            "ledger.db": file_facts(&root.join("ledger.db"), true),
            "ledger.db-wal": file_facts(&root.join("ledger.db-wal"), true),
            "ledger.db-shm": file_facts(&root.join("ledger.db-shm"), false),
            "ledger.db-journal": file_facts(&root.join("ledger.db-journal"), false),
            "coordinator.lock": file_facts(&root.join("coordinator.lock"), false),
            "owner.key": file_facts(&root.join("owner.key"), false),
            EMERGENCY_LOG: file_facts(&root.join(EMERGENCY_LOG), false),
        },
        "coordinator_lock": lock,
        "supported_schema": CURRENT_SCHEMA,
        "bounds": {"budget_ms": BUDGET_MS, "list_limit": LIST, "hash_limit_bytes": HASH_LIMIT, "dry_run_limit_bytes": DRY_RUN_LIMIT},
    });
    let emergency = recovery::read_incidents(root);
    out["emergency_log"] = match &emergency {
        Ok((e, truncated)) => {
            let ok: Vec<&Value> = e.iter().filter_map(|x| x.as_ref().ok()).collect();
            json!({
                "records": ok.len(),
                "malformed": e.iter().filter(|x| x.is_err()).count(),
                "truncated": truncated,
                "latest": ok.iter().rev().take(LIST).collect::<Vec<_>>(),
            })
        }
        Err(e) => json!({"error": e}),
    };
    let (mut status, mut reasons, ledger) = inspect(root, deadline, &out, &emergency);
    if let Err(e) = &root_ok
        && matches!(status, "healthy" | "inconclusive" | "degraded")
    {
        status = "degraded";
        reasons.push(format!("ordinary startup refuses this state root: {e}"));
    }
    // Every command refuses what the pre-open guard refuses (for example a backup directory).
    if let Err(e) = store::preopen(root)
        && matches!(status, "healthy" | "inconclusive" | "degraded")
    {
        status = "degraded";
        reasons.push(format!("every command refuses this directory: {e}"));
    }
    // A ledger this user can read but not write fails every ordinary startup.
    if matches!(status, "healthy" | "inconclusive" | "degraded") {
        for p in [root.to_path_buf(), root.join("ledger.db")] {
            let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()));
            if c.is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0) {
                status = "degraded";
                reasons.push(format!(
                    "{} is not writable by this user; ordinary startup cannot write",
                    p.display()
                ));
            }
        }
    }
    out["ledger"] = ledger;
    out["status"] = json!(status);
    out["reasons"] = json!(reasons);
    out["accessible"] = json!(matches!(
        status,
        "healthy" | "inconclusive" | "degraded" | "corrupt" | "incompatible"
    ));
    out["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    out["next"] = json!(match status {
        "absent" => "no ledger here; never initialize over an expected ledger: check the path",
        "unavailable" =>
            "the ledger exists but cannot be opened or read now; preserve the files, fix permissions/storage/locking, then diagnose again; nothing was written",
        "unreadable" =>
            "not safely readable as a database; preserve ledger.db and any WAL or journal beside it untouched (inspect copies only); restore a verified backup into a separate target with `restore`",
        "corrupt" =>
            "integrity check failed; ordinary startup refuses; preserve the files; restore a verified backup into a separate target with `restore`",
        "incompatible" =>
            "schema not supported by this build; no migration was attempted; use a build that supports it",
        "degraded" =>
            "accessible but not safe for ordinary startup or awaiting recovery: see reasons; `recover` runs one bounded explicit pass (no consequential action)",
        "inconclusive" =>
            "accessible, but the checks listed in checks_not_completed did not run or did not finish (size bound or time budget), so diagnose cannot say whether ordinary startup can proceed; nothing was written; preserve the files and diagnose again, or examine a copy",
        _ => "ordinary startup can proceed",
    });
    out
}

fn inspect(
    root: &Path,
    deadline: Instant,
    so_far: &Value,
    emergency: &EmergencyLog,
) -> (&'static str, Vec<String>, Value) {
    let mut reasons = Vec::new();
    // Checks that `healthy` relies on which did not run or did not finish.
    let mut not_done: Vec<String> = Vec::new();
    let db = root.join("ledger.db");
    let main = &so_far["files"]["ledger.db"];
    let files = &so_far["files"];
    let journal = files["ledger.db-journal"]["present"] != false;
    if main["present"] == false {
        if files["ledger.db-wal"]["present"] != false || journal {
            return (
                "unreadable",
                vec!["ledger.db does not exist, but a WAL or rollback journal is present; it may hold committed transactions: preserve it".into()],
                json!({"open": "not attempted"}),
            );
        }
        return (
            "absent",
            vec!["ledger.db does not exist".into()],
            json!(null),
        );
    }
    // Never let SQLite open what it would damage or block on.
    if main["regular_file"] != true {
        return (
            "unavailable",
            vec!["ledger.db is not a regular file; not opened".into()],
            json!({"open": "not attempted"}),
        );
    }
    if journal {
        return (
            "unreadable",
            vec!["a rollback journal (ledger.db-journal) is beside the ledger; any ordinary open would play it back into ledger.db (or block on it), so nothing was opened; both files are preserved".into()],
            json!({"open": "not attempted"}),
        );
    }
    for name in ["ledger.db-wal", "ledger.db-shm"] {
        if files[name]["present"] != false && files[name]["regular_file"] != true {
            return (
                "unavailable",
                vec![format!(
                    "{name} is not a regular file (or its type is unknown); not opened"
                )],
                json!({"open": "not attempted"}),
            );
        }
    }
    let wal = files["ledger.db-wal"]["present"] == true;
    if main["bytes"] == 0 {
        let why = if wal {
            "the main database file is empty beside a WAL; not opened, so SQLite cannot discard the WAL"
        } else {
            "the main database file is empty"
        };
        return (
            "unreadable",
            vec![why.into()],
            json!({"open": "not attempted"}),
        );
    }
    crate::storage::prepare();
    let held = so_far["coordinator_lock"]["state"] == "held";
    let opened = if !wal && !held {
        store::open_immutable(&db).map_err(|e| match e {
            crate::Error::Sql(s) => s,
            other => rusqlite::Error::InvalidParameterName(other.to_string()),
        })
    } else {
        Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    };
    let conn = match opened {
        Ok(c) => c,
        Err(e) => {
            return (
                class_of(&e, false),
                vec![format!("open failed: {e}")],
                json!({"open": "failed", "error": e.to_string()}),
            );
        }
    };
    let _ = conn.busy_timeout(Duration::from_millis(1000));
    let _ = conn.pragma_update(None, "query_only", true);
    let _dog = Watchdog::new(&conn, deadline);
    let mut ledger = json!({"open": if !wal && !held {"read_only_immutable"} else {"read_only"},
                            "sqlite_version": rusqlite::version()});
    // Integrity first, bounded in output and time; rows are kept even if the check then fails.
    let mut rows = Vec::new();
    let integrity = (|| -> rusqlite::Result<()> {
        let mut s = conn.prepare(&format!("PRAGMA quick_check({LIST})"))?;
        let mut q = s.query([])?;
        while let Some(r) = q.next()? {
            rows.push(r.get::<_, String>(0)?);
        }
        Ok(())
    })();
    match integrity {
        Ok(()) if rows == ["ok"] => ledger["quick_check"] = json!("ok"),
        Ok(()) => {
            ledger["quick_check"] = json!(rows);
            reasons.push("quick_check reported problems".into());
            return ("corrupt", reasons, ledger);
        }
        Err(e) => {
            ledger["quick_check"] = json!(rows);
            ledger["quick_check_error"] = json!(e.to_string());
            if Instant::now() >= deadline {
                reasons.push("diagnosis budget exhausted during the integrity check".into());
                ledger["interrupted"] = json!(true);
                return ("unavailable", reasons, ledger);
            }
            reasons.push(format!("integrity check failed: {e}"));
            return (class_of(&e, true), reasons, ledger);
        }
    }
    let version: i64 = match conn.query_row(
        "SELECT version FROM schema_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    ) {
        Ok(v) => v,
        Err(e) => {
            ledger["schema_error"] = json!(e.to_string());
            let class = match &e {
                rusqlite::Error::SqliteFailure(..) if e.to_string().contains("no such table") => {
                    "incompatible"
                }
                rusqlite::Error::SqliteFailure(..) => class_of(&e, true),
                _ => "incompatible",
            };
            reasons.push(format!("schema version unreadable: {e}"));
            return (class, reasons, ledger);
        }
    };
    ledger["schema_version"] = json!(version);
    ledger["migration_required"] = json!(version < CURRENT_SCHEMA);
    if !(1..=CURRENT_SCHEMA).contains(&version) {
        reasons.push(format!(
            "schema version {version} is not supported (1..={CURRENT_SCHEMA})"
        ));
        return ("incompatible", reasons, ledger);
    }
    let fk = (|| -> rusqlite::Result<usize> {
        let mut s = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = s.query([])?;
        let mut n = 0;
        while rows.next()?.is_some() && n <= LIST {
            n += 1;
        }
        Ok(n)
    })();
    match fk {
        Ok(0) => {
            ledger["foreign_key_violations"] = json!(0);
            // The full check compares index content with table rows (quick_check does not);
            // it runs while budget remains and never decides availability by itself.
            test_delay_before_full_check();
            if Instant::now() < deadline {
                match store::integrity(&conn) {
                    Ok(rows) if rows == ["ok"] => ledger["integrity_check"] = json!("ok"),
                    Ok(rows) => {
                        ledger["integrity_check"] = json!(rows);
                        reasons.push("PRAGMA integrity_check reported problems (index content disagrees with table rows or similar)".into());
                        return ("corrupt", reasons, ledger);
                    }
                    Err(crate::Error::Corrupt) => {
                        reasons.push("PRAGMA integrity_check raised a corruption error".into());
                        return ("corrupt", reasons, ledger);
                    }
                    Err(e) => {
                        ledger["integrity_check"] = json!(format!("not completed: {e}"));
                        reasons.push("the full integrity check did not complete within the budget; only the quick check passed".into());
                        not_done.push("the full integrity check (PRAGMA integrity_check)".into());
                    }
                }
            } else {
                ledger["integrity_check"] = json!("not run: budget exhausted");
                not_done.push("the full integrity check (PRAGMA integrity_check)".into());
            }
        }
        Ok(n) => {
            ledger["foreign_key_violations"] = json!(n);
            reasons.push("foreign key violations".into());
            return ("corrupt", reasons, ledger);
        }
        Err(e) => {
            ledger["foreign_key_check_error"] = json!(e.to_string());
            reasons.push(format!("foreign key check could not run: {e}"));
            return (class_of(&e, true), reasons, ledger);
        }
    }
    let mut degraded = false;
    let count = |sql: &str| -> Value {
        if Instant::now() >= deadline {
            return json!("not examined: budget exhausted");
        }
        match conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
            Ok(n) => json!(n),
            Err(e) => json!(format!("unavailable: {e}")),
        }
    };
    let now = store::now_ms().unwrap_or(0);
    ledger["obligations"] = json!({
        "active_work": count("SELECT count(*) FROM work WHERE mode='active'"),
        "overdue_work": count(&format!("SELECT count(*) FROM work WHERE mode='active' AND due_at<{now}")),
        "unknown_attempts": count("SELECT count(*) FROM attempt WHERE state='unknown'"),
        "unsettled_attempts": count("SELECT count(*) FROM attempt WHERE state NOT IN ('cleaned','intent')"),
    });
    if version >= 4 {
        ledger["obligations"]["unresolved_effects"] = count(
            "SELECT count(*) FROM effect WHERE state IN ('dispatching','unknown','conflict')",
        );
        ledger["obligations"]["approved_undispatched_effects"] =
            count("SELECT count(*) FROM effect WHERE state='intent'");
    }
    if version >= 6 {
        ledger["obligations"]["open_blocking_attention"] =
            count("SELECT count(*) FROM attention WHERE state<>'resolved' AND blocking=1");
        ledger["obligations"]["open_discrepancies"] = count(
            "SELECT count(*) FROM attention WHERE state<>'resolved' AND source_key LIKE 'discrepancy:%'",
        );
    }
    // Boot identity (read-only): the running boot against the last owner session's, and the
    // durable boot-change history. Process identities from an earlier boot are stale.
    {
        let current = crate::host::boot_id();
        let recorded: Option<String> = conn
            .query_row(
                "SELECT boot FROM owner_session ORDER BY epoch DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        let mut host = json!({
            "current_boot": match &current { Ok(b) => json!(b), Err(e) => json!(format!("unreadable: {e}")) },
            "last_owner_session_boot": recorded,
            "rebooted_since_last_owner_session": match (&current, &recorded) {
                (Ok(c), Some(r)) => json!(c != r),
                _ => json!("unknown"),
            },
        });
        if version >= 8 {
            host["boot_changes"] =
                count("SELECT count(*) FROM host_boot WHERE previous_boot IS NOT NULL");
            host["reboot_gaps"] =
                count("SELECT count(*) FROM evidence_gap WHERE kind='host_rebooted'");
            host["latest_change"] = conn
                .query_row(
                    "SELECT previous_boot,current_boot,detected_epoch FROM host_boot WHERE previous_boot IS NOT NULL ORDER BY seq DESC LIMIT 1",
                    [],
                    |r| {
                        Ok(json!({"previous_boot": r.get::<_, String>(0)?,
                                  "current_boot": r.get::<_, String>(1)?,
                                  "detected_epoch": r.get::<_, i64>(2)?}))
                    },
                )
                .unwrap_or(Value::Null);
        }
        ledger["host"] = host;
    }
    // Explicit evidence gaps by kind (read-only). Each one blocks any pass on its attempt.
    if version >= 8 {
        let mut gaps = json!({});
        for kind in [
            "output_truncated",
            "capture_missing",
            "capture_torn",
            "fact_corrupt",
            "host_rebooted",
        ] {
            gaps[kind] = count(&format!(
                "SELECT count(*) FROM evidence_gap WHERE kind='{kind}'"
            ));
        }
        ledger["evidence_gaps"] = gaps;
    }
    // Time (read-only): the durable floor, recorded clock anomalies and the verdict an
    // authority check would reach now. Nothing is written; the floor is not advanced.
    if version >= 8 {
        let mut time = json!({
            "floor": conn
                .query_row(
                    "SELECT wall_ms,boot,epoch FROM time_floor WHERE singleton=1",
                    [],
                    |r| Ok(json!({"wall_ms": r.get::<_, i64>(0)?, "boot": r.get::<_, String>(1)?,
                                  "epoch": r.get::<_, i64>(2)?})),
                )
                .unwrap_or(Value::Null),
            "anomalies": {
                "regression": count("SELECT count(*) FROM time_anomaly WHERE kind='regression'"),
                "jump": count("SELECT count(*) FROM time_anomaly WHERE kind='jump'"),
                "unreadable": count("SELECT count(*) FROM time_anomaly WHERE kind='unreadable'"),
            },
            "latest_anomaly": conn
                .query_row(
                    "SELECT kind,epoch,boot,observed_wall,floor_wall FROM time_anomaly ORDER BY seq DESC LIMIT 1",
                    [],
                    |r| Ok(json!({"kind": r.get::<_, String>(0)?, "epoch": r.get::<_, i64>(1)?,
                                  "boot": r.get::<_, String>(2)?, "observed_wall": r.get::<_, Option<i64>>(3)?,
                                  "floor_wall": r.get::<_, i64>(4)?})),
                )
                .unwrap_or(Value::Null),
        });
        match store::authority_verdict(&conn) {
            Ok(store::AuthorityTime::Known(_)) => time["authority"] = json!("known"),
            Ok(store::AuthorityTime::Uncertain { why, .. }) => {
                time["authority"] = json!("uncertain");
                time["uncertain_because"] = json!(why);
            }
            Err(e) => time["authority"] = json!(format!("unavailable: {e}")),
        }
        ledger["time"] = time;
    }
    // Startup preconditions that can be checked read-only: the epoch must be able to advance
    // to a value no owner session already holds, and a pending migration's objects must not
    // already exist.
    match conn
        .query_row("SELECT epoch FROM coordinator_epoch", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()
    {
        Ok(epoch) => {
            let epoch = epoch.flatten();
            ledger["coordinator_epoch"] = json!(epoch);
            let next = epoch.unwrap_or(0).checked_add(1);
            match next {
                None => {
                    degraded = true;
                    reasons.push("the coordinator epoch is at its maximum and cannot advance; every ordinary startup fails".into());
                }
                Some(n) if version >= 2 => {
                    match conn.query_row(
                        "SELECT count(*) FROM owner_session WHERE epoch=?1",
                        [n],
                        |r| r.get::<_, i64>(0),
                    ) {
                        Ok(0) => (),
                        Ok(_) => {
                            degraded = true;
                            reasons.push(format!("an owner session already holds epoch {n}, the next epoch; every ordinary startup fails"));
                        }
                        Err(e) => {
                            degraded = true;
                            reasons.push(format!("owner sessions unreadable: {e}"));
                        }
                    }
                }
                Some(_) => (),
            }
        }
        Err(e) => {
            degraded = true;
            ledger["coordinator_epoch"] = json!(null);
            reasons.push(format!(
                "the coordinator epoch is unreadable ({e}); ordinary startup reads it and fails"
            ));
        }
    }
    if version < CURRENT_SCHEMA {
        reasons.push(format!(
            "schema {version} will be migrated additively to {CURRENT_SCHEMA} by the next mutating startup"
        ));
    }
    // Every other precondition of the steps before the epoch commit (any pending migration's
    // objects, adverse recovery history, the epoch transaction itself) is found by running
    // those very steps on a private in-memory copy. Nothing touches the ledger's files.
    let main_bytes = so_far["files"]["ledger.db"]["bytes"].as_u64().unwrap_or(0);
    let wal_bytes = so_far["files"]["ledger.db-wal"]["bytes"]
        .as_u64()
        .unwrap_or(0);
    ledger["startup_dry_run"] = if main_bytes.saturating_add(wal_bytes) > DRY_RUN_LIMIT {
        not_done.push("the startup dry run (pending migrations, admission and the epoch transaction on a private in-memory copy)".into());
        json!("not run: the ledger is larger than the in-memory dry-run bound")
    } else if Instant::now() >= deadline {
        not_done.push("the startup dry run (pending migrations, admission and the epoch transaction on a private in-memory copy)".into());
        json!("not run: budget exhausted")
    } else {
        match dry_run(&conn, deadline) {
            Ok(()) => json!("ok"),
            Err(DryRun::Unfinished(e)) => {
                not_done.push("the startup dry run (pending migrations, admission and the epoch transaction on a private in-memory copy)".into());
                json!(format!("not completed: {e}"))
            }
            Err(DryRun::Fails(e)) => {
                degraded = true;
                reasons.push(format!(
                    "ordinary startup cannot begin: migration, admission and the epoch transaction, run on a private in-memory copy, fail: {e}"
                ));
                json!(format!("fails: {e}"))
            }
        }
    };
    // One clock read decides whether the recovery and emergency checks run, so a deadline
    // passing in between can never skip them without recording it. Ledgers below schema 7 have
    // no recovery tables, so only the recovery block is not a check there; the emergency log
    // lives outside SQLite and is examined on every schema.
    let recovery_in_budget = Instant::now() < deadline;
    if !recovery_in_budget {
        if version >= 7 {
            not_done.push("the recovery checks (startup breaker, newest recovery run, restore or snapshot marker)".into());
        }
        not_done.push("the emergency-record check".into());
    }
    if version >= 7 && recovery_in_budget {
        match recovery::status(&conn) {
            Ok(r) => {
                if r["startup_blocked"] == true {
                    degraded = true;
                    reasons.push("ordinary startup is blocked: the newest recovery runs failed or never finished".into());
                } else if r["recent_runs"][0]["state"]
                    .as_str()
                    .is_some_and(|s| s != "completed" && s != "partial")
                {
                    degraded = true;
                    reasons.push(format!(
                        "the newest recovery run is '{}' (not completed){}",
                        r["recent_runs"][0]["state"].as_str().unwrap_or("?"),
                        if held {
                            "; a coordinator holds the lock and may be running it"
                        } else {
                            ""
                        }
                    ));
                }
                if !r["restore"].is_null() {
                    degraded = true;
                    reasons.push("derived from a backup (restored or snapshot): inspect-only; ordinary startup, launches and dispatch are refused".into());
                }
                ledger["recovery"] = r;
            }
            Err(e) => {
                degraded = true;
                ledger["recovery_error"] = json!(e.to_string());
                reasons.push(format!(
                    "recovery records are unreadable ({e}); ordinary startup reads them and fails"
                ));
            }
        }
    }
    if recovery_in_budget {
        // The emergency-record check. Every line of the log is an obligation until a recovery
        // pass has examined it under the same identity `recover` records it by (its incident,
        // or a line-bound identity for a torn, unparseable or unidentifiable line). A log that
        // cannot be read or exceeds its bound leaves the check incomplete: a failed or partial
        // read establishes nothing, least of all that nothing is pending. An absent log is an
        // empty one and needs no check.
        match emergency {
            Err(e) => {
                not_done.push(format!(
                    "the emergency-record check (the emergency log could not be read: {e})"
                ));
            }
            Ok((_, true)) => {
                not_done.push(format!(
                    "the emergency-record check (the emergency log exceeds its {} byte bound; its records were not examined)",
                    recovery::EMERGENCY_MAX_BYTES
                ));
            }
            Ok((entries, false)) => {
                let mut pending = 0usize;
                let mut complete = true;
                for (n, entry) in entries.iter().enumerate() {
                    if Instant::now() >= deadline {
                        ledger["emergency_records_examined_partially"] = json!(true);
                        not_done.push("the emergency-record check".into());
                        complete = false;
                        break;
                    }
                    let subject = recovery::record_subject(n, entry);
                    // A well-formed record is settled once it is accounted; a torn, unparseable
                    // or unidentifiable line once a recovery pass has recorded it (as
                    // unclassifiable, with its case) under the line-bound identity. Below
                    // schema 7 there is no recovery_item table: nothing has examined any
                    // record, and none is invented or settled.
                    let identified =
                        matches!(entry, Ok(v) if v["incident"].as_str() == Some(subject.as_str()));
                    let settled = version >= 7
                        && conn
                            .query_row(
                                "SELECT classification FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1 ORDER BY rowid DESC LIMIT 1",
                                [&subject],
                                |r| r.get::<_, String>(0),
                            )
                            .optional()
                            .ok()
                            .flatten()
                            .is_some_and(|c| !identified || c == "accounted");
                    if !settled {
                        pending += 1;
                    }
                }
                if complete {
                    ledger["emergency_records_not_settled"] = json!(pending);
                    if pending > 0 {
                        degraded = true;
                        reasons.push(format!(
                            "{pending} emergency record(s) await import or a conclusive read back{}",
                            if version < 7 {
                                " (a ledger below schema 7 has no recovery records that could have examined them)"
                            } else {
                                ""
                            }
                        ));
                    }
                }
            }
        }
    }
    if Instant::now() >= deadline {
        ledger["interrupted"] = json!(true);
        reasons.push("diagnosis budget exhausted; some facts were not examined".into());
    }
    if !not_done.is_empty() {
        ledger["checks_not_completed"] = json!(not_done);
        reasons.push(format!(
            "not established: {} did not run or did not finish (size bound or time budget); this diagnosis cannot say whether ordinary startup can proceed",
            not_done.join("; ")
        ));
    }
    // A known problem still wins over an incomplete check; its reasons then also list what
    // was not done.
    (
        if degraded {
            "degraded"
        } else if !not_done.is_empty() {
            "inconclusive"
        } else {
            "healthy"
        },
        reasons,
        ledger,
    )
}

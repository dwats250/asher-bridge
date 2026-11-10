use crate::{
    model::{Disposition, Receipt, Work},
    protocol::{PermitScope, SubmitWork, fingerprint, valid_id},
};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage failure; acceptance may be unknown: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("filesystem failure: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("same request key was already accepted with different content")]
    Conflict,
    #[error("coordinator already owned by another process")]
    Owned,
    #[error("unsupported schema version {0}; no migration performed")]
    Schema(i64),
    #[error("database integrity check failed")]
    Corrupt,
    #[error("permit is missing, expired, revoked, or outside scope")]
    Permit,
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("evidence conflict: same source/epoch/sequence was committed with different content")]
    EvidenceConflict,
    /// A classified failure at a documented transaction boundary (see `storage`).
    #[error("{0}")]
    Storage(Box<crate::storage::StorageFault>),
    /// Ordinary startup refused before any write; `diagnose` explains, `recover` may retry.
    #[error("ordinary startup refused without writing: {0}")]
    StartupBlocked(String),
    /// A ledger derived from a backup is inspect-only; ordinary startup is refused unwritten.
    #[error("ordinary startup refused without writing: {0}")]
    Restored(String),
    /// The ledger files cannot be read as a database; refused without opening them.
    #[error("ledger unreadable: {0}")]
    Unreadable(&'static str),
    /// Startup had begun (epoch advanced, recovery run started) when this failed; the second
    /// value says whether the failure itself was recorded on the run.
    #[error("startup began and failed ({record}): {cause}", record = failure_record(.1), cause = cause(.0))]
    StartupFailed(Box<Error>, crate::recovery::Recorded),
    /// The ledger was verified, but ordinary startup could not begin (migration, admission
    /// read or epoch transaction failed). The second value says what was written.
    #[error("ordinary startup could not begin on this verified ledger ({written}): {cause}", written = .1, cause = cause(.0))]
    StartupPrecondition(Box<Error>, Written),
    /// An internal COMMIT (`storage::Point`) failed in a way that leaves its outcome unknown.
    #[error("the COMMIT of {what} failed and its outcome is unknown: {cause}", what = .0, cause = cause(.1))]
    CommitUncertain(&'static str, Box<Error>),
}
/// What a startup that could not begin wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// Nothing: the failure is known to precede every startup COMMIT (no migration was
    /// pending, or the schema version read back unchanged after the failure).
    Nothing,
    /// The additive migration from this schema committed; nothing after it did.
    Migrated(i64),
    /// No epoch change, recovery run or reconciliation was committed, and whether the pending
    /// additive migration from this schema committed is unknown: the schema version could not
    /// be read back after the failure.
    MigrationUnknown(i64),
    /// A COMMIT failed with an unknown outcome: what it would have written may be in the ledger.
    /// The schema an additive migration had already committed from, if one had.
    Unknown(Option<i64>),
}
impl std::fmt::Display for Written {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Written::Nothing => f.write_str(
                "nothing was written: no migration, epoch change, recovery run or reconciliation",
            ),
            Written::Migrated(v) => write!(
                f,
                "the additive migration from schema {v} committed; no epoch change, recovery run or reconciliation was committed"
            ),
            Written::Unknown(m) => write!(
                f,
                "a startup COMMIT failed and its outcome is unknown: what it would have written may be in the ledger{}",
                m.map(|v| format!(" (the additive migration from schema {v} had committed)"))
                    .unwrap_or_default()
            ),
            Written::MigrationUnknown(v) => write!(
                f,
                "no epoch change, recovery run or reconciliation was committed; whether the additive migration from schema {v} committed is unknown: the schema version could not be read back after the failure"
            ),
        }
    }
}
fn failure_record(r: &crate::recovery::Recorded) -> &'static str {
    match r {
        crate::recovery::Recorded::Yes => "recorded as a failed recovery run",
        crate::recovery::Recorded::No => {
            "the failure could not be recorded; the run stays 'started' and the next pass marks it interrupted"
        }
        crate::recovery::Recorded::Unknown => {
            "recording the failure hit a storage fault, so it may or may not be recorded; if not, the run stays 'started' and the next pass marks it interrupted"
        }
    }
}
/// An inner cause without the generic "acceptance may be unknown" prefix of `Error::Sql`, which
/// speaks for work acceptance, not for a startup step.
pub fn cause(e: &Error) -> String {
    match e {
        Error::Sql(s) => sql_text(s),
        other => other.to_string(),
    }
}
/// A SQLite error without the full statement text a failed batch would otherwise quote.
pub(crate) fn sql_text(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqlInputError { msg, offset, .. } => {
            format!("{msg} (statement offset {offset})")
        }
        other => other.to_string(),
    }
}
const SCHEMA: &str = include_str!("../migrations/001.sql");
/// Newest schema this build understands. Newer or unknown versions are refused.
pub const CURRENT_SCHEMA: i64 = 8;
const COLUMNS: &str = "id,principal,request_key,request_hash,permit_id,mode,phase,version,next_owner,next_action,due_at,accepted_at";
pub struct Owner {
    pub(crate) conn: Connection,
    _lock: File,
    pub epoch: i64,
    root: PathBuf,
    recovery: Option<serde_json::Value>,
}
// Explicit unlock also releases a temporarily fork-inherited open-file description.
// Domain transactions cannot outlive Owner; connection drop only performs SQLite cleanup.
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._lock);
    }
}
pub struct Reader {
    conn: Connection,
}

pub fn now_ms() -> Result<i64, Error> {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("clock before epoch"))?
        .as_millis();
    let n = i64::try_from(n).map_err(|_| Error::Invalid("clock overflow"))?;
    n.checked_add(clock_offset()?)
        .ok_or(Error::Invalid("clock overflow"))
}
/// Test-only injected clock: a forward offset for expiry tests, optionally re-read from a
/// file so an already-running (delayed) process observes time passing. Absent from
/// ordinary builds.
#[cfg(feature = "fault-injection")]
fn clock_offset() -> Result<i64, Error> {
    if let Some(p) = std::env::var_os("ASHER_TEST_CLOCK_OFFSET_FILE") {
        if let Ok(v) = fs::read_to_string(p) {
            return v
                .trim()
                .parse()
                .map_err(|_| Error::Invalid("bad test clock offset"));
        }
    }
    match std::env::var("ASHER_TEST_CLOCK_OFFSET_MS") {
        Ok(v) => v
            .parse()
            .map_err(|_| Error::Invalid("bad test clock offset")),
        Err(_) => Ok(0),
    }
}
#[cfg(not(feature = "fault-injection"))]
fn clock_offset() -> Result<i64, Error> {
    Ok(0)
}
/// A wall step back larger than this (beyond what boot-local time allows) is a regression.
const REGRESSION_TOLERANCE_MS: i64 = 1_000;
/// A same-boot forward step larger than boot-local time explains by this much is a jump.
const JUMP_MS: i64 = 300_000;
/// Floor writes are coalesced: the floor moves when accepted wall time passes it by this.
const FLOOR_STEP_MS: i64 = 1_000;

/// The time an authority decision may use. `Uncertain` (unreadable wall or boot clock, or a
/// wall clock behind the durable floor) never authorizes: every deadline counts as reached
/// and every issue time as not yet reached, so existing expiry paths refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityTime {
    Known(i64),
    Uncertain { floor: i64, why: &'static str },
}
impl AuthorityTime {
    pub fn known(&self) -> Option<i64> {
        match self {
            Self::Known(n) => Some(*n),
            Self::Uncertain { .. } => None,
        }
    }
    /// The deadline `at` has been reached, or may have been.
    pub fn reached(&self, at: i64) -> bool {
        self.known().is_none_or(|n| n >= at)
    }
    /// `at` (an issue time) is still in the future, or may be.
    pub fn before(&self, at: i64) -> bool {
        self.known().is_none_or(|n| n < at)
    }
    /// A record stamp: the wall time, or when it is uncertain the durable floor (the latest
    /// wall time the ledger accepted). A stamp never authorizes anything.
    pub fn stamp(&self) -> i64 {
        match self {
            Self::Known(n) => *n,
            Self::Uncertain { floor, .. } => *floor,
        }
    }
}

struct Observation {
    verdict: AuthorityTime,
    /// (kind, observed wall, floor wall)
    anomaly: Option<(&'static str, Option<i64>, i64)>,
    /// New floor (wall, boot, boot-local ms).
    advance: Option<(i64, String, u64)>,
    boot: Option<String>,
    boot_ms: Option<u64>,
}

fn has_time_floor(conn: &Connection) -> Result<bool, Error> {
    Ok(conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='time_floor'",
        [],
        |r| r.get::<_, i64>(0),
    )? == 1)
}

/// Read-only assessment of the current time against the durable floor. `None` before
/// migration 008 (no floor exists yet).
fn assess(conn: &Connection) -> Result<Option<Observation>, Error> {
    if !has_time_floor(conn)? {
        return Ok(None);
    }
    let stored: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT wall_ms,boot,boot_ms FROM time_floor WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    // A ledger without a floor yet is seeded no earlier than its recorded history.
    let floor = match &stored {
        Some(f) => Some(f.clone()),
        None => conn
            .query_row("SELECT MAX(at) FROM event", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .map(|at| (at, String::new(), 0)),
    };
    let boot = crate::host::boot_id().ok();
    let boot_ms = crate::host::boot_ms().ok();
    let wall = now_ms().ok();
    let floor_wall = floor.as_ref().map(|f| f.0);
    let same_boot = matches!((&floor, &boot), (Some((_, b, _)), Some(c)) if b == c);
    // Conservative lower bound on the true wall time: the floor, plus the boot-local time
    // that has passed since it was taken when that is comparable (same boot).
    let lower = match (&floor, boot_ms) {
        (Some((w, _, m)), Some(n)) if same_boot => {
            w.saturating_add((i64::try_from(n).unwrap_or(i64::MAX) - m).max(0))
        }
        (Some((w, _, _)), _) => *w,
        (None, _) => i64::MIN,
    };
    let unreadable = |why| Observation {
        verdict: AuthorityTime::Uncertain {
            floor: floor_wall.unwrap_or(0),
            why,
        },
        anomaly: Some(("unreadable", wall, floor_wall.unwrap_or(0))),
        advance: None,
        boot: boot.clone(),
        boot_ms,
    };
    let Some(w) = wall else {
        return Ok(Some(unreadable("wall clock unreadable")));
    };
    let (Some(b), Some(bms)) = (boot.clone(), boot_ms) else {
        return Ok(Some(unreadable("boot clock unreadable")));
    };
    if w.saturating_add(REGRESSION_TOLERANCE_MS) < lower {
        let f = floor_wall.unwrap_or(0);
        return Ok(Some(Observation {
            verdict: AuthorityTime::Uncertain {
                floor: f,
                why: "wall clock behind the durable time floor",
            },
            anomaly: Some(("regression", Some(w), f)),
            // A seed taken from history is stored even on regression; a floor is never lowered.
            advance: stored.is_none().then(|| (f, b.clone(), bms)),
            boot: Some(b),
            boot_ms: Some(bms),
        }));
    }
    let jump = same_boot && w > lower.saturating_add(JUMP_MS);
    let advance = match &stored {
        None => Some((w.max(floor_wall.unwrap_or(w)), b.clone(), bms)),
        Some((fw, fb, _)) if *fb != b || w >= fw.saturating_add(FLOOR_STEP_MS) => {
            Some((w.max(*fw), b.clone(), bms))
        }
        Some(_) => None,
    };
    // Inside the regression tolerance the wall may still read below the conservative lower
    // bound (the floor, plus same-boot elapsed time). Authority is decided against the larger
    // of the two, so a sub-second step back can never re-open a deadline the ledger already
    // observed as passed.
    Ok(Some(Observation {
        verdict: AuthorityTime::Known(w.max(lower)),
        anomaly: jump.then(|| ("jump", Some(w), floor_wall.unwrap_or(0))),
        advance,
        boot: Some(b),
        boot_ms: Some(bms),
    }))
}

/// Records an observation: the anomaly (once per kind and floor) and the floor advance.
fn record(conn: &Connection, o: &Observation) -> Result<(), Error> {
    let Some(epoch) = conn
        .query_row("SELECT epoch FROM coordinator_epoch", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
    else {
        return Ok(());
    };
    if let Some((kind, observed, floor)) = o.anomaly {
        let boot = o.boot.as_deref().unwrap_or("unknown");
        let seen: i64 = conn.query_row(
            "SELECT count(*) FROM time_anomaly WHERE seq=(SELECT MAX(seq) FROM time_anomaly) AND kind=?1 AND floor_wall=?2 AND boot=?3",
            params![kind, floor, boot],
            |r| r.get(0),
        )?;
        if seen == 0 {
            conn.execute(
                "INSERT INTO time_anomaly(kind,epoch,boot,observed_wall,floor_wall,at_boot_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![kind, epoch, boot, observed, floor, o.boot_ms.map(|m| m as i64).unwrap_or(0)],
            )?;
        }
    }
    if let Some((wall, boot, bms)) = &o.advance {
        conn.execute(
            "INSERT INTO time_floor(singleton,wall_ms,boot,boot_ms,epoch) VALUES(1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET wall_ms=excluded.wall_ms,boot=excluded.boot,boot_ms=excluded.boot_ms,epoch=excluded.epoch WHERE excluded.wall_ms>=time_floor.wall_ms",
            params![wall, boot, *bms as i64, epoch],
        )?;
    }
    Ok(())
}

/// The time an authority decision uses, checked against the durable floor. A regression,
/// forward jump or unreadable clock is recorded and the floor advanced: inside an open
/// transaction the records join it, otherwise they commit in their own IMMEDIATE
/// transaction. Never lowers the floor; never turns an uncertain time into a valid one.
pub fn authority_time(conn: &Connection) -> Result<AuthorityTime, Error> {
    let Some(o) = assess(conn)? else {
        return Ok(match now_ms() {
            Ok(n) => AuthorityTime::Known(n),
            Err(_) => AuthorityTime::Uncertain {
                floor: 0,
                why: "wall clock unreadable",
            },
        });
    };
    if o.anomaly.is_none() && o.advance.is_none() {
        return Ok(o.verdict);
    }
    if !conn.is_autocommit() {
        record(conn, &o)?;
        return Ok(o.verdict);
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| {
        let o = assess(conn)?.ok_or(Error::Invalid("time floor vanished"))?;
        record(conn, &o)?;
        Ok(o.verdict)
    })();
    match r {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Read-only verdict (for `diagnose`): the same assessment, nothing written.
pub fn authority_verdict(conn: &Connection) -> Result<AuthorityTime, Error> {
    Ok(match assess(conn)? {
        Some(o) => o.verdict,
        None => match now_ms() {
            Ok(n) => AuthorityTime::Known(n),
            Err(_) => AuthorityTime::Uncertain {
                floor: 0,
                why: "wall clock unreadable",
            },
        },
    })
}

/// The clock evidence freshness is judged against: the authority time, read-only. Under an
/// uncertain time no evidence is fresh, so a rollback can never make stale evidence count
/// again for closure, Task 3 status or revalidation.
pub fn freshness_clock(conn: &Connection) -> Result<i64, Error> {
    Ok(authority_verdict(conn)?.known().unwrap_or(i64::MAX / 2))
}

/// A record stamp that survives an unreadable wall clock: the wall time, else the durable
/// floor, else 0. Used only to stamp lifecycle facts, never to authorize.
pub fn stamp_ms(conn: &Connection) -> i64 {
    now_ms().unwrap_or_else(|_| {
        conn.query_row(
            "SELECT wall_ms FROM time_floor WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    })
}
fn validate_ancestors(root: &Path) -> Result<(), Error> {
    let full = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    for p in full.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error::Invalid("symlink state path"));
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(crate) fn validate_root(root: &Path) -> Result<(), Error> {
    validate_ancestors(root)?;
    let m = fs::symlink_metadata(root)?;
    if !m.is_dir() || m.file_type().is_symlink() || m.mode() & 0o077 != 0 {
        return Err(Error::Invalid(
            "state root must be a private real directory (0700)",
        ));
    }
    // Refuse symlink traversal in every existing ancestor, not just the leaf.
    let full = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    for p in full.ancestors() {
        if fs::symlink_metadata(p)?.file_type().is_symlink() {
            return Err(Error::Invalid("symlink state path"));
        }
    }
    for name in [
        "ledger.db",
        "coordinator.lock",
        "ledger.db-wal",
        "ledger.db-shm",
        "owner.key",
        crate::recovery::EMERGENCY_LOG,
    ] {
        let p = root.join(name);
        match fs::symlink_metadata(&p) {
            Ok(m) if !m.is_file() || m.file_type().is_symlink() || m.nlink() != 1 => {
                return Err(Error::Invalid("unsafe state file"));
            }
            Ok(m) if m.mode() & 0o077 != 0 => {
                return Err(Error::Invalid("state files must be private"));
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn lock(root: &Path) -> Result<File, Error> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("coordinator.lock"))?;
    f.try_lock_exclusive().map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock {
            Error::Owned
        } else {
            Error::Io(e)
        }
    })?;
    Ok(f)
}
/// The startup steps up to and including the epoch commit: additive migration, read-only
/// admission and the epoch transaction (new epoch, owner session, a 'started' recovery run).
/// Ordinary startup runs it on the ledger; `diagnose` runs it on a private in-memory copy to
/// find a precondition that would make every startup fail.
pub(crate) fn begin_startup(
    conn: &mut Connection,
    trigger: crate::recovery::Trigger,
) -> Result<(i64, crate::recovery::Run, bool), Error> {
    migrate(conn)?;
    // Read-only: refuse a restored (inspect-only) ledger and, when earlier startups never
    // finished, ordinary startup, before any startup write.
    crate::recovery::admit(conn, trigger)?;
    let restored = crate::recovery::is_restored(conn)?;
    // An unreadable wall clock cannot stamp the epoch or the recovery run, so startup is
    // refused; the unreadable clock is first recorded durably (its own transaction, under the
    // existing epoch) so the refusal is visible in diagnose.
    if let Err(e) = now_ms() {
        let _ = authority_time(conn);
        return Err(e);
    }
    let boot = crate::host::boot_id()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // The boot the previous owner session ran under, before this startup adds its own.
    let previous_boot: Option<String> = tx
        .query_row(
            "SELECT boot FROM owner_session ORDER BY epoch DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let old: i64 = tx.query_row(
        "SELECT COALESCE(MAX(epoch),0) FROM coordinator_epoch",
        [],
        |r| r.get(0),
    )?;
    let epoch = old.checked_add(1).ok_or(Error::Invalid("epoch overflow"))?;
    tx.execute("INSERT INTO coordinator_epoch VALUES(1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET epoch=excluded.epoch,instance_id=excluded.instance_id,boot_id=excluded.boot_id,committed_heartbeat_at=excluded.committed_heartbeat_at",params![epoch,Uuid::new_v4().to_string(),boot,now_ms()?])?;
    record_boot(&tx, epoch, previous_boot.as_deref(), &boot)?;
    // Durable time floor: seeded, advanced, or the regression/jump/unreadable clock recorded,
    // in the epoch transaction. Startup authorizes nothing; it fails on time only when the wall
    // clock is unreadable (refused above, with the anomaly recorded).
    authority_time(&tx)?;
    let identity = crate::process_scope::Identity::current()?;
    tx.execute(
        "INSERT INTO owner_session VALUES(?1,?2,?3,?4)",
        params![epoch, identity.pid, identity.start, identity.boot],
    )?;
    // The recovery pass is recorded in the same transaction as the epoch it runs under, so
    // every startup that changed anything is accounted. It finishes exactly once; a pass that
    // dies stays 'started' and the next one marks it interrupted.
    let run = crate::recovery::begin_in(&tx, epoch, trigger)?;
    crate::storage::commit_point(tx, crate::storage::Point::Epoch)?;
    Ok((epoch, run, restored))
}
/// Durably records a boot change relative to the previous owner session (and the first boot a
/// ledger is started under), in the epoch transaction. Append-only; never inferred from PIDs.
fn record_boot(
    tx: &Connection,
    epoch: i64,
    previous: Option<&str>,
    current: &str,
) -> Result<(), Error> {
    let last: Option<String> = tx
        .query_row(
            "SELECT current_boot FROM host_boot ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let changed = previous.is_some_and(|p| p != current);
    // Same boot as the previous owner session and already baselined: nothing to record.
    if !changed && last.is_some() {
        return Ok(());
    }
    let at = now_ms().ok();
    tx.execute(
        "INSERT INTO host_boot(detected_epoch,previous_boot,current_boot,wall_at) VALUES(?1,?2,?3,?4)",
        params![epoch, if changed { previous } else { None }, current, at],
    )?;
    if changed {
        tx.execute(
            "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(NULL,'coordinator','host.rebooted',?1,?2,?3)",
            params![
                current,
                at.unwrap_or(0),
                serde_json::json!({"previous_boot": previous, "current_boot": current, "epoch": epoch,
                                   "meaning": "every process identity recorded under the previous boot is stale: not live, never signalled; missing exits stay gaps"})
                    .to_string()
            ],
        )?;
    }
    Ok(())
}
/// Full `PRAGMA integrity_check` (index content against table rows, not only structure),
/// output capped at 20 lines. `["ok"]` when the database is intact.
pub(crate) fn integrity(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut rows = Vec::new();
    let r = (|| -> rusqlite::Result<()> {
        let mut s = conn.prepare("PRAGMA integrity_check(20)")?;
        let mut q = s.query([])?;
        while let Some(row) = q.next()? {
            rows.push(row.get::<_, String>(0)?);
        }
        Ok(())
    })();
    match r {
        Ok(()) => Ok(rows),
        Err(rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            Err(Error::Corrupt)
        }
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn check(conn: &Connection) -> Result<(), Error> {
    // The full check, not quick_check: an index whose content disagrees with its table rows
    // (which quick_check does not compare) would otherwise admit mutation, for example a
    // second acceptance of the same request key through a damaged UNIQUE index.
    // A damaged page can make the check itself raise SQLITE_CORRUPT; that is a failed
    // integrity check of a readable database, not an unavailable one. NOTADB stays an error.
    let integrity: String = match conn.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0)) {
        Ok(s) => s,
        Err(rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            return Err(Error::Corrupt);
        }
        Err(e) => return Err(e.into()),
    };
    if integrity != "ok" {
        return Err(Error::Corrupt);
    }
    let version: i64 = conn.query_row(
        "SELECT version FROM schema_meta WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if !(1..=CURRENT_SCHEMA).contains(&version) {
        return Err(Error::Schema(version));
    }
    let mut fk = conn.prepare("PRAGMA foreign_key_check")?;
    if fk.query([])?.next()?.is_some() {
        return Err(Error::Corrupt);
    }
    Ok(())
}
/// Refuse, without opening anything, a ledger SQLite would damage by opening: an empty main
/// file beside a WAL makes SQLite discard the WAL, which may hold committed transactions, and
/// a rollback journal beside it (in any form) would be played back into the main file by any
/// ordinary open, or block the open if it is not a regular file.
pub(crate) fn preopen(root: &Path) -> Result<(), Error> {
    if fs::symlink_metadata(root.join(crate::restore::MANIFEST)).is_ok() {
        return Err(Error::Restored(
            "this directory is a backup, not a state root; restore it into a separate target"
                .into(),
        ));
    }
    refuse_rollback_journal(&root.join("ledger.db"))?;
    match fs::symlink_metadata(root.join("ledger.db")) {
        Ok(m)
            if m.is_file()
                && m.len() == 0
                && fs::symlink_metadata(root.join("ledger.db-wal")).is_ok() =>
        {
            Err(Error::Unreadable(
                "the main database file is empty beside a WAL; refused without opening so the WAL is preserved",
            ))
        }
        _ => Ok(()),
    }
}
/// An Asher ledger is always in WAL mode, so a `-journal` beside a database is foreign (or left
/// by a crash before WAL mode was set). Any ordinary open would roll it back into the main
/// file; refuse instead, leaving both untouched. Anything other than a confirmed absence
/// (including an unreadable directory entry) refuses.
pub(crate) fn refuse_rollback_journal(db: &Path) -> Result<(), Error> {
    let mut j = db.as_os_str().to_owned();
    j.push("-journal");
    match fs::symlink_metadata(Path::new(&j)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error::Unreadable(
            "a rollback journal is beside the database; SQLite would play it back into the main file on open, so it is refused unopened and both files are preserved",
        )),
    }
}
/// `file:` URI for a path (percent-encoded), for SQLite URI parameters.
pub(crate) fn sqlite_uri(p: &Path) -> String {
    let mut s = String::from("file:");
    for b in p.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(b) {
            s.push(*b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}
/// Read-only view that takes no lock and never creates a WAL or shared-memory file. Only
/// for a database with no WAL beside it (an immutable view ignores a WAL).
pub(crate) fn open_immutable(path: &Path) -> Result<Connection, Error> {
    Ok(Connection::open_with_flags(
        format!("{}?mode=ro&immutable=1", sqlite_uri(path)),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}
pub(crate) fn configure(conn: &Connection) -> Result<(), Error> {
    conn.busy_timeout(Duration::from_millis(1000))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let fk: i64 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let sync: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    if fk != 1 || journal != "wal" || sync != 2 {
        return Err(Error::Invalid("SQLite configuration unavailable"));
    }
    Ok(())
}
fn work_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Work> {
    Ok(Work {
        id: r.get(0)?,
        principal: r.get(1)?,
        request_key: r.get(2)?,
        request_hash: r.get(3)?,
        permit_id: r.get(4)?,
        mode: r.get(5)?,
        phase: r.get(6)?,
        version: r.get(7)?,
        next_owner: r.get(8)?,
        next_action: r.get(9)?,
        due_at: r.get(10)?,
        accepted_at: r.get(11)?,
    })
}
impl Owner {
    /// Creates a fresh state directory; never reinitializes an existing or corrupt ledger.
    pub fn initialize(root: &Path) -> Result<Self, Error> {
        validate_ancestors(root)?;
        fs::DirBuilder::new().mode(0o700).create(root)?;
        let f = lock(root)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("ledger.db"))?
            .sync_all()?;
        crate::storage::prepare();
        let mut conn =
            Connection::open_with_flags(root.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        configure(&conn)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        File::open(root)?.sync_all()?;
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        Self::from_connection(root, conn, f, crate::recovery::Trigger::Startup)
    }
    /// Ordinary mutating startup. Refused without writing when the ledger is corrupt,
    /// incompatible or unverifiable, or when the startup breaker is open (see `recovery`).
    pub fn open(root: &Path) -> Result<Self, Error> {
        Self::open_as(root, crate::recovery::Trigger::Startup)
    }
    /// The same startup, explicitly requested by the owner (`recover`): one bounded pass
    /// that is not subject to the startup breaker. It never dispatches, launches, notifies,
    /// closes or repairs anything that ordinary startup would not.
    pub fn open_for_recovery(root: &Path) -> Result<Self, Error> {
        Self::open_as(root, crate::recovery::Trigger::Explicit)
    }
    fn open_as(root: &Path, trigger: crate::recovery::Trigger) -> Result<Self, Error> {
        validate_root(root)?;
        preopen(root)?;
        // A missing ledger is never created, and no lock file is left where there is none. A
        // WAL without its main file may hold committed transactions: preserve it.
        if !fs::symlink_metadata(root.join("ledger.db")).is_ok_and(|m| m.is_file())
            && fs::symlink_metadata(root.join("ledger.db-wal")).is_ok()
        {
            return Err(Error::Unreadable(
                "ledger.db is missing beside a WAL that may hold committed transactions; nothing was opened or created, and the WAL is preserved",
            ));
        }
        if !fs::symlink_metadata(root.join("ledger.db")).is_ok_and(|m| m.is_file()) {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "no ledger.db in this state root; nothing was created",
            )));
        }
        crate::storage::prepare();
        // With no WAL beside the ledger (so no writer is active), verify and admit it through
        // an immutable read-only view before taking the lock, so a refusal leaves no new file
        // behind (a read-write open creates a WAL; the lock may create its file).
        if fs::symlink_metadata(root.join("ledger.db-wal")).is_err() {
            let c = open_immutable(&root.join("ledger.db"))?;
            check(&c)?;
            crate::recovery::admit(&c, trigger)?;
        }
        let f = lock(root)?;
        let conn =
            Connection::open_with_flags(root.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // Until the ledger is verified and startup admitted, closing this connection must not
        // checkpoint the WAL into the main file or delete it: a refused ledger keeps its
        // database and WAL byte for byte for investigation.
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        // Validate before changing journal settings or touching epoch on an unknown schema.
        check(&conn)?;
        configure(&conn)?;
        Self::from_connection(root, conn, f, trigger)
    }
    fn from_connection(
        root: &Path,
        mut conn: Connection,
        f: File,
        trigger: crate::recovery::Trigger,
    ) -> Result<Self, Error> {
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        check(&conn)?;
        let from = crate::recovery::schema_version(&conn)?;
        // The ledger is verified from here on: a failure before the epoch transaction commits
        // is a startup precondition of an accessible ledger, not an unavailable database.
        let (epoch, run, restored) = match begin_startup(&mut conn, trigger) {
            Ok(x) => x,
            Err(e @ (Error::StartupBlocked(_) | Error::Restored(_))) => return Err(e),
            Err(e) => {
                let written = match (&e, crate::recovery::schema_version(&conn)) {
                    // A failed COMMIT may have landed: never report it as nothing written.
                    (Error::CommitUncertain(..), Ok(v)) => {
                        Written::Unknown((v != from).then_some(from))
                    }
                    (Error::CommitUncertain(..), Err(_)) => Written::Unknown(None),
                    (_, Ok(v)) if v != from => Written::Migrated(from),
                    (_, Ok(_)) => Written::Nothing,
                    // No migration was pending, so nothing before the epoch transaction can
                    // have committed.
                    (_, Err(_)) if from >= CURRENT_SCHEMA => Written::Nothing,
                    // A failed read establishes nothing: the migration may have committed.
                    (_, Err(_)) => Written::MigrationUnknown(from),
                };
                return Err(Error::StartupPrecondition(Box::new(e), written));
            }
        };
        // Startup has begun: closing may now checkpoint as usual.
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            false,
        )?;
        let mut owner = Self {
            conn,
            _lock: f,
            epoch,
            root: root.to_path_buf(),
            recovery: None,
        };
        let steps = (|| -> Result<serde_json::Value, Error> {
            // A restored ledger is evidence of its backup: its rows are never reconciled into
            // new facts (that could, for example, record an applied effect as never
            // dispatched). Only the recovery accounting below runs on it.
            if !restored {
                owner.bootstrap_owner_key()?;
                crate::effect::reconcile_legacy_quarantine(&mut owner.conn)?;
                owner.reconcile()?;
            }
            crate::recovery::finish(&mut owner.conn, &owner.root, &run)
        })();
        match steps {
            Ok(summary) => {
                owner.recovery = Some(summary);
                Ok(owner)
            }
            Err(e) => {
                let recorded = crate::recovery::fail(&mut owner.conn, &run, &e);
                Err(Error::StartupFailed(Box::new(e), recorded))
            }
        }
    }
    /// Summary of the recovery pass this startup recorded.
    pub fn recovery_summary(&self) -> Option<&serde_json::Value> {
        self.recovery.as_ref()
    }
    pub fn add_permit(
        &mut self,
        id: &str,
        scope: &PermitScope,
        expires_at: i64,
    ) -> Result<(), Error> {
        scope.validate()?;
        let body = serde_json::to_string(scope)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authority_time(&tx)?
            .known()
            .ok_or(Error::Invalid("time uncertain; no permit is issued"))?;
        if !valid_id(id) || expires_at <= now || expires_at > now.saturating_add(86_400_000) {
            return Err(Error::Invalid("permit must expire within 24 hours"));
        }
        tx.execute(
            "INSERT INTO permit VALUES(?1,1,'local-owner',?2,?3,?4,?5,NULL)",
            params![id, fingerprint(&body), body, now, expires_at],
        )?;
        tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(NULL,'local-owner','permit.created',?1,?2,?3)",params![id,now,body])?;
        tx.commit()?;
        Ok(())
    }
    pub fn revoke_permit(&mut self, id: &str) -> Result<(), Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        if tx.execute(
            "UPDATE permit SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
            params![now, id],
        )? != 1
        {
            return Err(Error::Permit);
        }
        tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(NULL,'local-owner','permit.revoked',?1,?2,'{}')",params![id,now])?;
        tx.commit()?;
        Ok(())
    }
    /// T1. A failure is classified by the stage reached (`storage`); the request key is the
    /// stable ID for read back, and resubmitting the same body is idempotent.
    pub fn submit(&mut self, req: &SubmitWork) -> Result<Receipt, Error> {
        let mut g = crate::storage::Guard::new(crate::storage::Boundary::T1, &req.request_key);
        let r = self.submit_in(req, &mut g);
        g.finish(r)
    }
    fn submit_in(
        &mut self,
        req: &SubmitWork,
        g: &mut crate::storage::Guard,
    ) -> Result<Receipt, Error> {
        let body = req.canonical()?;
        let hash = fingerprint(&body);
        barrier("before_accept_begin")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let t = authority_time(&tx)?;
        let now = t.stamp();
        let sql =
            format!("SELECT {COLUMNS} FROM work WHERE principal='local-owner' AND request_key=?1");
        if let Some(w) = tx
            .query_row(&sql, [&req.request_key], work_row)
            .optional()?
        {
            if w.request_hash != hash {
                tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'local-owner','request.conflict',?2,?3,?4)",params![w.id,req.request_key,now,serde_json::json!({"submitted_hash":hash}).to_string()])?;
                tx.commit()?;
                return Err(Error::Conflict);
            }
            tx.commit()?;
            return Ok(Receipt {
                disposition: Disposition::Existing,
                work: w,
            });
        }
        let permit: Option<(String, i64, i64, Option<i64>)> = tx
            .query_row(
                "SELECT scope_json,issued_at,expires_at,revoked_at FROM permit WHERE id=?1",
                [&req.permit_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (scope, issued, expires, revoked) = permit.ok_or(Error::Permit)?;
        let scope: PermitScope = serde_json::from_str(&scope)?;
        scope.validate()?;
        if revoked.is_some()
            || t.before(issued)
            || t.reached(expires)
            || scope.fixture_id != req.fixture_id
        {
            return Err(Error::Permit);
        }
        let w = Work {
            id: Uuid::new_v4().to_string(),
            principal: "local-owner".into(),
            request_key: req.request_key.clone(),
            request_hash: hash,
            permit_id: req.permit_id.clone(),
            mode: "active".into(),
            phase: "queued".into(),
            version: 1,
            next_owner: "coordinator".into(),
            next_action: "prepare_fixture".into(),
            due_at: now
                .checked_add(10000)
                .ok_or(Error::Invalid("deadline overflow"))?,
            accepted_at: now,
        };
        tx.execute("INSERT INTO work(id,principal,request_key,request_hash,request_json,permit_id,mode,phase,version,next_owner,next_action,due_at,accepted_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![w.id,w.principal,w.request_key,w.request_hash,body,w.permit_id,w.mode,w.phase,w.version,w.next_owner,w.next_action,w.due_at,w.accepted_at])?;
        tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'local-owner','work.accepted',?1,?2,?3)",params![w.id,now,serde_json::to_string(&w)?])?;
        barrier("before_accept_commit")?;
        g.commit(tx)?;
        barrier("after_accept_commit")?;
        Ok(Receipt {
            disposition: Disposition::Accepted,
            work: w,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}
impl Reader {
    pub fn open(root: &Path) -> Result<Self, Error> {
        validate_root(root)?;
        preopen(root)?;
        crate::storage::prepare();
        // With no WAL beside the ledger, verify through an immutable view first, so a refused
        // ledger gains no file (an ordinary read-only open of a WAL-mode ledger creates its WAL
        // and shared-memory index).
        if fs::symlink_metadata(root.join("ledger.db-wal")).is_err() {
            check(&open_immutable(&root.join("ledger.db"))?)?;
        }
        let conn =
            Connection::open_with_flags(root.join("ledger.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // A reader never checkpoints or removes the WAL when it closes.
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        conn.busy_timeout(Duration::from_millis(1000))?;
        check(&conn)?;
        Ok(Self { conn })
    }
    /// The verified read-only connection, for lock-free inspection (`attempts`).
    pub(crate) fn into_conn(self) -> Connection {
        self.conn
    }
    pub fn get(&self, key: &str) -> Result<Option<Work>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM work WHERE principal='local-owner' AND request_key=?1"
                ),
                [key],
                work_row,
            )
            .optional()?)
    }
    pub fn list(&self) -> Result<Vec<Work>, Error> {
        let mut s = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM work ORDER BY accepted_at,id"
        ))?;
        Ok(s.query_map([], work_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
    /// The verified read-only connection, for stable-ID read back (`storage::readback`).
    pub fn conn_for_readback(&self) -> &Connection {
        &self.conn
    }
    pub fn assert_accounted(&self) -> Result<(), Error> {
        if self.list()?.iter().any(|w| !w.is_accounted()) {
            return Err(Error::Invalid("unaccounted accepted work"));
        }
        // Closed work must carry its supported outcome (schema 6 and later).
        let v: i64 = self
            .conn
            .query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
        if v >= 6 {
            let orphan: i64 = self.conn.query_row(
                "SELECT count(*) FROM work w WHERE w.mode='closed' AND NOT EXISTS (SELECT 1 FROM work_outcome o WHERE o.work_id=w.id)",
                [],
                |r| r.get(0),
            )?;
            if orphan > 0 {
                return Err(Error::Invalid("closed work without a recorded outcome"));
            }
        }
        Ok(())
    }
}
#[cfg(not(feature = "fault-injection"))]
pub(crate) fn barrier(_: &str) -> Result<(), Error> {
    Ok(())
}
/// Test-only append-only executor journal (counts ref-update executions). No-op in release.
#[cfg(not(feature = "fault-injection"))]
pub(crate) fn journal(_: &str) {}
#[cfg(feature = "fault-injection")]
pub(crate) fn journal(line: &str) {
    use std::io::Write;
    if let Some(p) = std::env::var_os("ASHER_TEST_EFFECT_JOURNAL") {
        if let Ok(mut f) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(p)
        {
            let _ = writeln!(f, "{line}");
            let _ = f.sync_all();
        }
    }
}
#[cfg(feature = "fault-injection")]
pub(crate) fn barrier(name: &str) -> Result<(), Error> {
    if std::env::var("ASHER_TEST_BARRIER").ok().as_deref() != Some(name) {
        return Ok(());
    }
    let dir = PathBuf::from(
        std::env::var_os("ASHER_TEST_BARRIER_DIR")
            .ok_or(Error::Invalid("missing test barrier directory"))?,
    );
    let marker = dir.join(name);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker)?
        .sync_all()?;
    // Bounded handshake: parent kills child or releases it. Never an unbounded test wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !dir.join("release").exists() {
        if std::time::Instant::now() >= deadline {
            return Err(Error::Invalid("test barrier timed out"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<(), Error> {
    // Each supported step is one transaction; a failed step leaves the prior version intact.
    loop {
        let version: i64 = conn.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
        let step = match version {
            CURRENT_SCHEMA => return Ok(()),
            1 => include_str!("../migrations/002.sql"),
            2 => include_str!("../migrations/003.sql"),
            3 => include_str!("../migrations/004.sql"),
            4 => include_str!("../migrations/005.sql"),
            5 => include_str!("../migrations/006.sql"),
            6 => include_str!("../migrations/007.sql"),
            7 => include_str!("../migrations/008.sql"),
            v => return Err(Error::Schema(v)),
        };
        conn.pragma_update(None, "foreign_keys", false)?;
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(step)?;
            {
                let mut q = tx.prepare("PRAGMA foreign_key_check")?;
                if q.query([])?.next()?.is_some() {
                    return Err(Error::Corrupt);
                }
            }
            let after: i64 = tx.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
            if after != version + 1 {
                return Err(Error::Schema(after));
            }
            crate::storage::commit_point(tx, crate::storage::Point::Migrate)?;
            Ok(())
        })();
        conn.pragma_update(None, "foreign_keys", true)?;
        result?;
        crate::store::barrier("after_migration_commit")?;
    }
}

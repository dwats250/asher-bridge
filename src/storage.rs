//! Storage-fault classification at the documented transaction boundaries
//! (`architecture/04-storage.md`): T1 accept, T4 create review, T5 decide, T6 claim effect,
//! T7 record effect, S1 supervisor accepts launch, S2 supervisor records a lifecycle fact.
//!
//! A boundary's failure is classified by the stage the caller had reached, never by a later
//! readback:
//!
//! * before COMMIT was issued (including `BEGIN IMMEDIATE` busy): this call's transaction
//!   did not commit (`not_committed`); SQLite never makes an uncommitted transaction durable;
//! * COMMIT itself failed: busy, locked or a deferred constraint means SQLite rolled it back
//!   (`not_committed`); any other failure (full, I/O, corruption...) leaves the outcome
//!   `unknown`: the commit record may already be in the WAL;
//! * COMMIT returned OK and something failed afterwards: `committed`; only the response was
//!   lost or incomplete.
//!
//! `not_committed` speaks only for this call: an earlier call with the same stable ID may have
//! committed. Every boundary is therefore idempotent by its stable ID or guarded so that its
//! consequential step runs only after a confirmed commit; an `unknown` outcome is resolved by a
//! read back of that ID or by ordinary reconciliation, never by repeating consequential work.
use crate::Error;
use rusqlite::{ErrorCode, Transaction};
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Boundary {
    T1,
    T4,
    T5,
    T6,
    T7,
    S1,
    S2,
}
impl Boundary {
    pub const ALL: [Boundary; 7] = [
        Boundary::T1,
        Boundary::T4,
        Boundary::T5,
        Boundary::T6,
        Boundary::T7,
        Boundary::S1,
        Boundary::S2,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Boundary::T1 => "T1",
            Boundary::T4 => "T4",
            Boundary::T5 => "T5",
            Boundary::T6 => "T6",
            Boundary::T7 => "T7",
            Boundary::S1 => "S1",
            Boundary::S2 => "S2",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.as_str() == s)
    }
    /// Nonzero index used by the test-only fault VFS.
    #[cfg_attr(not(feature = "fault-injection"), allow(dead_code))]
    pub(crate) fn index(self) -> u8 {
        Self::ALL.iter().position(|b| *b == self).unwrap_or(0) as u8 + 1
    }
    pub fn operation(self) -> &'static str {
        match self {
            Boundary::T1 => "accept work",
            Boundary::T4 => "create review",
            Boundary::T5 => "record owner decision",
            Boundary::T6 => "claim effect for dispatch",
            Boundary::T7 => "record effect result",
            Boundary::S1 => "supervisor accepts launch",
            Boundary::S2 => "supervisor records lifecycle fact",
        }
    }
    /// The stable identity whose presence a read back checks.
    pub fn stable_id_kind(self) -> &'static str {
        match self {
            Boundary::T1 => "request_key",
            // The review ID minted before COMMIT: an attempt can have several reviews over
            // time, so only the review's own ID identifies one creation.
            Boundary::T4 => "review_id",
            Boundary::T5 => "decision_id",
            Boundary::T6 | Boundary::T7 => "effect_id",
            Boundary::S1 | Boundary::S2 => "attempt_id",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    BeforeCommit,
    Commit,
    AfterCommit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// This call's transaction did not commit (a prior call with the same ID may have).
    NotCommitted,
    /// The commit outcome is not established; read back the stable ID.
    Unknown,
    /// The transaction committed; a later step failed.
    Committed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Busy,
    Full,
    IoErr,
    Corrupt,
    ReadOnly,
    CantOpen,
    Constraint,
    PostCommit,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageFault {
    pub boundary: Boundary,
    pub operation: &'static str,
    /// Which commit of the boundary: T7 `executor_result` or `result_record`; S2 the fact kind.
    pub step: String,
    /// Boundaries this call had already committed before this fault (e.g. T6 for a T7 fault).
    pub committed_before: Vec<Boundary>,
    pub stage: Stage,
    pub durability: Durability,
    pub kind: FaultKind,
    pub sqlite_code: Option<i32>,
    pub stable_id_kind: &'static str,
    pub stable_id: String,
    pub detail: String,
    /// Never true: classification does not authorize any repetition.
    pub retry_consequential_work: bool,
}
impl StorageFault {
    pub fn next_step(&self) -> &'static str {
        match self.durability {
            Durability::NotCommitted => {
                "this boundary's transaction did not commit (what this call's startup committed, if anything, is unaffected); an earlier call with the same identity may have: retry only with the same identity"
            }
            Durability::Unknown => {
                "commit outcome unknown: do not assume it failed. A read back that does not show the stable ID is not evidence: a commit whose WAL sync failed can stay invisible while any other connection holds the ledger and appear later. `orchestrator recover STATE` settles it from the emergency record under the coordinator lock; otherwise retry only with the same identity. Never repeat consequential work to find out"
            }
            Durability::Committed => {
                "committed; only the response or a later step failed: read back the stable ID"
            }
        }
    }
}
impl fmt::Display for StorageFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match self.durability {
            Durability::NotCommitted => "not committed",
            Durability::Unknown => "commit outcome unknown",
            Durability::Committed => "committed, response lost",
        };
        write!(
            f,
            "storage fault at {} ({}) {}: {} ({:?}); {} {}: {}",
            self.boundary.as_str(),
            self.operation,
            match self.stage {
                Stage::BeforeCommit => "before commit",
                Stage::Commit => "during commit",
                Stage::AfterCommit => "after commit",
            },
            outcome,
            self.kind,
            self.stable_id_kind,
            self.stable_id,
            self.next_step()
        )
    }
}

/// The storage class of a SQLite error, if it is one.
pub fn kind_of(e: &rusqlite::Error) -> Option<(FaultKind, i32)> {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => Some((
            match f.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => FaultKind::Busy,
                ErrorCode::DiskFull => FaultKind::Full,
                ErrorCode::SystemIoFailure => FaultKind::IoErr,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => FaultKind::Corrupt,
                ErrorCode::ReadOnly => FaultKind::ReadOnly,
                ErrorCode::CannotOpen => FaultKind::CantOpen,
                ErrorCode::ConstraintViolation => FaultKind::Constraint,
                _ => FaultKind::Other,
            },
            f.extended_code,
        )),
        _ => None,
    }
}

/// Tracks how far one boundary call got, so its failure is classified by stage.
pub(crate) struct Guard {
    boundary: Boundary,
    kind: &'static str,
    stable_id: String,
    step: String,
    stage: Stage,
}
impl Guard {
    pub(crate) fn new(boundary: Boundary, stable_id: impl Into<String>) -> Self {
        prepare();
        Self {
            boundary,
            kind: boundary.stable_id_kind(),
            stable_id: stable_id.into(),
            step: String::new(),
            stage: Stage::BeforeCommit,
        }
    }
    /// Name what the initial stable ID is, when it is not yet the boundary's own identity
    /// (T4 starts from the attempt until its review ID is minted).
    pub(crate) fn identified_by(mut self, kind: &'static str) -> Self {
        self.kind = kind;
        self
    }
    /// Record the boundary's own identity once it exists (T4: the review ID minted before
    /// COMMIT), so an uncertain commit is read back by exactly that identity.
    pub(crate) fn set_identity(&mut self, kind: &'static str, id: &str) {
        self.kind = kind;
        self.stable_id = id.to_string();
    }
    /// Name the commit within the boundary (T7 has two).
    pub(crate) fn step(mut self, step: &str) -> Self {
        self.step = step.to_string();
        self
    }
    /// Record the stable ID once the request has been parsed.
    pub(crate) fn set_id(&mut self, id: &str) {
        self.stable_id = id.to_string();
    }
    /// Commit the boundary's transaction and classify a failure of COMMIT itself.
    pub(crate) fn commit(&mut self, tx: Transaction<'_>) -> Result<(), Error> {
        self.stage = Stage::Commit;
        arm(self.boundary);
        let r = tx.commit();
        disarm();
        match r {
            Ok(()) => {
                self.stage = Stage::AfterCommit;
                Ok(())
            }
            Err(e) => {
                let (kind, code) = kind_of(&e).unwrap_or((FaultKind::Other, 0));
                let durability = match kind {
                    // SQLite rolls back (or leaves open, then rusqlite rolls back) on these.
                    FaultKind::Busy | FaultKind::Constraint => Durability::NotCommitted,
                    _ => Durability::Unknown,
                };
                let f = self.fault(durability, kind, Some(code), e.to_string());
                pause_after_fault();
                Err(f)
            }
        }
    }
    /// Classify the boundary call's result by the stage it reached.
    pub(crate) fn finish<T>(&self, r: Result<T, Error>) -> Result<T, Error> {
        r.map_err(|e| self.classify(e))
    }
    fn classify(&self, e: Error) -> Error {
        if let Error::Storage(mut f) = e {
            // A fault in a later boundary of the same call, after this one committed: the call
            // did commit (this boundary, and possibly its consequential step), so a nested
            // `not_committed` is reported as `committed`, naming the earlier commit.
            if self.stage == Stage::AfterCommit {
                f.committed_before.insert(0, self.boundary);
                if f.durability == Durability::NotCommitted {
                    f.durability = Durability::Committed;
                    f.detail = format!(
                        "{} committed earlier in this call; then {} {} did not commit: {}",
                        self.boundary.as_str(),
                        f.boundary.as_str(),
                        f.step,
                        f.detail
                    );
                }
            }
            return Error::Storage(f);
        }
        match self.stage {
            Stage::BeforeCommit => match &e {
                Error::Sql(s) => match kind_of(s) {
                    Some((
                        k @ (FaultKind::Busy
                        | FaultKind::Full
                        | FaultKind::IoErr
                        | FaultKind::Corrupt
                        | FaultKind::ReadOnly
                        | FaultKind::CantOpen),
                        code,
                    )) => self.fault(Durability::NotCommitted, k, Some(code), s.to_string()),
                    _ => e,
                },
                // Domain refusals before commit are known rejections and keep their meaning.
                _ => e,
            },
            Stage::Commit => e,
            Stage::AfterCommit => {
                let code = match &e {
                    Error::Sql(s) => kind_of(s).map(|k| k.1),
                    _ => None,
                };
                self.fault(
                    Durability::Committed,
                    FaultKind::PostCommit,
                    code,
                    e.to_string(),
                )
            }
        }
    }
    fn fault(
        &self,
        durability: Durability,
        kind: FaultKind,
        code: Option<i32>,
        detail: String,
    ) -> Error {
        Error::Storage(Box::new(StorageFault {
            boundary: self.boundary,
            operation: self.boundary.operation(),
            step: self.step.clone(),
            committed_before: Vec::new(),
            stage: self.stage,
            durability,
            kind,
            sqlite_code: code,
            stable_id_kind: self.kind,
            stable_id: self.stable_id.clone(),
            detail,
            retry_consequential_work: false,
        }))
    }
}

/// Internal commits outside the documented boundaries whose failure must still be classified by
/// the stage reached: the startup epoch transaction, an additive migration step, and the backup
/// copy's snapshot marker. Test builds can fail them like a boundary (`EPOCH`, `MIGRATE`,
/// `BACKUP` in `ASHER_TEST_STORAGE_FAULT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    Epoch,
    Migrate,
    Backup,
}
impl Point {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "EPOCH" => Some(Point::Epoch),
            "MIGRATE" => Some(Point::Migrate),
            "BACKUP" => Some(Point::Backup),
            _ => None,
        }
    }
    #[cfg_attr(not(feature = "fault-injection"), allow(dead_code))]
    pub(crate) fn index(self) -> u8 {
        match self {
            Point::Epoch => 101,
            Point::Migrate => 102,
            Point::Backup => 103,
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Point::Epoch => {
                "the startup epoch transaction (new epoch, owner session and a 'started' recovery run)"
            }
            Point::Migrate => "an additive schema migration step",
            Point::Backup => "the backup copy's snapshot marker",
        }
    }
}
/// Commit an internal transaction. Busy and constraint failures roll back (not committed); any
/// other failure of COMMIT itself leaves the outcome unknown: the commit record may already be
/// in the WAL (`Error::CommitUncertain`).
pub(crate) fn commit_point(tx: Transaction<'_>, p: Point) -> Result<(), Error> {
    prepare();
    arm_index(p.index());
    let r = tx.commit();
    disarm();
    r.map_err(|e| match kind_of(&e) {
        Some((FaultKind::Busy | FaultKind::Constraint, _)) => Error::Sql(e),
        _ => Error::CommitUncertain(p.describe(), Box::new(Error::Sql(e))),
    })
}

/// Result of reading back a boundary's stable ID on a verified connection.
pub enum Readback {
    Present(serde_json::Value),
    Absent(serde_json::Value),
    /// The ID given does not identify one commit (for example an attempt ID for T4, which can
    /// have several reviews over time); never evidence of absence.
    Inconclusive(serde_json::Value),
}
/// Read back whether a boundary's stable ID committed. Read-only. A failure to read back is
/// an error, never `Absent`: an unavailable read back does not establish non-commit.
pub fn readback(conn: &rusqlite::Connection, b: Boundary, id: &str) -> Result<Readback, Error> {
    readback_step(conn, b, "", id)
}
/// As `readback`, for one named commit of the boundary: T7 `executor_result` or
/// `result_record` (the default); S2 a fact kind (default: any fact, which is not specific).
pub fn readback_step(
    conn: &rusqlite::Connection,
    b: Boundary,
    step: &str,
    id: &str,
) -> Result<Readback, Error> {
    use rusqlite::OptionalExtension;
    use serde_json::json;
    let row = |sql: &str| -> Result<Option<serde_json::Value>, Error> {
        let mut s = conn.prepare(sql)?;
        let names: Vec<String> = s.column_names().iter().map(|c| c.to_string()).collect();
        Ok(s.query_row([id], |r| {
            let mut m = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                m.insert(
                    n.clone(),
                    match v {
                        rusqlite::types::Value::Null => json!(null),
                        rusqlite::types::Value::Integer(x) => json!(x),
                        rusqlite::types::Value::Real(x) => json!(x),
                        rusqlite::types::Value::Text(x) => json!(x),
                        rusqlite::types::Value::Blob(_) => json!("<blob>"),
                    },
                );
            }
            Ok(serde_json::Value::Object(m))
        })
        .optional()?)
    };
    let found = match b {
        Boundary::T1 => row(
            "SELECT id AS work_id,version,mode,phase,request_hash FROM work WHERE principal='local-owner' AND request_key=?1",
        )?,
        // T4 is read back by the review ID minted before COMMIT, in any state: a review
        // retired after it committed still committed, and an earlier review of the same
        // attempt never stands for a later creation.
        Boundary::T4 => {
            if let Some(v) = row(
                "SELECT id AS review_id,attempt_id,state,state_reason,created_at FROM review WHERE id=?1",
            )? {
                return Ok(Readback::Present(v));
            }
            let reviews: Vec<serde_json::Value> = {
                let mut s = conn.prepare(
                    "SELECT id,state,created_at FROM review WHERE attempt_id=?1 ORDER BY created_at DESC,id LIMIT 20",
                )?;
                s.query_map([id], |r| {
                    Ok(
                        json!({"review_id": r.get::<_, String>(0)?, "state": r.get::<_, String>(1)?,
                              "created_at": r.get::<_, i64>(2)?}),
                    )
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let attempt: i64 =
                conn.query_row("SELECT count(*) FROM attempt WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
            return Ok(if attempt > 0 {
                Readback::Inconclusive(json!({
                    "attempt_id": id, "reviews_for_attempt": reviews,
                    "reason": "an attempt ID does not identify one review creation; read back the review_id named in the fault record, or rerun review-create, which returns the open review if one exists",
                }))
            } else {
                Readback::Absent(json!({"review": null}))
            });
        }
        Boundary::T5 => row("SELECT review_id,choice,decided_at FROM decision WHERE id=?1")?,
        Boundary::T6 => {
            let r = row("SELECT state,claim_epoch,dispatched_at FROM effect WHERE id=?1")?;
            return Ok(match r {
                Some(v) if !v["claim_epoch"].is_null() => Readback::Present(v),
                Some(v) => Readback::Absent(json!({"effect": v, "claimed": false})),
                None => Readback::Absent(json!({"effect": null})),
            });
        }
        Boundary::T7 => {
            let r = row(
                "SELECT state,result_observation_id,(SELECT count(*) FROM effect_executor_result x WHERE x.effect_id=?1) AS executor_results FROM effect WHERE id=?1",
            )?;
            let present = |v: &serde_json::Value| match step {
                "executor_result" => v["executor_results"].as_i64().unwrap_or(0) > 0,
                _ => !v["result_observation_id"].is_null(),
            };
            return Ok(match r {
                Some(v) if present(&v) => Readback::Present(json!({"step": step, "effect": v})),
                Some(v) => Readback::Absent(json!({"step": step, "effect": v})),
                None => Readback::Absent(json!({"effect": null})),
            });
        }
        Boundary::S1 => row(
            "SELECT request_id,coordinator_epoch,accepted_at FROM supervisor_command WHERE attempt_id=?1",
        )?,
        // Facts carry no individual identity; a kind is the most specific read back.
        Boundary::S2 if !step.is_empty() => {
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM supervisor_fact WHERE attempt_id=?1 AND kind=?2",
                [id, step],
                |r| r.get(0),
            )?;
            let d = json!({"kind": step, "facts_of_kind": n,
                           "note": "facts of a kind are not individually identified"});
            return Ok(if n > 0 {
                Readback::Present(d)
            } else {
                Readback::Absent(d)
            });
        }
        Boundary::S2 => row(
            "SELECT count(*) AS facts,(SELECT kind FROM supervisor_fact y WHERE y.attempt_id=?1 ORDER BY seq DESC LIMIT 1) AS latest_kind FROM supervisor_fact WHERE attempt_id=?1 HAVING count(*)>0",
        )?,
    };
    Ok(match found {
        Some(v) => Readback::Present(v),
        None => Readback::Absent(json!(null)),
    })
}

/// S2-style autocommit statement (no explicit transaction): the statement is its own commit.
pub(crate) fn autocommit<T>(
    boundary: Boundary,
    stable_id: &str,
    step: &str,
    f: impl FnOnce() -> rusqlite::Result<T>,
) -> Result<T, Error> {
    prepare();
    arm(boundary);
    let r = f();
    disarm();
    r.map_err(|e| {
        let (kind, code) = kind_of(&e).unwrap_or((FaultKind::Other, 0));
        let durability = match kind {
            FaultKind::Busy | FaultKind::Constraint => Durability::NotCommitted,
            _ => Durability::Unknown,
        };
        let g = Guard {
            boundary,
            kind: boundary.stable_id_kind(),
            stable_id: stable_id.to_string(),
            step: step.to_string(),
            stage: Stage::Commit,
        };
        g.fault(durability, kind, Some(code), e.to_string())
    })
}

/// Must run before a process opens its first SQLite connection (test VFS registration).
pub(crate) fn prepare() {
    #[cfg(feature = "fault-injection")]
    crate::faultvfs::install();
}
#[cfg(feature = "fault-injection")]
fn arm(b: Boundary) {
    crate::faultvfs::arm(b.index());
}
#[cfg(not(feature = "fault-injection"))]
fn arm(_: Boundary) {}
#[cfg(feature = "fault-injection")]
fn arm_index(i: u8) {
    crate::faultvfs::arm(i);
}
#[cfg(not(feature = "fault-injection"))]
fn arm_index(_: u8) {}
#[cfg(feature = "fault-injection")]
fn disarm() {
    crate::faultvfs::disarm();
}
#[cfg(not(feature = "fault-injection"))]
fn disarm() {}
/// Test-only named point after a classified commit failure, before the connection closes.
#[cfg(feature = "fault-injection")]
fn pause_after_fault() {
    let _ = crate::store::barrier("after_storage_fault");
}
#[cfg(not(feature = "fault-injection"))]
fn pause_after_fault() {}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, TransactionBehavior, ffi};
    fn sqlite(code: i32) -> Error {
        Error::Sql(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None))
    }
    fn fault(e: Result<(), Error>) -> StorageFault {
        match e {
            Err(Error::Storage(f)) => *f,
            other => panic!("expected a classified storage fault, got {other:?}"),
        }
    }

    #[test]
    fn failures_are_classified_by_the_stage_reached() {
        let g = Guard::new(Boundary::T1, "k");
        // Before COMMIT: a storage failure means this call did not commit.
        let f = fault(g.finish(Err(sqlite(ffi::SQLITE_BUSY))));
        assert_eq!(
            (f.stage, f.durability, f.kind),
            (
                Stage::BeforeCommit,
                Durability::NotCommitted,
                FaultKind::Busy
            )
        );
        let f = fault(g.finish(Err(sqlite(ffi::SQLITE_IOERR_READ))));
        assert_eq!(
            (f.durability, f.kind),
            (Durability::NotCommitted, FaultKind::IoErr)
        );
        // Domain refusals and constraint aborts keep their meaning.
        assert!(matches!(
            g.finish(Err::<(), _>(Error::Conflict)),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            g.finish(Err::<(), _>(sqlite(ffi::SQLITE_CONSTRAINT_TRIGGER))),
            Err(Error::Sql(_))
        ));
        assert!(!f.retry_consequential_work);
        assert_eq!(f.stable_id, "k");
    }

    #[test]
    fn a_committed_boundary_classifies_every_later_failure_as_committed() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t(x)").unwrap();
        let mut g = Guard::new(Boundary::T5, "d1");
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO t VALUES(1)", []).unwrap();
        g.commit(tx).unwrap();
        let f = fault(g.finish(Err(Error::Invalid("response could not be produced"))));
        assert_eq!(
            (f.stage, f.durability, f.kind),
            (
                Stage::AfterCommit,
                Durability::Committed,
                FaultKind::PostCommit
            )
        );
        let n: i64 = c
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_commit_refused_by_a_deferred_constraint_is_not_committed() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE p(id INTEGER PRIMARY KEY);
             CREATE TABLE c(p INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap();
        let mut g = Guard::new(Boundary::T4, "a");
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO c VALUES(42)", []).unwrap();
        let f = fault(g.commit(tx));
        assert_eq!(
            (f.stage, f.durability, f.kind),
            (
                Stage::Commit,
                Durability::NotCommitted,
                FaultKind::Constraint
            )
        );
        let n: i64 = c
            .query_row("SELECT count(*) FROM c", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "rolled back");
    }

    #[test]
    fn boundaries_round_trip_and_name_their_stable_identity() {
        for b in Boundary::ALL {
            assert_eq!(Boundary::parse(b.as_str()), Some(b));
            assert!(b.index() > 0);
        }
        assert_eq!(Boundary::parse("T9"), None);
        assert_eq!(Boundary::T1.stable_id_kind(), "request_key");
        assert_eq!(Boundary::T6.stable_id_kind(), "effect_id");
        assert_eq!(Boundary::T4.stable_id_kind(), "review_id");
    }
}

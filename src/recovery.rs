//! Durable recovery accounting (Gate 6 M1).
//!
//! Every mutating startup, every explicit `recover` and the verification of a restored ledger
//! is one recovery run (`recovery_run`, migration 007). The run is committed as `started`
//! before it examines anything, and it finishes exactly once:
//!
//! * `completed`: every examined obligation is recorded in `recovery_item` (a trigger refuses
//!   completion otherwise); unresolved ones are assigned to an open `recovery_case`;
//! * `partial`: more than `MAX_ITEMS` obligations; the first `MAX_ITEMS` are recorded and no
//!   case is resolved, because incomplete coverage cannot show that a condition is gone;
//! * `failed`: startup failed after the run began (recorded best-effort);
//! * `interrupted`: the process died; the next run records this, so a dead pass never claims
//!   completion.
//!
//! When the newest `STARTUP_BREAKER` runs all failed or never finished, ordinary startup is
//! refused before it writes anything (`startup_gate`): repeating it would repeat the same
//! failure without bound. `diagnose` explains the state without writing; `recover` runs one
//! explicitly requested bounded pass.
//!
//! Recovery only observes, classifies, records evidence and assigns responsibility. Nothing
//! here dispatches, launches, retries, stops, notifies, closes, repairs or restores. A case is
//! resolved only by a later complete run that no longer observes its condition; restore cases
//! and storage incidents are never resolved by inference.
use crate::{
    Error,
    storage::{Boundary, Readback, StorageFault},
    store::now_ms,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

/// Best-effort emergency diagnostics written outside SQLite when a boundary faults.
pub const EMERGENCY_LOG: &str = "emergency.jsonl";
pub const EMERGENCY_MAX_BYTES: u64 = 256 * 1024;
const EMERGENCY_MAX_LINE: usize = 4096;
/// Consecutive failed or unfinished runs after which ordinary startup is refused.
pub const STARTUP_BREAKER: usize = 3;
/// Most obligations one run records.
pub const MAX_ITEMS: usize = 2000;
const LIST: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    Explicit,
    Restore,
}
impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Startup => "startup",
            Trigger::Explicit => "explicit",
            Trigger::Restore => "restore",
        }
    }
}

pub(crate) struct Run {
    pub(crate) id: String,
    trigger: Trigger,
    epoch: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Accounted,
    Unresolved,
    Unclassifiable,
}
impl Class {
    fn as_str(self) -> &'static str {
        match self {
            Class::Accounted => "accounted",
            Class::Unresolved => "unresolved",
            Class::Unclassifiable => "unclassifiable",
        }
    }
}
struct Subject {
    kind: &'static str,
    id: String,
    work: Option<String>,
    class: Class,
    key: String,
    reason: String,
    observation: Value,
    responsible: String,
    action: String,
    due: Option<i64>,
    attention: Option<String>,
}

/// One column as JSON whatever its stored type: adverse content (a TEXT where an INTEGER is
/// expected) is reported as it is instead of failing the read that admits or diagnoses.
fn cell(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match r.get_ref(i)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(x) => json!(x),
        ValueRef::Real(x) => json!(x),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(format!("<blob {} bytes>", b.len())),
    })
}

pub(crate) fn schema_version(conn: &Connection) -> Result<i64, Error> {
    Ok(conn.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?)
}

/// The newest runs (by insertion order, never by wall-clock time, which can move backwards)
/// and how many of them, newest first, failed or never finished.
pub(crate) fn breaker(conn: &Connection) -> Result<(usize, Vec<Value>), Error> {
    let mut s = conn.prepare(
        "SELECT id,trigger,state,coordinator_epoch,started_at,finished_at,summary_json FROM recovery_run ORDER BY rowid DESC LIMIT 5",
    )?;
    let rows = s
        .query_map([], |r| {
            Ok(json!({
                "id": cell(r, 0)?, "trigger": cell(r, 1)?,
                "state": cell(r, 2)?, "coordinator_epoch": cell(r, 3)?,
                "started_at": cell(r, 4)?, "finished_at": cell(r, 5)?,
                "summary": cell(r, 6)?.as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok()),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Only a run recorded as completed or partial ends the streak; any other (or unreadable)
    // state counts as unfinished.
    let unfinished = rows
        .iter()
        .take_while(|r| !matches!(r["state"].as_str(), Some("completed" | "partial")))
        .count();
    Ok((unfinished, rows))
}

/// Whether this ledger was derived from a backup (restored, or a copied snapshot).
pub(crate) fn is_restored(conn: &Connection) -> Result<bool, Error> {
    Ok(schema_version(conn)? >= 7 && restore_record(conn)?.is_some())
}
/// Read-only admission, before any startup write: a ledger derived from a backup is
/// inspect-only (only an explicit `recover` may account it), and the startup breaker applies.
pub(crate) fn admit(conn: &Connection, trigger: Trigger) -> Result<(), Error> {
    if schema_version(conn)? < 7 {
        return Ok(());
    }
    if trigger == Trigger::Startup {
        if let Some(r) = restore_record(conn)? {
            return Err(Error::Restored(
                json!({
                    "reason": "this ledger was derived from a backup; external actions may have happened after the backup, and restoration authorizes no replay, resume or repair, so ordinary startup is refused",
                    "restore": r,
                    "inspect": "diagnose, readback, list, get, inbox and check are read-only; recover records an accounting pass only",
                })
                .to_string(),
            ));
        }
    }
    startup_gate(conn, trigger)
}

/// Read-only gate, before any startup write. Explicit recovery is not gated.
pub(crate) fn startup_gate(conn: &Connection, trigger: Trigger) -> Result<(), Error> {
    if trigger != Trigger::Startup || schema_version(conn)? < 7 {
        return Ok(());
    }
    let (unfinished, rows) = breaker(conn)?;
    if unfinished >= STARTUP_BREAKER {
        return Err(Error::StartupBlocked(
            json!({
                "reason": format!("the newest {unfinished} recovery passes failed or never finished; repeating ordinary startup could repeat the same failure without bound"),
                "recent_runs": rows,
                "inspect": "orchestrator diagnose STATE (read-only, bounded)",
                "explicit_pass": "orchestrator recover STATE (one bounded pass; no consequential action)",
            })
            .to_string(),
        ));
    }
    Ok(())
}

/// Record a run as started (and any dead started run as interrupted) before examining.
pub(crate) fn begin(conn: &mut Connection, epoch: i64, trigger: Trigger) -> Result<Run, Error> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let run = begin_in(&tx, epoch, trigger)?;
    tx.commit()?;
    Ok(run)
}
/// As `begin`, inside the caller's transaction (startup: the epoch transaction).
pub(crate) fn begin_in(tx: &Connection, epoch: i64, trigger: Trigger) -> Result<Run, Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms()?;
    // The caller holds the coordinator lock (or a fresh restore target), so no other run is live.
    // Ids are matched by their stored value, whatever its type, so adverse content cannot
    // make every startup fail here.
    let dead: Vec<rusqlite::types::Value> = {
        let mut s = tx.prepare("SELECT id FROM recovery_run WHERE state='started'")?;
        s.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // A ledger derived from a backup inherits runs that were still running when the backup
    // was taken; they may have finished on the source afterwards.
    let meaning = if trigger == Trigger::Restore {
        "this pass had not finished when the backup was taken (it may have finished on the source afterwards); nothing it examined is claimed complete here"
    } else {
        "the process ended before this pass finished; nothing it examined is claimed complete"
    };
    for d in dead {
        tx.execute(
            "UPDATE recovery_run SET state='interrupted',finished_at=?1,summary_json=?2 WHERE id=?3 AND state='started'",
            params![
                now,
                json!({"completion_claimed": false, "detected_by_run": id, "detected_at_epoch": epoch,
                       "meaning": meaning})
                .to_string(),
                d
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO recovery_run(id,coordinator_epoch,trigger,state,started_at) VALUES(?1,?2,?3,'started',?4)",
        params![id, epoch, trigger.as_str(), now],
    )?;
    Ok(Run { id, trigger, epoch })
}

/// Whether a failed run's failure was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Yes,
    No,
    /// Recording it hit a storage fault that leaves its outcome unknown.
    Unknown,
}
/// Best effort: if storage cannot record the failure the run stays `started` and the next run
/// marks it interrupted. Either way it never claims completion. Returns whether the failure
/// was recorded.
pub(crate) fn fail(conn: &mut Connection, run: &Run, err: &Error) -> Recorded {
    let summary = json!({
        "completion_claimed": false, "error": err.to_string(), "error_class": error_class(err),
    });
    let now = match now_ms() {
        Ok(n) => n,
        Err(_) => return Recorded::No,
    };
    match conn.execute(
        "UPDATE recovery_run SET state='failed',finished_at=?1,summary_json=?2 WHERE id=?3 AND state='started'",
        params![now, summary.to_string(), run.id],
    ) {
        Ok(1) => Recorded::Yes,
        Ok(_) => Recorded::No,
        // The statement's own commit: busy or a constraint rolled it back; anything else
        // (full, I/O) may or may not have committed.
        Err(e) => match crate::storage::kind_of(&e) {
            Some((crate::storage::FaultKind::Busy | crate::storage::FaultKind::Constraint, _)) => {
                Recorded::No
            }
            _ => Recorded::Unknown,
        },
    }
}

/// Short class of a startup or boundary error, for summaries and diagnostics.
pub fn error_class(e: &Error) -> &'static str {
    match e {
        Error::Storage(_) => "storage_fault",
        Error::Sql(s) => match crate::storage::kind_of(s).map(|k| k.0) {
            Some(crate::storage::FaultKind::Busy) => "storage_busy",
            Some(crate::storage::FaultKind::Full) => "storage_full",
            Some(crate::storage::FaultKind::IoErr) => "storage_io_error",
            Some(crate::storage::FaultKind::Corrupt) => "ledger_corrupt",
            Some(crate::storage::FaultKind::ReadOnly) => "storage_read_only",
            Some(crate::storage::FaultKind::CantOpen) => "ledger_unavailable",
            Some(crate::storage::FaultKind::Constraint) => "ledger_constraint_failed",
            Some(_) => "storage_error",
            None => match s {
                rusqlite::Error::SqlInputError { .. } => "ledger_statement_failed",
                _ => "malformed_ledger_content",
            },
        },
        Error::Corrupt => "ledger_corrupt",
        Error::Schema(_) => "ledger_incompatible",
        Error::StartupBlocked(_) => "startup_blocked",
        Error::Restored(_) => "restored_inspect_only",
        Error::Unreadable(_) => "ledger_unreadable",
        Error::StartupFailed(inner, _)
        | Error::StartupPrecondition(inner, _)
        | Error::CommitUncertain(_, inner) => error_class(inner),
        Error::Owned => "owned_by_another_coordinator",
        Error::Io(_) => "filesystem_error",
        Error::Json(_) => "malformed_ledger_content",
        _ => "refused",
    }
}

/// Examine every obligation, record the run's items and cases, and finish the run once.
pub(crate) fn finish(conn: &mut Connection, root: &Path, run: &Run) -> Result<Value, Error> {
    crate::store::barrier("during_recovery_run")?;
    let restored = restore_record(conn)?;
    // A ledger derived from a backup is frozen evidence: its obligations are examined as what
    // the backup cannot vouch for, and nothing in it is resolved by inference.
    let frozen = restored.is_some();
    let mut subjects = examine(conn, frozen)?;
    let (incidents, settled) = import_incidents(conn, root, frozen)?;
    subjects.extend(incidents);
    // Unresolved subjects first, so a bound never hides them behind accounted obligations.
    subjects.sort_by_key(|s| s.class == Class::Accounted);
    let seen_total = subjects.len();
    let partial = seen_total > MAX_ITEMS;
    subjects.truncate(MAX_ITEMS);
    let still_unresolved: HashSet<(String, String)> = subjects
        .iter()
        .filter(|s| s.class != Class::Accounted)
        .map(|s| (s.kind.to_string(), s.id.clone()))
        .collect();

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms()?;
    let (mut opened, mut resolved, mut unresolved) = (0usize, 0usize, 0usize);
    let mut open_keys = HashSet::new();
    for (i, s) in subjects.iter().enumerate() {
        let case = if s.class == Class::Accounted {
            None
        } else {
            unresolved += 1;
            open_keys.insert(s.key.clone());
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM recovery_case WHERE subject_key=?1 AND state='open'",
                    [&s.key],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                Some(id) => Some(id),
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    tx.execute(
                        "INSERT INTO recovery_case(id,subject_key,subject_kind,subject_id,work_id,reason,state,responsible,required_action,due_at,attention_id,opened_run_id,opened_at,observation_json) VALUES(?1,?2,?3,?4,?5,?6,'open',?7,?8,?9,?10,?11,?12,?13)",
                        params![id, s.key, s.kind, s.id, s.work, s.reason, s.responsible, s.action, s.due, s.attention, run.id, now, s.observation.to_string()],
                    )?;
                    opened += 1;
                    Some(id)
                }
            }
        };
        tx.execute(
            "INSERT INTO recovery_item(run_id,seq,subject_kind,subject_id,work_id,classification,observation_json,case_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![run.id, i as i64 + 1, s.kind, s.id, s.work, s.class.as_str(), s.observation.to_string(), case],
        )?;
    }
    if !partial && !frozen {
        let open: Vec<(String, String, String, String)> = {
            let mut q = tx.prepare(
                "SELECT id,subject_key,subject_kind,subject_id FROM recovery_case WHERE state='open' ORDER BY opened_at,id",
            )?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, key, kind, subject) in open {
            // Restore uncertainty and storage incidents are never resolved by inference, and a
            // subject this run still found unresolved (under any key) is not resolved.
            if open_keys.contains(&key)
                || key.starts_with("restore:")
                || kind == "storage_incident"
                || still_unresolved.contains(&(kind.clone(), subject.clone()))
            {
                continue;
            }
            let state = current_state(&tx, &kind, &subject)?;
            tx.execute(
                "UPDATE recovery_case SET state='resolved',resolved_at=?1,resolved_run_id=?2,resolution=?3 WHERE id=?4 AND state='open'",
                params![now, run.id, format!("no longer observed by complete recovery run {}: {kind} {subject} is now {state}", run.id), id],
            )?;
            resolved += 1;
        }
    }
    // Storage incidents settle only on evidence: a later read back of the stable ID by a pass
    // that has committed under the coordinator lock. A partial pass resolves nothing, and a
    // ledger derived from a backup resolves nothing it inherited.
    let settled = if partial || frozen {
        Vec::new()
    } else {
        settled
    };
    for (key, resolution) in &settled {
        resolved += tx.execute(
            "UPDATE recovery_case SET state='resolved',resolved_at=?1,resolved_run_id=?2,resolution=?3 WHERE subject_key=?4 AND state='open'",
            params![now, run.id, resolution, key],
        )?;
    }
    let open_cases: i64 = tx.query_row(
        "SELECT count(*) FROM recovery_case WHERE state='open'",
        [],
        |r| r.get(0),
    )?;
    let state = if partial { "partial" } else { "completed" };
    let summary = json!({
        "run_id": run.id, "trigger": run.trigger.as_str(), "coordinator_epoch": run.epoch,
        "state": state, "examined": subjects.len(), "not_examined": seen_total - subjects.len(),
        "unresolved": unresolved, "cases_opened": opened, "cases_resolved": resolved,
        "open_cases": open_cases, "restored_ledger": restored.is_some(),
        "restore": restored,
        "fence": restored.as_ref().map(|_| json!({"ordinary_startup": "refused", "launch": "refused", "dispatch": "refused"})),
        "consequential_actions": "none: recovery dispatched, launched, retried, stopped, notified, closed, repaired and restored nothing",
    });
    tx.execute(
        "UPDATE recovery_run SET state=?1,finished_at=?2,examined=?3,unresolved=?4,summary_json=?5 WHERE id=?6 AND state='started'",
        params![state, now, subjects.len() as i64, unresolved as i64, summary.to_string(), run.id],
    )?;
    tx.commit()?;
    Ok(summary)
}

fn current_state(conn: &Connection, kind: &str, id: &str) -> Result<String, Error> {
    let q = match kind {
        "work" => "SELECT mode||'/'||phase||' '||next_action FROM work WHERE id=?1",
        "attempt" => "SELECT state FROM attempt WHERE id=?1",
        "effect" => "SELECT state FROM effect WHERE id=?1",
        "review" => "SELECT state FROM review WHERE id=?1",
        "artifact" => "SELECT state FROM artifact WHERE id=?1",
        "attention" => "SELECT state FROM attention WHERE id=?1",
        _ => return Ok("not tracked".into()),
    };
    Ok(conn
        .query_row(q, [id], |r| r.get::<_, String>(0))
        .optional()?
        .unwrap_or_else(|| "absent".into()))
}

/// Responsibility currently recorded for a work (closed work has none of its own).
fn responsibility(
    conn: &Connection,
    work: &str,
) -> Result<(String, String, Option<i64>, Option<String>), Error> {
    let (owner, action, due, mode): (String, String, i64, String) = conn.query_row(
        "SELECT next_owner,next_action,due_at,mode FROM work WHERE id=?1",
        [work],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let attention: Option<String> = conn
        .query_row(
            "SELECT id FROM attention WHERE work_id=?1 AND state<>'resolved' AND blocking=1 ORDER BY created_at,id LIMIT 1",
            [work],
            |r| r.get(0),
        )
        .optional()?;
    if mode == "closed" {
        return Ok((
            "local-owner".into(),
            "inspect_post_closure_discrepancy".into(),
            None,
            attention,
        ));
    }
    Ok((owner, action, Some(due), attention))
}

// Each argument is one recorded field of the examined subject.
#[allow(clippy::too_many_arguments)]
fn subject(
    conn: &Connection,
    kind: &'static str,
    id: &str,
    work: &str,
    class: Class,
    key: String,
    reason: impl Into<String>,
    observation: Value,
) -> Result<Subject, Error> {
    let (responsible, action, due, attention) = responsibility(conn, work)?;
    Ok(Subject {
        kind,
        id: id.into(),
        work: Some(work.into()),
        class,
        key,
        reason: reason.into(),
        observation,
        responsible,
        action,
        due,
        attention,
    })
}

/// Read-only examination of every obligation. `restore` marks what a backup cannot vouch for.
fn examine(conn: &Connection, restore: bool) -> Result<Vec<Subject>, Error> {
    let now = now_ms()?;
    let mut out = Vec::new();
    let works: Vec<(String, String, String, String, i64, i64)> = {
        let mut s = conn.prepare(
            "SELECT id,phase,next_owner,next_action,due_at,version FROM work WHERE mode='active' ORDER BY accepted_at,id",
        )?;
        s.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (id, phase, owner, action, due, version) in works {
        let obs = json!({"phase": phase, "next_owner": owner, "next_action": action,
                         "due_at": due, "overdue": due < now, "version": version});
        out.push(if restore {
            subject(
                conn,
                "work",
                &id,
                &id,
                Class::Unresolved,
                format!("restore:work:{id}"),
                "restored obligation: progress after the backup is unknown to this ledger",
                obs,
            )?
        } else {
            subject(
                conn,
                "work",
                &id,
                &id,
                Class::Accounted,
                format!("work:{id}"),
                "responsibility recorded",
                obs,
            )?
        });
    }
    let attempts: Vec<(String, String, String, String)> = {
        let mut s = conn.prepare(
            "SELECT id,work_id,state,reason FROM attempt WHERE state<>'cleaned' ORDER BY created_at,id",
        )?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, work, state, reason) in attempts {
        let receipt: bool = conn
            .query_row(
                "SELECT 1 FROM supervisor_command WHERE attempt_id=?1",
                [&id],
                |_| Ok(true),
            )
            .optional()?
            .is_some();
        let obs = json!({"state": state, "reason": reason, "supervisor_receipt": receipt});
        out.push(if restore {
            subject(conn, "attempt", &id, &work, Class::Unresolved, format!("restore:attempt:{id}"),
                "a process may have been launched or run after the backup; the restored ledger cannot show it and never relaunches", obs)?
        } else if state == "unknown" {
            subject(conn, "attempt", &id, &work, Class::Unresolved, format!("attempt:{id}"),
                "attempt activity could not be established; the live slot stays occupied and nothing is relaunched", obs)?
        } else {
            subject(conn, "attempt", &id, &work, Class::Accounted, format!("attempt:{id}"),
                format!("attempt {state}; tracked"), obs)?
        });
    }
    let effects: Vec<(String, String, String, String, String)> = {
        let mut s = conn.prepare(
            "SELECT id,work_id,state,state_reason,action_json FROM effect WHERE state IN ('intent','dispatching','unknown','conflict') ORDER BY created_at,id",
        )?;
        s.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (id, work, state, reason, action) in effects {
        let mut obs = json!({"state": state, "state_reason": reason});
        if restore && state != "conflict" {
            let (class, probe) = probe_fixture(&action, &id);
            obs["fixture_observation"] = probe;
            let class = if class == Class::Unclassifiable {
                class
            } else {
                Class::Unresolved
            };
            out.push(subject(conn, "effect", &id, &work, class, format!("restore:effect:{id}"),
                "the effect may have been applied after the backup; the restored ledger cannot show it and never dispatches", obs)?);
            continue;
        }
        let (class, why) = match state.as_str() {
            "intent" => (Class::Accounted, "approved; not dispatched".to_string()),
            "dispatching" => (Class::Unresolved, "dispatched; result not recorded or the executor may still be live; never re-dispatched".into()),
            "unknown" => (Class::Unresolved, "effect outcome unknown; held for reconciliation, never retried".into()),
            _ => (Class::Unresolved, "conflicting target state; no automatic retry or rollback".into()),
        };
        out.push(subject(
            conn,
            "effect",
            &id,
            &work,
            class,
            format!("effect:{id}"),
            why,
            obs,
        )?);
    }
    let artifacts: Vec<(String, String, Option<String>)> = {
        let mut s = conn.prepare(
            "SELECT a.id,a.work_id,a.invalid_reason FROM artifact a JOIN work w ON w.id=a.work_id WHERE a.state='invalid' AND w.mode='active' ORDER BY a.sealed_at,a.id",
        )?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, work, reason) in artifacts {
        out.push(subject(
            conn,
            "artifact",
            &id,
            &work,
            Class::Unresolved,
            format!("artifact:{id}"),
            "artifact invalid; dependent review and effect stay frozen",
            json!({"invalid_reason": reason}),
        )?);
    }
    if restore {
        let reviews: Vec<(String, String, String)> = {
            let mut s = conn.prepare(
                "SELECT id,work_id,state FROM review WHERE state IN ('pending','authorized') ORDER BY created_at,id",
            )?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, work, state) in reviews {
            out.push(subject(conn, "review", &id, &work, Class::Unresolved, format!("restore:review:{id}"),
                "an owner decision may have been made after the backup; the restored review cannot show it",
                json!({"state": state}))?);
        }
    }
    let discrepancies: Vec<(String, String, String, String)> = {
        let mut s = conn.prepare(
            "SELECT id,work_id,source_key,reason FROM attention WHERE state<>'resolved' AND source_key LIKE 'discrepancy:%' ORDER BY created_at,id",
        )?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, work, key, reason) in discrepancies {
        out.push(subject(
            conn,
            "attention",
            &id,
            &work,
            Class::Unresolved,
            format!("attention:{id}"),
            "post-closure discrepancy awaiting the owner",
            json!({"source_key": key, "reason": reason}),
        )?);
    }
    Ok(out)
}

/// Read-only look at the fixture for an effect a restored ledger cannot account for.
fn probe_fixture(action_json: &str, effect_id: &str) -> (Class, Value) {
    let action: crate::review::Action = match serde_json::from_str(action_json) {
        Ok(a) => a,
        Err(e) => {
            return (
                Class::Unclassifiable,
                json!({"error": format!("stored action unreadable: {e}")}),
            );
        }
    };
    let repo = action.repo();
    let current = crate::gitfixture::read_ref(&repo);
    let log = crate::gitfixture::read_log(&repo);
    let message = crate::gitfixture::effect_message(effect_id);
    let audit = log
        .as_ref()
        .map(|(_, l)| l.iter().filter(|e| e.message == message).count());
    (
        Class::Unresolved,
        json!({
            "repository": action.repository.path,
            "ref_now": current.as_ref().ok(), "ref_error": current.as_ref().err(),
            "expected_old": action.expected_old, "proposed_commit": action.new_commit,
            "audit_entries_for_effect": audit.as_ref().ok(), "reflog_error": log.as_ref().err(),
            "meaning": "observation only; absence of an audit entry does not show the effect was not applied, and nothing is dispatched to find out",
        }),
    )
}

fn restore_record(conn: &Connection) -> Result<Option<Value>, Error> {
    let snapshot = conn
        .query_row(
            "SELECT id,source_root,source_schema,created_at FROM ledger_snapshot ORDER BY created_at,id LIMIT 1",
            [],
            |r| {
                Ok(json!({"snapshot_id": cell(r, 0)?, "source_root": cell(r, 1)?,
                          "source_schema": cell(r, 2)?, "backup_created_at": cell(r, 3)?}))
            },
        )
        .optional()?;
    let restore = conn
        .query_row(
            "SELECT id,source_root,backup_created_at,restored_at,backup_schema FROM ledger_restore ORDER BY restored_at,id LIMIT 1",
            [],
            |r| {
                Ok(json!({"restore_id": cell(r, 0)?, "source_root": cell(r, 1)?,
                          "backup_created_at": cell(r, 2)?, "restored_at": cell(r, 3)?,
                          "backup_schema": cell(r, 4)?}))
            },
        )
        .optional()?;
    Ok(match (restore, snapshot) {
        (None, None) => None,
        (r, s) => Some(json!({"restored": r, "snapshot": s})),
    })
}

/// Launches, effect dispatch, revocation and every other lock-free write are refused in a
/// ledger derived from a backup (produced by `restore`, or a copied snapshot).
pub(crate) fn restore_fence(conn: &Connection, what: &str) -> Result<(), Error> {
    if schema_version(conn)? < 7 {
        return Ok(());
    }
    if let Some(r) = restore_record(conn)? {
        return Err(Error::Rejected(format!(
            "{what} refused: this ledger was derived from a backup (restored, or a copied snapshot: {r}); it is inspect-only, external actions may have happened after the backup, and restoration authorizes no replay, resume, repair or new owner fact; act on the live source ledger"
        )));
    }
    Ok(())
}

/// Append one emergency record. Best effort and bounded; never a durable acknowledgment.
pub fn record_incident(root: &Path, fault: &StorageFault) -> Result<String, String> {
    let incident = uuid::Uuid::new_v4().to_string();
    let mut f = fault.clone();
    f.detail.truncate(512);
    let line = json!({"v": 1, "incident": incident, "at": now_ms().unwrap_or(0),
                      "pid": std::process::id(),
                      "identity": crate::process_scope::Identity::current().ok(), "fault": f})
    .to_string();
    if line.len() > EMERGENCY_MAX_LINE {
        return Err("emergency record too large".into());
    }
    let m = fs::symlink_metadata(root).map_err(|e| format!("state root unavailable: {e}"))?;
    if !m.is_dir() {
        return Err("state root is not a directory".into());
    }
    let p = root.join(EMERGENCY_LOG);
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)
        .map_err(|e| format!("emergency log unavailable: {e}"))?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() || m.nlink() != 1 || m.mode() & 0o077 != 0 {
        return Err("emergency log is not a private regular file".into());
    }
    if m.len() + line.len() as u64 + 1 > EMERGENCY_MAX_BYTES {
        return Err("emergency log is full; this incident is not recorded".into());
    }
    // A torn earlier record (no final newline) must not swallow this one.
    let mut last = [0u8; 1];
    let torn = m.len() > 0
        && std::os::unix::fs::FileExt::read_at(&file, &mut last, m.len() - 1)
            .map_err(|e| e.to_string())?
            == 1
        && last[0] != b'\n';
    let text = if torn {
        format!("\n{line}\n")
    } else {
        format!("{line}\n")
    };
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_data())
        .map_err(|e| format!("emergency record not written: {e}"))?;
    Ok(incident)
}

/// Bounded, read-only parse of the emergency log (no SQLite involved).
pub(crate) fn read_incidents(root: &Path) -> Result<(Vec<Result<Value, String>>, bool), String> {
    let p = root.join(EMERGENCY_LOG);
    let m = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => return Err(format!("emergency log unreadable: {e}")),
    };
    if !m.is_file() || m.file_type().is_symlink() || m.nlink() != 1 {
        return Err("emergency log is not a regular file; not read".into());
    }
    let mut b = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)
        .map_err(|e| e.to_string())?
        .take(EMERGENCY_MAX_BYTES)
        .read_to_end(&mut b)
        .map_err(|e| e.to_string())?;
    let truncated = m.len() > EMERGENCY_MAX_BYTES;
    let text = String::from_utf8_lossy(&b);
    let out = text
        .split_terminator('\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<Value>(l).map_err(|_| l.chars().take(200).collect()))
        .collect();
    Ok((out, truncated))
}

/// The `recovery_item` subject under which an import records line `n` (0-based) of the
/// emergency log: the incident of a well-formed record with a valid incident and boundary,
/// otherwise an identity bound to the line and its content. `diagnose` uses the same identity
/// to ask whether a line has been examined, so no line can be settled by being unparseable.
pub(crate) fn record_subject(n: usize, entry: &Result<Value, String>) -> String {
    let derived = |b: &[u8]| format!("line-{}-{}", n + 1, &crate::artifact::sha256_hex(b)[..16]);
    match entry {
        Err(text) => derived(text.as_bytes()),
        Ok(v) => {
            let incident = v["incident"].as_str().unwrap_or("");
            let boundary = v["fault"]["boundary"].as_str().and_then(Boundary::parse);
            if boundary.is_some() && crate::protocol::valid_id(incident) {
                incident.to_string()
            } else {
                derived(v.to_string().as_bytes())
            }
        }
    }
}

/// Import emergency records as secondary evidence and read back each stable ID. A record is
/// imported once; one whose read back was not yet conclusive is read back again by later runs
/// and settles (its case resolves) only on evidence. Returns the examined subjects and the
/// cases settled by evidence.
#[allow(clippy::type_complexity)]
fn import_incidents(
    conn: &Connection,
    root: &Path,
    frozen: bool,
) -> Result<(Vec<Subject>, Vec<(String, String)>), Error> {
    let local = |class, id: String, reason: String, observation: Value| Subject {
        kind: "storage_incident",
        key: format!("storage_incident:{id}"),
        id,
        work: None,
        class,
        reason,
        observation,
        responsible: "local-owner".into(),
        action: "inspect_storage_incident".into(),
        due: None,
        attention: None,
    };
    let mut out = Vec::new();
    let mut settled = Vec::new();
    let (entries, truncated) = match read_incidents(root) {
        Ok(x) => x,
        Err(e) => {
            out.push(local(
                Class::Unclassifiable,
                "emergency-log".into(),
                "emergency log could not be read".into(),
                json!({"error": e}),
            ));
            return Ok((out, settled));
        }
    };
    for (n, entry) in entries.into_iter().enumerate() {
        let subject = record_subject(n, &entry);
        let v = match entry {
            Ok(v) => v,
            Err(text) => {
                let id = subject;
                if last_classification(conn, &id)?.is_none() {
                    out.push(local(
                        Class::Unclassifiable,
                        id,
                        "malformed emergency record".into(),
                        json!({"line": n + 1, "prefix": text}),
                    ));
                }
                continue;
            }
        };
        let incident = v["incident"].as_str().unwrap_or("").to_string();
        let boundary = v["fault"]["boundary"].as_str().and_then(Boundary::parse);
        let step = v["fault"]["step"].as_str().unwrap_or("").to_string();
        let stable = v["fault"]["stable_id"].as_str().unwrap_or("").to_string();
        let durability = v["fault"]["durability"].as_str().unwrap_or("").to_string();
        let Some(b) = boundary.filter(|_| crate::protocol::valid_id(&incident)) else {
            let id = subject;
            if last_classification(conn, &id)?.is_none() {
                out.push(local(
                    Class::Unclassifiable,
                    id,
                    "emergency record without a valid incident or boundary".into(),
                    json!({"line": n + 1}),
                ));
            }
            continue;
        };
        let prior = last_classification(conn, &incident)?;
        if prior.as_deref() == Some("accounted") {
            continue;
        }
        let faulting = faulting_process(&v);
        let pid_alive = faulting != "absent" && faulting != "earlier_boot";
        let mut obs = json!({"record": v, "boundary": b.as_str(), "step": step, "stable_id": stable,
                             "faulting_process": faulting, "reread": prior.is_some()});
        let (class, reason) = match crate::storage::readback_step(conn, b, &step, &stable) {
            Ok(Readback::Present(d)) => {
                obs["readback"] = json!("present");
                obs["detail"] = d;
                (
                    Class::Accounted,
                    "stable ID present: a commit with this identity is in the ledger".to_string(),
                )
            }
            Ok(Readback::Inconclusive(d)) => {
                obs["readback"] = json!("inconclusive");
                obs["detail"] = d;
                (
                    Class::Unresolved,
                    "the recorded ID does not identify one commit; this does not establish that the transaction did not commit"
                        .into(),
                )
            }
            // A ledger derived from a backup cannot show a commit made after the backup:
            // absence in it establishes nothing about the source.
            Ok(Readback::Absent(d)) if frozen => {
                obs["readback"] = json!("absent");
                obs["detail"] = d;
                obs["meaning"] = json!(
                    "absent in a ledger derived from a backup; a commit made after the backup cannot appear here, so this establishes nothing; the live source ledger, if it exists, is the evidence"
                );
                (
                    Class::Unresolved,
                    "stable ID absent in a ledger derived from a backup: whether it committed on the source after the backup is unknown"
                        .into(),
                )
            }
            Ok(Readback::Absent(d)) if pid_alive => {
                obs["readback"] = json!("absent");
                obs["detail"] = d;
                (
                    Class::Unresolved,
                    "stable ID absent but the faulting process may still hold its connection; read back again after it ends"
                        .into(),
                )
            }
            Ok(Readback::Absent(d)) if durability == "committed" => {
                obs["readback"] = json!("absent");
                obs["detail"] = d;
                (
                    Class::Unresolved,
                    "the incident reported a commit but its stable ID is absent; inspect before any retry"
                        .into(),
                )
            }
            Ok(Readback::Absent(d)) => {
                obs["readback"] = json!("absent");
                obs["detail"] = d;
                obs["meaning"] = json!(
                    "absent on a verified read made under the coordinator lock after this pass's own commit (from then on a commit whose WAL sync failed can no longer appear) and after the faulting process ended: not committed; the same request may be resubmitted with the same identity; recovery resubmitted nothing"
                );
                (
                    Class::Accounted,
                    "stable ID absent on a verified read".into(),
                )
            }
            Err(e) => {
                obs["readback"] = json!("unavailable");
                obs["error"] = json!(e.to_string());
                (
                    Class::Unresolved,
                    "read back unavailable; this does not establish that the transaction did not commit"
                        .into(),
                )
            }
        };
        if prior.is_some() && class == Class::Accounted {
            settled.push((
                format!("storage_incident:{incident}"),
                match obs["readback"].as_str() {
                    Some("present") => "settled on evidence: the stable ID is present".to_string(),
                    _ => "settled on evidence: the stable ID is absent on a read made under the coordinator lock after this pass committed and after the faulting process ended".to_string(),
                },
            ));
        }
        out.push(local(class, incident, reason, obs));
    }
    if truncated {
        out.push(local(
            Class::Unresolved,
            "emergency-log-truncated".into(),
            "emergency log exceeds its bound; records beyond it were not examined".into(),
            json!({"bound_bytes": EMERGENCY_MAX_BYTES}),
        ));
    } else {
        settled.push((
            "storage_incident:emergency-log-truncated".into(),
            "the emergency log is back within its bound".into(),
        ));
    }
    Ok((out, settled))
}
/// The latest recorded classification of an imported incident, if it was ever imported.
fn last_classification(conn: &Connection, id: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row(
            "SELECT classification FROM recovery_item WHERE subject_kind='storage_incident' AND subject_id=?1 ORDER BY rowid DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

impl crate::Owner {
    /// Recorded recovery state (runs, breaker, open cases, restore) for the owner console.
    pub fn recovery_status(&self) -> Result<Value, Error> {
        status(&self.conn)
    }
}

/// Bounded read-only summary of recorded recovery state, for `diagnose` and `recover`.
pub(crate) fn status(conn: &Connection) -> Result<Value, Error> {
    let (unfinished, runs) = breaker(conn)?;
    let open: i64 = conn.query_row(
        "SELECT count(*) FROM recovery_case WHERE state='open'",
        [],
        |r| r.get(0),
    )?;
    let mut s = conn.prepare(&format!(
        "SELECT id,subject_kind,subject_id,work_id,reason,responsible,required_action,due_at,attention_id,opened_at FROM recovery_case WHERE state='open' ORDER BY opened_at,id LIMIT {LIST}"
    ))?;
    let cases = s
        .query_map([], |r| {
            Ok(json!({"case_id": cell(r, 0)?, "subject_kind": cell(r, 1)?,
                      "subject_id": cell(r, 2)?, "work_id": cell(r, 3)?,
                      "reason": cell(r, 4)?, "responsible": cell(r, 5)?,
                      "required_action": cell(r, 6)?, "due_at": cell(r, 7)?,
                      "attention_id": cell(r, 8)?, "opened_at": cell(r, 9)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({
        "recent_runs": runs, "consecutive_unfinished_runs": unfinished,
        "startup_blocked": unfinished >= STARTUP_BREAKER, "breaker_threshold": STARTUP_BREAKER,
        "open_cases": open, "cases": cases, "cases_listed_limit": LIST,
        "restore": restore_record(conn)?,
    }))
}

/// Whether the process that wrote an emergency record may still hold its connection. A PID is
/// never liveness evidence: a recorded identity (pid, start, boot) is checked as an identity,
/// and one from an earlier boot is stale without any /proc read. A record without an identity
/// (written before schema 8) can only show absence: no /proc entry for its PID means no process
/// with that PID exists; an entry that exists is not the recorded process, only "unknown".
fn faulting_process(record: &Value) -> &'static str {
    if let Ok(i) =
        serde_json::from_value::<crate::process_scope::Identity>(record["identity"].clone())
    {
        return match i.from_earlier_boot() {
            Some(true) => "earlier_boot",
            _ if i.live() => "may_be_live",
            _ => "absent",
        };
    }
    let pid = record["pid"].as_i64().unwrap_or(0);
    if pid <= 0 {
        return "absent";
    }
    match fs::symlink_metadata(format!("/proc/{pid}")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent",
        _ => "unknown",
    }
}

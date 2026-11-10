//! Coordinator-only synchronous domain mutations. No detached Owner workers.
use crate::{
    Error, Owner,
    process_scope::{Identity, scrub},
    protocol::{PermitScope, fingerprint, valid_id},
    store::{barrier, now_ms},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FakeMode {
    Complete,
    Hold,
    Descendant,
    Escape,
    Ignore,
    Flood,
    PipeHolder,
    /// Task 3: rewrite only `candidate/symbols.csv` to the fixed correction.
    PrepareSymbol,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub mode: FakeMode,
    pub runtime_ms: u64,
    pub output_limit: u64,
}
impl Spec {
    pub fn validate(&self) -> Result<(), Error> {
        if self.runtime_ms == 0
            || self.runtime_ms > 60000
            || self.output_limit < 1024
            || self.output_limit > 65536
        {
            return Err(Error::Invalid("unbounded fake spec"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub work_id: String,
    pub spec_hash: String,
    pub state: String,
    pub reason: String,
    pub fixture_path: String,
    pub epoch: i64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub version: u32,
    pub role: String,
    pub attempt_id: String,
    pub command_id: String,
    pub fingerprint: String,
    pub epoch: i64,
}
pub fn attempts(conn: &Connection) -> Result<Vec<Attempt>, Error> {
    let mut q=conn.prepare("SELECT id,work_id,spec_hash,state,reason,fixture_path,coordinator_epoch FROM attempt ORDER BY created_at,id")?;
    Ok(q.query_map([], |r| {
        Ok(Attempt {
            id: r.get(0)?,
            work_id: r.get(1)?,
            spec_hash: r.get(2)?,
            state: r.get(3)?,
            reason: r.get(4)?,
            fixture_path: r.get(5)?,
            epoch: r.get(6)?,
        })
    })?
    .collect::<Result<_, _>>()?)
}
/// What a lock-free caller (one that does not run startup) does with the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read only (`attempts`): a read-only connection that never checkpoints or writes.
    Inspect,
    /// Records owner or supervisor facts (`revoke`, a supervisor launch). Refused in a ledger
    /// derived from a backup, which is inspect-only. Allowed while the startup breaker blocks
    /// ordinary startup (these paths run no startup; revocation only narrows authority, and a
    /// supervisor only acts for a launch the current epoch accepted), but such a ledger is
    /// never checkpointed when this connection closes.
    Act(&'static str),
}
fn verify(c: &Connection, access: Access) -> Result<bool, Error> {
    crate::store::check(c)?;
    let version: i64 = c.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
    if version != crate::store::CURRENT_SCHEMA {
        return Err(Error::Schema(version));
    }
    if let Access::Act(what) = access {
        crate::recovery::restore_fence(c, what)?;
    }
    Ok(crate::recovery::breaker(c)?.0 >= crate::recovery::STARTUP_BREAKER)
}
pub fn open_ledger(root: &Path, access: Access) -> Result<Connection, Error> {
    crate::store::validate_root(root)?;
    crate::store::preopen(root)?;
    crate::storage::prepare();
    // With no WAL beside the ledger, verify through an immutable view first, so a refusal
    // leaves no new file behind (any ordinary open of a WAL-mode ledger creates the WAL).
    if std::fs::symlink_metadata(root.join("ledger.db-wal")).is_err() {
        let v = crate::store::open_immutable(&root.join("ledger.db"))?;
        verify(&v, access)?;
    }
    if access == Access::Inspect {
        let c = crate::store::Reader::open(root)?.into_conn();
        verify(&c, access)?;
        return Ok(c);
    }
    let c = Connection::open_with_flags(
        root.join("ledger.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    // A refused ledger must not be checkpointed or lose its WAL when this connection closes.
    c.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    let blocked = verify(&c, access)?;
    if !blocked {
        c.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            false,
        )?;
    }
    crate::store::configure(&c)?;
    if blocked {
        // Nor does a commit of this connection backfill the blocked ledger automatically.
        c.pragma_update(None, "wal_autocheckpoint", 0)?;
    }
    Ok(c)
}
pub fn check_permit(conn: &Connection, attempt: &str, epoch: i64) -> Result<(), Error> {
    let (issued,expires,revoked,scope,spec,owner):(i64,i64,Option<i64>,String,String,i64)=conn.query_row(
        "SELECT p.issued_at,p.expires_at,p.revoked_at,p.scope_json,a.spec_json,a.coordinator_epoch FROM attempt a JOIN permit p ON p.id=a.permit_id WHERE a.id=?1",[attempt],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let current: i64 = conn.query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))?;
    // Durable conservative time: an uncertain clock counts the permit as expired.
    let t = crate::store::authority_time(conn)?;
    let scope: PermitScope = serde_json::from_str(&scope)?;
    scope.validate()?;
    let spec: Spec = serde_json::from_str(&spec)?;
    spec.validate()?;
    if t.before(issued)
        || t.reached(expires)
        || revoked.is_some()
        || spec.runtime_ms > scope.max_runtime_ms
        || epoch != current
        || owner != epoch
    {
        return Err(Error::Permit);
    }
    Ok(())
}
/// The runtime budget of a released workload whose supervisor is gone, measured in wall time
/// from the durable `started` fact (boot-local time is not comparable across a reboot). An
/// uncertain clock makes the budget unverifiable, never "within budget".
fn budget_note(
    conn: &Connection,
    attempt: &str,
    time: &crate::store::AuthorityTime,
) -> Result<Option<String>, Error> {
    let started: Option<i64> = conn
        .query_row(
            "SELECT at FROM supervisor_fact WHERE attempt_id=?1 AND kind='started' ORDER BY seq LIMIT 1",
            [attempt],
            |r| r.get(0),
        )
        .optional()?;
    let Some(started) = started else {
        return Ok(None);
    };
    let spec: Spec = serde_json::from_str(&conn.query_row::<String, _, _>(
        "SELECT spec_json FROM attempt WHERE id=?1",
        [attempt],
        |r| r.get(0),
    )?)?;
    let limit = started.saturating_add(i64::try_from(spec.runtime_ms).unwrap_or(i64::MAX));
    Ok(match time.known() {
        Some(_) if !time.reached(limit) => None,
        Some(_) => Some(format!(
            "runtime budget exceeded ({} ms from start at {started}): recorded, not signalled",
            spec.runtime_ms
        )),
        None => Some(
            "runtime budget unverifiable (time uncertain): recorded, not signalled".to_string(),
        ),
    })
}
/// The first supervisor fact of an attempt whose payload does not parse, as (seq, kind).
fn first_unreadable_fact(conn: &Connection, attempt: &str) -> Result<Option<(i64, String)>, Error> {
    let mut s = conn.prepare(
        "SELECT seq,kind,payload_json FROM supervisor_fact WHERE attempt_id=?1 ORDER BY seq",
    )?;
    let rows: Vec<(i64, String, String)> = s
        .query_map([attempt], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .find(|(_, _, p)| serde_json::from_str::<serde_json::Value>(p).is_err())
        .map(|(seq, kind, _)| (seq, kind)))
}
/// One durable evidence gap per (attempt, kind); repeating the accounting adds nothing.
fn record_gap(
    conn: &Connection,
    attempt: &str,
    kind: &str,
    detail: serde_json::Value,
) -> Result<(), Error> {
    let recorded: i64 = conn.query_row(
        "SELECT count(*) FROM evidence_gap WHERE attempt_id=?1 AND kind=?2",
        params![attempt, kind],
        |r| r.get(0),
    )?;
    if recorded == 0 {
        conn.execute(
            "INSERT INTO evidence_gap(attempt_id,kind,detail_json,at) VALUES(?1,?2,?3,?4)",
            params![
                attempt,
                kind,
                detail.to_string(),
                crate::store::stamp_ms(conn)
            ],
        )?;
    }
    Ok(())
}
/// Marker written into an un-launched intent's reason when closure retires it.
pub(crate) const RETIRED_PREFIX: &str = "retired at closure";

/// A new launch is allowed only for an un-retired intent of active, un-cancelled work.
/// Checked by the coordinator before spawning and again by the supervisor inside the
/// transaction that would record its receipt, so a stale CLI action can never run a
/// workload for closed or cancelled work.
pub(crate) fn launch_fence(conn: &Connection, attempt: &str) -> Result<(), Error> {
    let (work, reason): (String, String) = conn.query_row(
        "SELECT work_id,reason FROM attempt WHERE id=?1",
        [attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if reason.starts_with(RETIRED_PREFIX) {
        return Err(Error::Rejected(format!(
            "launch intent was retired when the work closed: {reason}"
        )));
    }
    crate::recovery::restore_fence(conn, "launch")?;
    crate::attention::progress_fence(conn, &work)
}
fn symbol_mode(conn: &Connection, attempt: &str) -> Result<bool, Error> {
    let spec: String = conn.query_row(
        "SELECT spec_json FROM attempt WHERE id=?1",
        [attempt],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str::<Spec>(&spec)?.mode == FakeMode::PrepareSymbol)
}
pub fn fixture_path(conn: &Connection, id: &str) -> Result<PathBuf, Error> {
    let (s, dev, ino): (String, i64, i64) = conn.query_row(
        "SELECT fixture_path,fixture_dev,fixture_ino FROM attempt WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let p = PathBuf::from(s);
    let m = fs::symlink_metadata(&p)?;
    if !m.is_dir() || m.dev() as i64 != dev || m.ino() as i64 != ino || p.canonicalize()? != p {
        return Err(Error::Invalid("fixture identity changed"));
    }
    Ok(p)
}
impl Owner {
    pub fn intent(&mut self, key: &str, id: &str, spec: &Spec) -> Result<Attempt, Error> {
        spec.validate()?;
        if !valid_id(id) {
            return Err(Error::Invalid("invalid attempt ID"));
        }
        let body = serde_json::to_string(spec)?;
        let hash = fingerprint(&body);
        let parent = self.root().join("fixtures");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (work, permit, request): (String, String, String) = tx.query_row(
            "SELECT id,permit_id,request_json FROM work WHERE request_key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if let Some(a) = attempts(&tx)?.into_iter().find(|a| a.id == id) {
            if a.work_id != work || a.spec_hash != hash {
                return Err(Error::Conflict);
            }
            tx.commit()?;
            return Ok(a);
        }
        crate::attention::progress_fence(&tx, &work)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM attempt WHERE work_id=?1",
            [&work],
            |r| r.get(0),
        )?;
        // Task 1 permit explicitly grants one attempt. Do not add automatic retries here.
        if count != 0 {
            return Err(Error::Invalid(
                "attempt budget exhausted or unresolved execution",
            ));
        }
        let (scope, issued, expiry, revoked): (String, i64, i64, Option<i64>) = tx.query_row(
            "SELECT scope_json,issued_at,expires_at,revoked_at FROM permit WHERE id=?1",
            [&permit],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let scope: PermitScope = serde_json::from_str(&scope)?;
        scope.validate()?;
        let request: crate::protocol::SubmitWork = serde_json::from_str(&request)?;
        let t = crate::store::authority_time(&tx)?;
        let now = t.stamp();
        if revoked.is_some()
            || t.before(issued)
            || t.reached(expiry)
            || spec.runtime_ms > scope.max_runtime_ms
            || scope.fixture_id != request.fixture_id
        {
            return Err(Error::Permit);
        }
        if !parent.exists() {
            fs::DirBuilder::new().mode(0o700).create(&parent)?;
        }
        if parent.canonicalize()? != parent {
            return Err(Error::Invalid("symlink fixture root"));
        }
        let p = parent.join(uuid::Uuid::new_v4().to_string());
        fs::DirBuilder::new().mode(0o700).create(&p)?;
        if spec.mode == FakeMode::PrepareSymbol {
            // Synced before the intent commits; a crash leaves only a harmless orphan.
            crate::validation::create_symbol_layout(&p)?;
            fs::File::open(&parent)?.sync_all()?;
        } else {
            fs::write(p.join("symbols.csv"), b"symbol,scope\nBAD SYMBOL,core\n")?;
        }
        let m = fs::metadata(&p)?;
        tx.execute("INSERT INTO attempt VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'intent','launch not yet accepted',?10,?11)",params![id,work,permit,hash,body,p.to_string_lossy(),m.dev() as i64,m.ino() as i64,self.epoch,now,now+10000])?;
        let next = if spec.mode == FakeMode::PrepareSymbol {
            "reproduce_initial_defect"
        } else {
            "request_fake_launch"
        };
        tx.execute("UPDATE work SET phase='preparing',version=version+1,next_owner='coordinator',next_action=?1,due_at=?2 WHERE id=?3",params![next,now+10000,work])?;
        crate::attention::on_transition(&tx, &work)?;
        tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'coordinator','attempt.intent',?2,?3,?4)",params![work,id,now,body])?;
        tx.commit()?;
        barrier("after_launch_intent")?;
        attempts(&self.conn)?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or(Error::Invalid("missing committed attempt"))
    }
    pub fn reconcile(&mut self) -> Result<Vec<Attempt>, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_boot = crate::host::boot_id()?;
        let time = crate::store::authority_time(&tx)?;
        for a in attempts(&tx)? {
            let mut rebooted: Option<String> = None;
            // No verified current-boot supervisor holds this attempt's scope.
            let mut unsupervised = false;
            let receipt:Option<(i32,String,String)>=tx.query_row("SELECT supervisor_pid,supervisor_start,supervisor_boot FROM supervisor_command WHERE attempt_id=?1",[&a.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if receipt.is_none() {
                continue;
            }
            let last:Option<String>=tx.query_row("SELECT kind FROM supervisor_fact WHERE attempt_id=?1 ORDER BY seq DESC LIMIT 1",[&a.id],|r|r.get(0)).optional()?;
            // A torn or corrupt lifecycle fact is an evidence gap, never a crash and never
            // cleanup: the first unreadable fact is named and the attempt is unknown.
            let corrupt = first_unreadable_fact(&tx, &a.id)?;
            let (state, reason, next) = match last.as_deref() {
                Some("cleanup_completed") => (
                    "cleaned",
                    "fake cleanup confirmed; work outcome NOT validated",
                    if symbol_mode(&tx, &a.id)? {
                        "seal_candidate"
                    } else {
                        "inspect_fake_result_not_validatable"
                    },
                ),
                Some("unknown") => (
                    "unknown",
                    "unresolved supervision or escaped descendant; notification delivery unavailable",
                    "inspect_supervision_uncertainty",
                ),
                _ => {
                    let (pid, start, boot) = receipt.unwrap();
                    // A receipt from an earlier boot names no live process: classified stale
                    // before any /proc read. The missing exit stays a gap; nothing is relaunched.
                    let stale = boot != current_boot;
                    if stale {
                        rebooted = Some(boot.clone());
                    }
                    let alive = !stale
                        && Identity::read(pid)
                            .is_ok_and(|i| i.start == start && i.boot == boot && i.live());
                    if stale {
                        unsupervised = true;
                        (
                            "unknown",
                            "host rebooted since launch",
                            "inspect_supervision_uncertainty",
                        )
                    } else if alive {
                        match last.as_deref() {
                            Some("launch_accepted") => (
                                "accepted",
                                "launch accepted; workload release unproven",
                                "reconcile_supervisor",
                            ),
                            Some("starting") => (
                                "starting",
                                "blocked wrapper identity recorded; workload release unproven",
                                "reconcile_supervisor",
                            ),
                            Some("started" | "descendant_observed") => (
                                "running",
                                "workload start observed; cleanup pending",
                                "reconcile_supervisor",
                            ),
                            Some("parent_exit_observed") => (
                                "exited",
                                "parent exited; descendant cleanup pending",
                                "reconcile_supervisor",
                            ),
                            Some("stop_requested" | "force_stop_sent") => (
                                "stop_requested",
                                "stop in progress; confirmation pending",
                                "reconcile_supervisor",
                            ),
                            Some("stop_confirmed") => (
                                "stopped",
                                "supported scope stopped; cleanup completion pending",
                                "reconcile_supervisor",
                            ),
                            _ => (
                                "unknown",
                                "receipt present but lifecycle evidence incomplete",
                                "inspect_supervision_uncertainty",
                            ),
                        }
                    } else {
                        unsupervised = true;
                        (
                            "unknown",
                            "supervisor gone; missing durable cleanup/exit evidence",
                            "inspect_supervision_uncertainty",
                        )
                    }
                }
            };
            let termination:Option<String>=tx.query_row("SELECT payload_json FROM supervisor_fact WHERE attempt_id=?1 AND kind='stop_requested' ORDER BY seq DESC LIMIT 1",[&a.id],|r|r.get(0)).optional()?;
            let reason = match &rebooted {
                Some(old) => {
                    format!("{reason} (boot {old} -> {current_boot}); exit unobserved: gap")
                }
                None => reason.to_string(),
            };
            // Without a verified current-boot supervised identity nothing may be signalled:
            // an exceeded (or unverifiable) runtime budget is recorded and, through the
            // 'unknown' routing below, raised to the owner.
            let reason = match unsupervised
                .then(|| budget_note(&tx, &a.id, &time))
                .transpose()?
                .flatten()
            {
                Some(note) => format!("{reason}; {note}"),
                None => reason,
            };
            let reason = if let Some(raw) = termination {
                let value = serde_json::from_str::<serde_json::Value>(&raw).ok();
                format!(
                    "{reason}; termination: {}",
                    match &value {
                        Some(v) => v["reason"].as_str().unwrap_or("unknown"),
                        None => "unreadable",
                    }
                )
            } else {
                reason
            };
            let (state, next, reason) = match &corrupt {
                Some((seq, kind)) => {
                    record_gap(
                        &tx,
                        &a.id,
                        "fact_corrupt",
                        serde_json::json!({"fact_seq": seq, "fact_kind": kind}),
                    )?;
                    (
                        "unknown",
                        "inspect_supervision_uncertainty",
                        format!("{reason}; supervisor fact {seq} ({kind}) unreadable: gap"),
                    )
                }
                None => (state, next, reason),
            };
            // Cleanup was confirmed, but the capture it accounted for is missing or torn: an
            // explicit durable gap. The M1 classification stays; quiescence refuses any pass.
            if state == "cleaned"
                && let Ok(Some((kind, detail))) = crate::validation::capture_gap(&tx, &a.id)
            {
                record_gap(&tx, &a.id, kind, detail)?;
            }
            if let Some(old) = &rebooted {
                // One durable gap per attempt: the exit (and anything after it) was never
                // observed. Effects and artifacts are still inspected below.
                let recorded: i64 = tx.query_row(
                    "SELECT count(*) FROM evidence_gap WHERE attempt_id=?1 AND kind='host_rebooted'",
                    [&a.id],
                    |r| r.get(0),
                )?;
                if recorded == 0 {
                    tx.execute(
                        "INSERT INTO evidence_gap(attempt_id,kind,detail_json,at) VALUES(?1,'host_rebooted',?2,?3)",
                        params![
                            a.id,
                            serde_json::json!({"launch_boot": old, "current_boot": current_boot,
                                               "last_fact": last, "exit": "unobserved"})
                                .to_string(),
                            now_ms()?
                        ],
                    )?;
                }
            }
            if a.state != state || a.reason != reason {
                tx.execute(
                    "UPDATE attempt SET state=?1,reason=?2 WHERE id=?3",
                    params![state, reason, a.id],
                )?;
                let settled = matches!(state, "cleaned" | "unknown");
                // Closed work is never rewritten: the evidence is surfaced post-closure instead.
                if let Some((phase, owner, next)) = crate::attention::route(
                    &tx,
                    &a.work_id,
                    if settled { "blocked" } else { "reconciling" },
                    if settled {
                        "local-owner"
                    } else {
                        "coordinator"
                    },
                    next,
                )? {
                    tx.execute("UPDATE work SET phase=?1,next_owner=?2,next_action=?3,due_at=?4,version=version+1 WHERE id=?5",params![phase,owner,next,now_ms()?+10000,a.work_id])?;
                    crate::attention::on_transition(&tx, &a.work_id)?;
                }
                tx.execute("INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'coordinator','attempt.reconciled',?2,?3,?4)",params![a.work_id,a.id,now_ms()?,serde_json::json!({"state":state,"reason":reason}).to_string()])?;
            }
        }
        tx.commit()?;
        // Task 3 snapshots: missing/changed identity invalidates durably. Lifecycle
        // projection above only writes on genuine transitions, so it cannot overwrite
        // later Task 3 responsibilities or postpone their deadlines.
        self.reconcile_artifacts()?;
        // Task 4: interrupted effects are classified (never repeated) and stale open
        // reviews retired, each only on a genuine transition.
        self.reconcile_effects()?;
        self.sweep_reviews()?;
        // Gate 5: re-derive post-closure discrepancies for every closed work (fresh readback of
        // succeeded effects; resolution only on evidence that a fact no longer holds).
        let closed: Vec<String> = {
            let mut q = self
                .conn
                .prepare("SELECT id FROM work WHERE mode='closed' ORDER BY accepted_at,id")?;
            q.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        for w in closed {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            crate::outcome::sync_discrepancies(&tx, &w)?;
            tx.commit()?;
        }
        attempts(&self.conn)
    }
    pub fn execute(&mut self, id: &str) -> Result<Attempt, Error> {
        self.reconcile()?;
        let a = attempts(&self.conn)?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or(Error::Invalid("unknown attempt"))?;
        // A replacement supervisor explicitly replays its durable receipt. It must
        // never create another wrapper for an accepted (even unknown) command.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if a.state == "intent" {
            // Only a NEW launch is fenced; replaying an accepted receipt (stop/cleanup) is not.
            launch_fence(&tx, id)?;
        }
        tx.execute(
            "UPDATE attempt SET coordinator_epoch=?1 WHERE id=?2",
            params![self.epoch, id],
        )?;
        if a.state == "intent" {
            check_permit(&tx, id, self.epoch)?;
        }
        tx.commit()?;
        self.require_reproduction(id)?;
        let (mut local, remote) = UnixStream::pair()?;
        local.set_read_timeout(Some(Duration::from_secs(75)))?;
        local.set_write_timeout(Some(Duration::from_secs(2)))?;
        let exe = std::env::current_exe()?.with_file_name("supervisor");
        let mut cmd = Command::new(exe);
        cmd.arg(self.root())
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(remote)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        scrub(&mut cmd, self.root());
        #[cfg(feature = "fault-injection")]
        for key in [
            "ASHER_TEST_BARRIER",
            "ASHER_TEST_BARRIER_DIR",
            "ASHER_TEST_FAIL",
            "ASHER_TEST_STORAGE_FAULT",
        ] {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        let mut child = cmd.spawn()?;
        drop(cmd); // close the parent's duplicate peer endpoint; EOF must reflect supervisor exit
        let message = Launch {
            version: 1,
            role: "coordinator".into(),
            attempt_id: id.into(),
            command_id: format!("launch:{id}"),
            fingerprint: a.spec_hash.clone(),
            epoch: self.epoch,
        };
        let io_result = (|| -> Result<(), Error> {
            writeln!(local, "{}", serde_json::to_string(&message)?)?;
            let mut buf = [0; 4096];
            let mut pending = Vec::new();
            let start = Instant::now();
            loop {
                let n = local.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if start.elapsed() > Duration::from_secs(75) {
                    return Err(Error::Invalid("supervisor response timeout"));
                }
                pending.extend_from_slice(&buf[..n]);
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    let response: serde_json::Value = serde_json::from_slice(&line)?;
                    if response.get("started").is_some() && response["durable"] == true {
                        barrier("coordinator_observing")?;
                    }
                }
                if pending.len() > 8192 {
                    return Err(Error::Invalid("oversize supervisor response"));
                }
            }
            Ok(())
        })();
        drop(local); // requests bounded stop before ownership can ever be released
        let end = Instant::now() + Duration::from_secs(4);
        while child.try_wait()?.is_none() {
            if Instant::now() > end {
                child.kill()?;
                child.wait()?;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // No worker can mutate domain state after this borrowed method returns.
        let result = self
            .reconcile()?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or(Error::Invalid("missing attempt"))?;
        if let Err(e) = io_result {
            eprintln!("supervisor transport incomplete: {e}; consult durable attempt");
        }
        Ok(result)
    }
}

//! Gate 5: durable attention, outbox, delivery attempts, manual sweeps and the inbox.
//!
//! Meanings kept separate (permit §2): the attention obligation (`attention`) is never the
//! delivery record. `delivery_attempt` rows say *queued* (none yet), *attempted* (committed
//! before the backend call), *backend-accepted*, *failed* or *delivery-unknown*. Backend
//! acceptance is not human receipt, acknowledgment, decision or resolution, and retries or
//! exhaustion never clear an obligation. No scheduler exists: a sweep runs only when invoked.
use crate::{
    Error, Owner, Reader,
    store::{AuthorityTime, authority_time, barrier, now_ms},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

/// Delay after the Nth failed/unknown attempt: 5 s, 30 s, 2 min, then 15 min.
pub const RETRY_DELAYS_MS: [i64; 4] = [5_000, 30_000, 120_000, 900_000];
/// Attempts allowed per escalation level; at most three levels (0, 1, 2) exist.
pub const MAX_LEVEL_TRIES: i64 = 8;
pub const CLAIM_MS: i64 = 10_000;
pub const ESCALATE_UNDELIVERED_MS: i64 = 5 * 60_000;
pub const ESCALATE_UNACKED_MS: i64 = 15 * 60_000;
pub const SWEEP_STALE_MS: i64 = 5 * 60_000;
pub const SWEEP_BATCH: usize = 20;
/// Absolute wall-clock budget for ONE backend delivery call (connect, handshake, request and
/// reply). Expiry after the request may have been sent records delivery-unknown.
pub const NOTIFY_BUDGET_MS: u64 = 10_000;
/// Absolute wall-clock budget for the delivery phase of one sweep. The sweep holds the
/// exclusive coordinator lock, so a slow or hostile backend must not hold it longer; items not
/// attempted in time stay queued with their obligation open and are reported as deferred.
pub const SWEEP_BUDGET_MS: u64 = 30_000;
/// No new delivery is started with less than this much sweep budget left.
pub const MIN_SEND_WINDOW_MS: u64 = 250;
#[cfg(feature = "fault-injection")]
fn budget_ms(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
#[cfg(not(feature = "fault-injection"))]
fn budget_ms(_var: &str, default: u64) -> u64 {
    default
}
/// Test-only: slow every controlled preparation item (fault-injection builds only).
#[cfg(feature = "fault-injection")]
fn prep_item_delay() {
    let ms = budget_ms("ASHER_TEST_PREP_ITEM_DELAY_MS", 0);
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
#[cfg(not(feature = "fault-injection"))]
fn prep_item_delay() {}
fn notify_budget() -> std::time::Duration {
    std::time::Duration::from_millis(budget_ms("ASHER_TEST_NOTIFY_BUDGET_MS", NOTIFY_BUDGET_MS))
}
fn sweep_budget() -> std::time::Duration {
    std::time::Duration::from_millis(budget_ms("ASHER_TEST_SWEEP_BUDGET_MS", SWEEP_BUDGET_MS))
}
pub const NO_BACKGROUND: &str = "no scheduler or service exists: obligations are only checked and delivered while a sweep command is running";

fn retry_delay(tries: i64) -> i64 {
    RETRY_DELAYS_MS[((tries - 1).max(0) as usize).min(RETRY_DELAYS_MS.len() - 1)]
}
fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn severity_for(action: &str) -> &'static str {
    if [
        "ambiguous",
        "unattributed",
        "conflict",
        "supervision",
        "unknown",
    ]
    .iter()
    .any(|k| action.contains(k))
    {
        "urgent"
    } else {
        "action"
    }
}
fn ev(
    tx: &Connection,
    work: Option<&str>,
    actor: &str,
    kind: &str,
    entity: &str,
    payload: Value,
) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![work, actor, kind, entity, now_ms()?, payload.to_string()],
    )?;
    Ok(())
}

/// Create one attention item and its outbox row, unless a live item with this source key
/// already exists (dedupe). Returns the new ID. Runs inside the caller's transaction, so
/// the item commits or rolls back with the transition that required it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_attention(
    tx: &Connection,
    work: &str,
    review: Option<&str>,
    kind: &str,
    key: &str,
    reason: &str,
    severity: &str,
    blocking: bool,
) -> Result<Option<String>, Error> {
    let exists: bool = tx.query_row(
        "SELECT count(*)>0 FROM attention WHERE source_key=?1 AND state<>'resolved'",
        [key],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(None);
    }
    let now = now_ms()?;
    let id = new_id();
    tx.execute(
        "INSERT INTO attention(id,work_id,review_id,kind,source_key,reason,severity,blocking,state,recipient,created_at,followup_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'open','local-owner',?9,?9)",
        params![id, work, review, kind, key, reason, severity, blocking as i64, now],
    )?;
    tx.execute(
        "INSERT INTO outbox(id,attention_id,channel,state,next_try_at,created_at) VALUES(?1,?2,'desktop','queued',?3,?3)",
        params![new_id(), id, now],
    )?;
    ev(
        tx,
        Some(work),
        "coordinator",
        "attention.created",
        &id,
        json!({"kind": kind, "severity": severity, "blocking": blocking, "reason": reason, "source_key": key}),
    )?;
    Ok(Some(id))
}

/// Resolve one live attention item and cancel its queued delivery. Bookkeeping resolutions
/// run as `coordinator`; owner-authorized ones pass the command ID and `local-owner`.
pub(crate) fn resolve_attention(
    tx: &Connection,
    id: &str,
    resolution: &str,
    by: &str,
    command: Option<&str>,
) -> Result<bool, Error> {
    let now = now_ms()?;
    let n = tx.execute(
        "UPDATE attention SET state='resolved',resolved_at=?1,resolution=?2,resolved_by=?3,resolved_command=?4,version=version+1 WHERE id=?5 AND state<>'resolved'",
        params![now, resolution, by, command, id],
    )?;
    if n == 1 {
        tx.execute(
            "UPDATE outbox SET state='cancelled' WHERE attention_id=?1",
            [id],
        )?;
        let work: String =
            tx.query_row("SELECT work_id FROM attention WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
        ev(
            tx,
            Some(&work),
            by,
            "attention.resolved",
            id,
            json!({"resolution": resolution, "command": command}),
        )?;
    }
    Ok(n == 1)
}
fn live_ids(
    tx: &Connection,
    work: &str,
    kinds: &str,
    except_key: &str,
) -> Result<Vec<String>, Error> {
    let mut q = tx.prepare(&format!(
        "SELECT id FROM attention WHERE work_id=?1 AND state<>'resolved' AND kind IN ({kinds}) AND source_key<>?2 ORDER BY created_at,id"
    ))?;
    Ok(q.query_map(params![work, except_key], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Called inside the same transaction as every genuine work-responsibility change.
/// Owner-directed responsibility gets (or keeps) exactly one live attention item; items for
/// a superseded responsibility are resolved as superseded in the SAME transaction, and a
/// changed deadline retires the overdue item for the old one.
pub(crate) fn on_transition(tx: &Connection, work: &str) -> Result<(), Error> {
    let (mode, phase, owner, action): (String, String, String, String) = tx.query_row(
        "SELECT mode,phase,next_owner,next_action FROM work WHERE id=?1",
        [work],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if mode == "closed" {
        return Ok(());
    }
    let key = format!("resp:{work}:{action}");
    let why = format!("superseded: responsibility is now {phase}/{owner}/{action}");
    for id in live_ids(tx, work, "'overdue'", "")? {
        resolve_attention(tx, &id, &why, "coordinator", None)?;
    }
    if owner == "local-owner" {
        for id in live_ids(tx, work, "'responsibility'", &key)? {
            resolve_attention(tx, &id, &why, "coordinator", None)?;
        }
        ensure_owner_attention(tx, work)?;
    } else {
        for id in live_ids(tx, work, "'responsibility'", "")? {
            resolve_attention(tx, &id, &why, "coordinator", None)?;
        }
    }
    Ok(())
}

/// The responsibility a cancellation request leaves until the owner closes the work.
pub(crate) const CANCEL_ACTION: &str = "cancellation_requested_account_and_close";
/// Forward-progress responsibilities. After a cancellation request none of them may become
/// the work's next responsibility again (the request is a persistent fence, never withdrawn
/// implicitly); inspection and hazard responsibilities are kept as they are.
const FORWARD_ACTIONS: [&str; 7] = [
    "request_fake_launch",
    "reproduce_initial_defect",
    "seal_candidate",
    "validate_candidate",
    "create_exact_review",
    "decide_exact_review",
    "dispatch_approved_effect",
];

pub(crate) fn cancel_requested(tx: &Connection, work: &str) -> Result<bool, Error> {
    Ok(tx.query_row(
        "SELECT count(*)>0 FROM cancel_request WHERE work_id=?1",
        [work],
        |r| r.get(0),
    )?)
}

/// Refuse any forward progress (new attempt, launch, reproduction, seal, validation, review,
/// decision, dispatch) on closed work or after a recorded cancellation request.
pub(crate) fn progress_fence(tx: &Connection, work: &str) -> Result<(), Error> {
    let mode: String = tx.query_row("SELECT mode FROM work WHERE id=?1", [work], |r| r.get(0))?;
    if mode != "active" {
        return Err(Error::Rejected(
            "work is closed: no launch, preparation, review, decision or dispatch is possible"
                .into(),
        ));
    }
    if cancel_requested(tx, work)? {
        return Err(Error::Rejected(
            "cancellation was requested for this work: forward progress is fenced; account for it and close (cancelled, failed or abandoned)".into(),
        ));
    }
    Ok(())
}

/// Route a proposed responsibility change. Closed work is immutable: later (adverse)
/// evidence is recorded as an event and accounted as per-fact discrepancy responsibilities
/// (`outcome::sync_discrepancies`) instead of a write the `work_closed_immutable` trigger would
/// abort (which would wedge every coordinator startup). After a cancellation request, forward-progress responsibilities are
/// replaced by the cancellation responsibility. Returns `None` when the work row must not change.
pub(crate) fn route(
    tx: &Connection,
    work: &str,
    phase: &str,
    owner: &str,
    action: &str,
) -> Result<Option<(String, String, String)>, Error> {
    let mode: String = tx.query_row("SELECT mode FROM work WHERE id=?1", [work], |r| r.get(0))?;
    if mode == "closed" {
        ev(
            tx,
            Some(work),
            "coordinator",
            "work.post_closure_evidence",
            work,
            json!({"proposed": {"phase": phase, "next_owner": owner, "next_action": action},
                   "work_unchanged": true, "outcome_unchanged": true}),
        )?;
        // Each adverse fact becomes (or stays) its own durable discrepancy responsibility.
        crate::outcome::sync_discrepancies(tx, work)?;
        return Ok(None);
    }
    if FORWARD_ACTIONS.contains(&action) && cancel_requested(tx, work)? {
        return Ok(Some((
            "blocked".into(),
            "local-owner".into(),
            CANCEL_ACTION.into(),
        )));
    }
    Ok(Some((phase.into(), owner.into(), action.into())))
}

/// Attach the just-created review to the live decide-review attention (set once; the
/// review row does not exist yet when the responsibility transition runs).
pub(crate) fn link_review(tx: &Connection, work: &str, review: &str) -> Result<(), Error> {
    tx.execute(
        "UPDATE attention SET review_id=?1 WHERE work_id=?2 AND kind='responsibility' AND state<>'resolved' AND review_id IS NULL AND source_key=?3",
        params![review, work, format!("resp:{work}:decide_exact_review")],
    )?;
    Ok(())
}

/// Create the live responsibility item for owner-directed active work if it is missing.
/// Never resolves anything (safe to call from a sweep).
pub(crate) fn ensure_owner_attention(tx: &Connection, work: &str) -> Result<bool, Error> {
    let (mode, phase, owner, action): (String, String, String, String) = tx.query_row(
        "SELECT mode,phase,next_owner,next_action FROM work WHERE id=?1",
        [work],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if mode != "active" || owner != "local-owner" {
        return Ok(false);
    }
    let review: Option<String> = if action == "decide_exact_review" {
        tx.query_row(
            "SELECT id FROM review WHERE work_id=?1 AND state='pending' ORDER BY created_at DESC LIMIT 1",
            [work],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    Ok(create_attention(
        tx,
        work,
        review.as_deref(),
        "responsibility",
        &format!("resp:{work}:{action}"),
        &if action != CANCEL_ACTION && cancel_requested(tx, work)? {
            // An inspection/hazard responsibility owns the work, but the request stays visible.
            format!("{action} ({phase}); CANCELLATION REQUESTED: account for it and close")
        } else {
            format!("{action} ({phase})")
        },
        severity_for(&action),
        true,
    )?
    .is_some())
}

// ---------------------------------------------------------------- backends
/// What a backend call established. `Unknown` means the display may or may not have happened.
pub enum SendResult {
    Accepted(Value),
    Failed(String),
    Unknown(String),
}
pub struct Message {
    pub attention_id: String,
    pub work_id: String,
    pub kind: String,
    pub severity: String,
    pub ordinal: i64,
    pub summary: String,
    pub body: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Desktop,
    #[cfg(feature = "fault-injection")]
    Sink,
}
impl Backend {
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "desktop" => Ok(Self::Desktop),
            #[cfg(feature = "fault-injection")]
            "sink" => Ok(Self::Sink),
            #[cfg(not(feature = "fault-injection"))]
            "sink" => Err(Error::Invalid(
                "the deterministic test sink is not available in ordinary builds",
            )),
            _ => Err(Error::Invalid("unknown backend; use desktop")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            #[cfg(feature = "fault-injection")]
            Self::Sink => "test_sink",
        }
    }
    fn send(self, m: &Message, deadline: std::time::Instant) -> SendResult {
        match self {
            Self::Desktop => crate::dbus::notify(m, deadline),
            #[cfg(feature = "fault-injection")]
            Self::Sink => sink_send(m, deadline),
        }
    }
}
/// Deterministic sink (fault-injection builds only). Mode file `<dir>/mode`: `accept`
/// (default), `offline`, `lose_receipt`, `duplicate`. Displays are appended to
/// `<dir>/delivered.jsonl`, the independent test oracle.
#[cfg(feature = "fault-injection")]
fn sink_send(m: &Message, deadline: std::time::Instant) -> SendResult {
    use std::io::Write;
    let Some(dir) = std::env::var_os("ASHER_TEST_SINK_DIR") else {
        return SendResult::Failed("test sink directory not configured".into());
    };
    let dir = std::path::PathBuf::from(dir);
    let mode = std::fs::read_to_string(dir.join("mode"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "accept".into());
    if mode == "offline" {
        return SendResult::Failed("sink offline".into());
    }
    // `slow:<ms>`: a backend that takes <ms> before displaying; it honours the deadline like
    // the desktop client (request already handed over, so expiry is delivery-unknown).
    if let Some(ms) = mode
        .strip_prefix("slow:")
        .and_then(|v| v.parse::<u64>().ok())
    {
        let want = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        std::thread::sleep(
            want.min(deadline)
                .saturating_duration_since(std::time::Instant::now()),
        );
        if want > deadline {
            return SendResult::Unknown("slow backend: time budget spent before a reply".into());
        }
    }
    let line = json!({"attention_id": m.attention_id, "work_id": m.work_id, "kind": m.kind,
        "severity": m.severity, "ordinal": m.ordinal, "summary": m.summary, "body": m.body})
    .to_string();
    let copies = if mode == "duplicate" { 2 } else { 1 };
    for _ in 0..copies {
        let r = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("delivered.jsonl"))
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = r {
            return SendResult::Failed(format!("sink write failed: {e}"));
        }
    }
    if mode == "lose_receipt" {
        return SendResult::Unknown("receipt lost after display".into());
    }
    SendResult::Accepted(json!({"sink": "test_sink", "displays": copies}))
}

fn message_for(
    tx: &Connection,
    attention: &str,
    ordinal: i64,
    severity: &str,
) -> Result<Message, Error> {
    let (work, kind, reason, review): (String, String, String, Option<String>) = tx.query_row(
        "SELECT work_id,kind,reason,review_id FROM attention WHERE id=?1",
        [attention],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let due: Option<i64> = tx
        .query_row(
            "SELECT due_at FROM work WHERE id=?1 AND mode='active'",
            [&work],
            |r| r.get(0),
        )
        .optional()?;
    let head = match (kind.as_str(), severity) {
        (_, _) if reason.starts_with("DISCREPANCY") => {
            "Asher Bridge: URGENT discrepancy in a closed outcome"
        }
        ("outcome", _) if reason.starts_with("NOT SUCCESS") => {
            "Asher Bridge: work closed, NOT SUCCESS"
        }
        ("outcome", _) => "Asher Bridge: work closed",
        ("overdue", _) => "Asher Bridge: OVERDUE obligation",
        (_, "urgent") => "Asher Bridge: URGENT owner action needed",
        _ => "Asher Bridge: owner action needed",
    };
    // Short, redacted, no secrets, and no authority: it only names where to look.
    let body = format!(
        "{reason}\nattention {attention}\nwork {work}{}{}\nOpen the inbox to act; this notice grants no approval.",
        review.map(|r| format!("\nreview {r}")).unwrap_or_default(),
        due.map(|d| format!("\ndeadline (unix ms) {d}"))
            .unwrap_or_default(),
    );
    Ok(Message {
        attention_id: attention.into(),
        work_id: work,
        kind,
        severity: severity.into(),
        ordinal,
        summary: head.into(),
        body,
    })
}

// ---------------------------------------------------------------- sweep
/// The value a sweep compares deadlines against (snooze end, delivery claim, retry time,
/// responsibility deadline, escalation age). A known time is itself; an uncertain time (wall
/// clock behind the durable floor, or unreadable) makes every deadline count as reached, as
/// `AuthorityTime::reached` does, so a rollback can never extend a snooze or a claim or hide an
/// overdue deadline. Record stamps keep using `AuthorityTime::stamp`.
fn deadline_clock(t: &AuthorityTime) -> i64 {
    t.known().unwrap_or(i64::MAX / 2)
}

impl Owner {
    /// One explicit, bounded obligation + delivery sweep. Nothing here runs unattended.
    pub fn sweep(&mut self, backend: Backend) -> Result<Value, Error> {
        let started = now_ms()?;
        let epoch = self.epoch;
        // ONE deadline bounds the whole sweep: preparation (stale attempts, attention backfill,
        // overdue recording, escalation) and delivery. Every phase works in bounded batches,
        // each committed in its own transaction (durable partial progress), and stops once the
        // deadline has passed. At least one batch runs per sweep, so repeated sweeps always
        // progress; anything not reached stays exactly as it was (obligations retained).
        let sweep_deadline = std::time::Instant::now() + sweep_budget();
        let mut did_work = false;
        let mut stopped_in: Option<&str> = None;
        let out_of_time = |did: bool| did && std::time::Instant::now() >= sweep_deadline;
        let mut stale_unknown = 0i64;
        let mut overdue_recorded = 0i64;
        let mut escalations = 0i64;
        let mut backfilled = 0i64;
        let batch = SWEEP_BATCH as i64;
        // 1. In-doubt attempts whose claim expired become delivery-unknown (never accepted).
        loop {
            if out_of_time(did_work) {
                stopped_in = Some("stale_attempts");
                break;
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let t = authority_time(&tx)?;
            let now = t.stamp();
            let check = deadline_clock(&t);
            let stale: Vec<(String, String)> = {
                let mut q = tx.prepare(
                    "SELECT id,outbox_id FROM delivery_attempt WHERE state='attempted' AND claim_until<=?1 ORDER BY started_at,id LIMIT ?2",
                )?;
                q.query_map(params![check, batch], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            if stale.is_empty() {
                break;
            }
            for (aid, oid) in &stale {
                // The budget is checked before every item, not only between batches.
                if out_of_time(did_work) {
                    stopped_in = Some("stale_attempts");
                    break;
                }
                tx.execute(
                    "UPDATE delivery_attempt SET state='unknown',finished_at=?1,error='claim expired without a recorded receipt' WHERE id=?2 AND state='attempted'",
                    params![now, aid],
                )?;
                let level_tries: i64 =
                    tx.query_row("SELECT level_tries FROM outbox WHERE id=?1", [oid], |r| {
                        r.get(0)
                    })?;
                let st = if level_tries >= MAX_LEVEL_TRIES {
                    "exhausted"
                } else {
                    "retry_wait"
                };
                tx.execute(
                    "UPDATE outbox SET state=?1,next_try_at=?2 WHERE id=?3 AND state NOT IN ('cancelled','accepted')",
                    params![st, now, oid],
                )?;
                stale_unknown += 1;
                did_work = true;
            }
            tx.commit()?; // completed items persist even when the budget cut the batch
            if stopped_in.is_some() {
                break;
            }
        }
        // 2+3. Every active work has its named responsibility; owner-directed work has attention;
        // overdue is RECORDED, never repaired (deadline and version untouched). Only works that
        // still need one of these are selected, so a later sweep continues where this one stopped.
        while stopped_in.is_none() {
            if out_of_time(did_work) {
                stopped_in = Some("attention_and_overdue");
                break;
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let t = authority_time(&tx)?;
            let now = t.stamp();
            let check = deadline_clock(&t);
            let works: Vec<(String, i64)> = {
                let mut q = tx.prepare(
                    "SELECT w.id,w.due_at FROM work w WHERE w.mode='active' AND (
                        (w.next_owner='local-owner' AND NOT EXISTS (SELECT 1 FROM attention a WHERE a.source_key='resp:'||w.id||':'||w.next_action AND a.state<>'resolved'))
                     OR (w.due_at<?1 AND NOT EXISTS (SELECT 1 FROM attention a WHERE a.source_key='overdue:'||w.id||':'||w.due_at AND a.state<>'resolved')))
                     ORDER BY w.accepted_at,w.id LIMIT ?2",
                )?;
                q.query_map(params![check, batch], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            if works.is_empty() {
                break;
            }
            for (w, due) in &works {
                if out_of_time(did_work) {
                    stopped_in = Some("attention_and_overdue");
                    break;
                }
                prep_item_delay();
                did_work = true;
                if ensure_owner_attention(&tx, w)? {
                    backfilled += 1;
                }
                if *due < check {
                    let key = format!("overdue:{w}:{due}");
                    let reason = format!("overdue: responsibility deadline {due} passed");
                    if create_attention(&tx, w, None, "overdue", &key, &reason, "urgent", true)?
                        .is_some()
                    {
                        overdue_recorded += 1;
                        ev(
                            &tx,
                            Some(w),
                            "coordinator",
                            "work.overdue_recorded",
                            w,
                            json!({"due_at": due, "recorded_at": now}),
                        )?;
                    }
                }
            }
            tx.commit()?;
        }
        // 4. Escalation (recorded even while snoozed; delivery is deferred by the snooze).
        // Candidates are fixed once (one level per item per sweep), then handled in batches.
        let cands: Vec<String> = if stopped_in.is_none() {
            let now = deadline_clock(&authority_time(&self.conn)?);
            let mut q = self.conn.prepare(
                "SELECT a.id FROM attention a WHERE a.state='open' AND a.blocking=1 AND a.escalation_level<2 AND (
                    (a.escalation_level=0 AND ?1-a.created_at>=?2 AND NOT EXISTS (SELECT 1 FROM delivery_attempt d WHERE d.attention_id=a.id AND d.state='accepted'))
                 OR ?1-a.created_at>=?3) ORDER BY a.created_at,a.id",
            )?;
            q.query_map(
                params![now, ESCALATE_UNDELIVERED_MS, ESCALATE_UNACKED_MS],
                |r| r.get(0),
            )?
            .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        for chunk in cands.chunks(SWEEP_BATCH) {
            if out_of_time(did_work) {
                stopped_in = Some("escalation");
                break;
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let t = authority_time(&tx)?;
            let now = t.stamp();
            for id in chunk {
                if out_of_time(did_work) {
                    stopped_in = Some("escalation");
                    break;
                }
                did_work = true;
                prep_item_delay();
                let row: Option<(i64, Option<i64>, String)> = tx
                    .query_row(
                        "SELECT escalation_level,snoozed_until,work_id FROM attention WHERE id=?1 AND state='open' AND escalation_level<2",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let Some((level, snoozed, work)) = row else {
                    continue;
                };
                tx.execute(
                    "UPDATE attention SET escalation_level=escalation_level+1,severity='urgent' WHERE id=?1",
                    [id],
                )?;
                // A snooze defers delivery only while its end is provably still ahead; under an
                // uncertain time it counts as ended (a rollback never extends a snooze).
                let snooze_running = snoozed.is_some_and(|s| !t.reached(s));
                let next_try = match snoozed {
                    Some(s) if snooze_running => s.max(now),
                    _ => now,
                };
                tx.execute(
                    "UPDATE outbox SET state='retry_wait',level_tries=0,next_try_at=?1 WHERE attention_id=?2 AND state IN ('queued','retry_wait','accepted','exhausted')
                       AND NOT EXISTS (SELECT 1 FROM delivery_attempt d WHERE d.outbox_id=outbox.id AND d.state='attempted')",
                    params![next_try, id],
                )?;
                ev(
                    &tx,
                    Some(&work),
                    "coordinator",
                    "attention.escalated",
                    id,
                    json!({
                    "level": level + 1, "channel": "desktop", "recipient": "local-owner",
                    "delivery_deferred_until": next_try, "snoozed": snooze_running}),
                )?;
                escalations += 1;
            }
            tx.commit()?;
            if stopped_in.is_some() {
                break;
            }
        }
        // 5. Delivery: bounded batch AND bounded time. The attempt is committed BEFORE the
        // backend call; each call has an absolute deadline inside the sweep's own budget.
        let mut deferred_by_budget = 0i64;
        let mut attempts = Vec::new();
        let due: Vec<String> = {
            let now = deadline_clock(&authority_time(&self.conn)?);
            let mut q = self.conn.prepare(
                "SELECT o.id FROM outbox o JOIN attention a ON a.id=o.attention_id
                  WHERE o.state IN ('queued','retry_wait') AND o.next_try_at<=?1 AND a.state='open'
                    AND (a.snoozed_until IS NULL OR a.snoozed_until<=?1)
                  ORDER BY o.next_try_at,a.created_at,o.id LIMIT ?2",
            )?;
            q.query_map(params![now, SWEEP_BATCH as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let due_count = due.len();
        for (n, oid) in due.into_iter().enumerate() {
            let left = sweep_deadline.saturating_duration_since(std::time::Instant::now());
            if left < std::time::Duration::from_millis(MIN_SEND_WINDOW_MS) {
                // Not attempted: stays queued, obligation open, reported below.
                deferred_by_budget = (due_count - n) as i64;
                break;
            }
            let call_deadline = sweep_deadline.min(std::time::Instant::now() + notify_budget());
            let (attempt_id, msg, ordinal) = {
                let tx = self
                    .conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                let t = authority_time(&tx)?;
                let now = t.stamp();
                let check = deadline_clock(&t);
                let row: Option<(String, i64, i64, String)> = tx
                    .query_row(
                        "SELECT o.attention_id,o.tries,o.level_tries,a.severity FROM outbox o JOIN attention a ON a.id=o.attention_id
                          WHERE o.id=?1 AND o.state IN ('queued','retry_wait') AND o.next_try_at<=?2 AND a.state='open'
                            AND (a.snoozed_until IS NULL OR a.snoozed_until<=?2)",
                        params![oid, check],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((att, tries, level_tries, severity)) = row else {
                    continue;
                };
                if level_tries >= MAX_LEVEL_TRIES {
                    tx.execute("UPDATE outbox SET state='exhausted' WHERE id=?1", [&oid])?;
                    tx.commit()?;
                    continue;
                }
                let ordinal = tries + 1;
                let attempt_id = new_id();
                tx.execute(
                    "INSERT INTO delivery_attempt(id,outbox_id,attention_id,ordinal,backend,severity,state,started_at,claim_until,coordinator_epoch) VALUES(?1,?2,?3,?4,?5,?6,'attempted',?7,?8,?9)",
                    params![attempt_id, oid, att, ordinal, backend.name(), severity, now, now + CLAIM_MS, epoch],
                )?;
                tx.execute(
                    "UPDATE outbox SET tries=tries+1,level_tries=level_tries+1,next_try_at=?1 WHERE id=?2",
                    params![now + CLAIM_MS, oid],
                )?;
                let msg = message_for(&tx, &att, ordinal, &severity)?;
                tx.commit()?;
                (attempt_id, msg, ordinal)
            };
            barrier("after_attempt_recorded")?;
            let result = backend.send(&msg, call_deadline);
            barrier("after_backend_send")?;
            let (state, receipt, error) = match result {
                SendResult::Accepted(r) => ("accepted", Some(r), None),
                SendResult::Failed(e) => ("failed", None, Some(e)),
                SendResult::Unknown(e) => ("unknown", None, Some(e)),
            };
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = now_ms()?;
            tx.execute(
                "UPDATE delivery_attempt SET state=?1,finished_at=?2,receipt_json=?3,error=?4 WHERE id=?5 AND state='attempted'",
                params![state, now, receipt.as_ref().map(|r| r.to_string()), error, attempt_id],
            )?;
            let (tries, level_tries): (i64, i64) = tx.query_row(
                "SELECT tries,level_tries FROM outbox WHERE id=?1",
                [&oid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if state == "accepted" {
                tx.execute(
                    "UPDATE outbox SET state='accepted',next_try_at=?1 WHERE id=?2 AND state NOT IN ('cancelled')",
                    params![now, oid],
                )?;
            } else if level_tries >= MAX_LEVEL_TRIES {
                tx.execute(
                    "UPDATE outbox SET state='exhausted' WHERE id=?1 AND state NOT IN ('cancelled')",
                    [&oid],
                )?;
            } else {
                tx.execute(
                    "UPDATE outbox SET state='retry_wait',next_try_at=?1 WHERE id=?2 AND state NOT IN ('cancelled')",
                    params![now + retry_delay(tries), oid],
                )?;
            }
            ev(
                &tx,
                Some(&msg.work_id),
                "coordinator",
                "delivery.attempt",
                &msg.attention_id,
                json!({
                "attempt_id": attempt_id, "ordinal": ordinal, "backend": backend.name(), "state": state,
                "meaning": "backend result only; not human receipt, acknowledgment, decision or resolution"}),
            )?;
            tx.commit()?;
            attempts.push(json!({"attention_id": msg.attention_id, "ordinal": ordinal,
                "backend": backend.name(), "state": state, "error": error}));
        }
        let finished = now_ms()?;
        let summary = json!({
            "started_at": started, "finished_at": finished, "backend": backend.name(),
            "overdue_recorded": overdue_recorded, "escalations": escalations,
            "attention_backfilled": backfilled, "stale_attempts_marked_unknown": stale_unknown,
            "attempts": attempts,
            "budget_ms": {"sweep": sweep_budget().as_millis() as u64, "per_delivery": notify_budget().as_millis() as u64},
            "complete": stopped_in.is_none() && deferred_by_budget == 0,
            "stopped_in": stopped_in.or((deferred_by_budget > 0).then_some("delivery")),
            "budget_exhausted": stopped_in.is_some() || deferred_by_budget > 0, "deferred_by_budget": deferred_by_budget,
            "background_progress": false, "note": NO_BACKGROUND,
        });
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO sweep_run(started_at,finished_at,coordinator_epoch,backend,summary_json) VALUES(?1,?2,?3,?4,?5)",
            params![started, finished, epoch, backend.name(), summary.to_string()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        let mut out = summary;
        out["sweep_id"] = json!(id);
        Ok(out)
    }
}

// ---------------------------------------------------------------- inbox (read-only)
/// ordinal, state, backend, started_at, finished_at, error
type AttemptRow = (i64, String, String, i64, Option<i64>, Option<String>);

impl Reader {
    /// Durable inbox: live attention with delivery facts kept separate, work state, the last
    /// recorded sweep and closed outcomes. Computed at read time; performs no background work.
    pub fn inbox(&self) -> Result<Value, Error> {
        let c = self.conn();
        let version: i64 = c.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
        if version < 6 {
            return Err(Error::Invalid(
                "ledger predates schema 6 (attention); run a mutating command such as sweep to migrate it",
            ));
        }
        let now = now_ms()?;
        let mut q = c.prepare(
            "SELECT a.id,a.work_id,a.review_id,a.kind,a.severity,a.blocking,a.state,a.reason,a.version,a.escalation_level,a.created_at,a.followup_at,a.snoozed_until,
                    w.mode,w.phase,w.next_owner,w.next_action,w.due_at,w.version
               FROM attention a JOIN work w ON w.id=a.work_id WHERE a.state<>'resolved' ORDER BY a.created_at,a.id",
        )?;
        let rows: Vec<Value> = q
            .query_map([], |r| {
                let mode: String = r.get(13)?;
                let due: i64 = r.get(17)?;
                Ok(json!({
                    "attention_id": r.get::<_, String>(0)?, "work_id": r.get::<_, String>(1)?,
                    "review_id": r.get::<_, Option<String>>(2)?, "kind": r.get::<_, String>(3)?,
                    "severity": r.get::<_, String>(4)?, "blocking": r.get::<_, i64>(5)? == 1,
                    "state": r.get::<_, String>(6)?, "reason": r.get::<_, String>(7)?,
                    "version": r.get::<_, i64>(8)?, "escalation_level": r.get::<_, i64>(9)?,
                    "created_at": r.get::<_, i64>(10)?, "followup_at": r.get::<_, i64>(11)?,
                    "snoozed_until": r.get::<_, Option<i64>>(12)?,
                    "human_receipt": "not established",
                    "work": {"mode": mode, "phase": r.get::<_, String>(14)?, "next_owner": r.get::<_, String>(15)?,
                             "next_action": r.get::<_, String>(16)?, "due_at": due,
                             "overdue": mode == "active" && due < now, "version": r.get::<_, i64>(18)?},
                }))
            })?
            .collect::<Result<_, _>>()?;
        let mut items = Vec::new();
        for mut it in rows {
            let id = it["attention_id"].as_str().unwrap_or_default().to_string();
            let mut aq = c.prepare(
                "SELECT ordinal,state,backend,started_at,finished_at,error FROM delivery_attempt WHERE attention_id=?1 ORDER BY ordinal",
            )?;
            let atts: Vec<AttemptRow> = aq
                .query_map([&id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            let accepted = atts.iter().any(|a| a.1 == "accepted");
            let status = match atts.last() {
                None => "queued",
                Some(a) => match a.1.as_str() {
                    "attempted" => "attempted",
                    "accepted" => "backend_accepted",
                    "failed" => "failed",
                    _ => "delivery_unknown",
                },
            };
            let outbox: Option<(String, i64)> = c
                .query_row(
                    "SELECT state,next_try_at FROM outbox WHERE attention_id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            it["delivery"] = json!({
                "status": status, "attempts": atts.len(), "backend_accepted": accepted,
                "outbox_state": outbox.as_ref().map(|o| o.0.clone()),
                "next_try_at": outbox.map(|o| o.1),
                "last_attempt": atts.last().map(|a| json!({"ordinal": a.0, "state": a.1, "backend": a.2,
                    "started_at": a.3, "finished_at": a.4, "error": a.5})),
            });
            items.push(it);
        }
        let last: Option<i64> =
            c.query_row("SELECT MAX(finished_at) FROM sweep_run", [], |r| r.get(0))?;
        let mut oq = c.prepare(
            "SELECT work_id,outcome,reason,closed_at,notice_attention_id FROM work_outcome ORDER BY closed_at,work_id",
        )?;
        let outcomes: Vec<Value> = oq
            .query_map([], |r| {
                let outcome: String = r.get(1)?;
                Ok(json!({"work_id": r.get::<_, String>(0)?, "outcome": outcome,
                    "success": outcome == "succeeded", "reason": r.get::<_, String>(2)?,
                    "closed_at": r.get::<_, i64>(3)?, "notice_attention_id": r.get::<_, String>(4)?}))
            })?
            .collect::<Result<_, _>>()?;
        // Challenged-outcome view: the historical record is unchanged, but open post-closure
        // discrepancy responsibilities are shown next to it, never hidden behind `success`.
        let mut outcomes = outcomes;
        for o in outcomes.iter_mut() {
            let w = o["work_id"].as_str().unwrap_or_default().to_string();
            let n: i64 = c.query_row(
                "SELECT count(*) FROM attention WHERE work_id=?1 AND state<>'resolved' AND substr(source_key,1,length(?2))=?2",
                params![w, format!("discrepancy:{w}:")],
                |r| r.get(0),
            )?;
            o["open_discrepancies"] = json!(n);
            o["challenged"] = json!(n > 0);
        }
        Ok(json!({
            "now": now, "last_sweep_at": last,
            "sweep_stale": last.is_none_or(|l| now - l > SWEEP_STALE_MS),
            "sweep_stale_after_ms": SWEEP_STALE_MS,
            "background_progress": false, "note": NO_BACKGROUND,
            "items": items, "closed_outcomes": outcomes,
        }))
    }
}

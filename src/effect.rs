//! Task 4: dispatch claim, native fixture-ref compare-and-swap, independent readback and
//! four-way reconciliation. No SQLite transaction is held across the ref mutation, and
//! there is NO exactly-once claim across SQLite and Git: effect identity is persisted
//! before the action and an interrupted effect is reconciled, never blindly repeated.
use crate::{
    Error, Owner,
    gitfixture::{self, Cas, Repo},
    process_scope::Identity,
    protocol::{fingerprint, valid_id},
    review::{Action, Envelope, Stale, authority_current, candidate_objects, load_review},
    store::{barrier, journal, now_ms},
    validation::{event, set_responsibility},
};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};

pub const RESULT_NOT_COMPLETION: &str = "an action result is not work completion; closing the work needs an authenticated owner `close` command against `closure-summary`";

/// Reassert the durable responsibility attached to quarantined schema-5 legacy effects.
/// This also repairs databases migrated by an earlier schema-5 build. The deadline is preserved,
/// and once the row agrees, repeated opens/status projections are no-ops.
pub(crate) fn reconcile_legacy_quarantine(conn: &mut Connection) -> Result<(), Error> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    const STALE_ACTION: &str = "action_applied_verify_and_close_task5_not_implemented";
    const INSPECT_ACTION: &str = "inspect_legacy_unattributed_confirmation";
    let candidates: Vec<(String, String, String)> = {
        let mut q = tx.prepare(
            "SELECT w.id,w.phase,w.next_owner FROM work w
              WHERE w.next_action=?1
                AND EXISTS (
                  SELECT 1 FROM effect e
                    JOIN event quarantine ON quarantine.entity_id=e.id AND quarantine.work_id=e.work_id
                   WHERE e.work_id=w.id AND e.state='confirmed'
                     AND quarantine.kind='effect.legacy_confirmation_unattributed'
                     AND COALESCE((SELECT json_extract(o.payload_json,'$.attribution')
                                     FROM effect_observation o WHERE o.effect_id=e.id
                                    ORDER BY o.observed_at DESC,o.rowid DESC LIMIT 1),'')
                         NOT IN ('audit_entry','executor_reported_cas')
                )",
        )?;
        q.query_map([STALE_ACTION], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    for (work_id, phase, owner) in candidates {
        let changed = tx.execute(
            "UPDATE work SET next_action=?1,version=version+1
              WHERE id=?2 AND next_action=?3
                AND EXISTS (
                  SELECT 1 FROM effect e
                    JOIN event quarantine ON quarantine.entity_id=e.id AND quarantine.work_id=e.work_id
                   WHERE e.work_id=work.id AND e.state='confirmed'
                     AND quarantine.kind='effect.legacy_confirmation_unattributed'
                     AND COALESCE((SELECT json_extract(o.payload_json,'$.attribution')
                                     FROM effect_observation o WHERE o.effect_id=e.id
                                    ORDER BY o.observed_at DESC,o.rowid DESC LIMIT 1),'')
                         NOT IN ('audit_entry','executor_reported_cas')
                )",
            params![INSPECT_ACTION, work_id, STALE_ACTION],
        )?;
        if changed == 0 {
            continue;
        }
        crate::attention::on_transition(&tx, &work_id)?;
        let effect_ids: Vec<String> = {
            let mut q = tx.prepare(
                "SELECT DISTINCT e.id FROM effect e
                   JOIN event quarantine ON quarantine.entity_id=e.id AND quarantine.work_id=e.work_id
                  WHERE e.work_id=?1 AND e.state='confirmed'
                    AND quarantine.kind='effect.legacy_confirmation_unattributed'
                    AND COALESCE((SELECT json_extract(o.payload_json,'$.attribution')
                                    FROM effect_observation o WHERE o.effect_id=e.id
                                   ORDER BY o.observed_at DESC,o.rowid DESC LIMIT 1),'')
                        NOT IN ('audit_entry','executor_reported_cas')
                  ORDER BY e.id",
            )?;
            q.query_map([&work_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        event(
            &tx,
            &work_id,
            "work.legacy_quarantine_responsibility_repaired",
            &work_id,
            json!({
                "quarantined_effect_ids": effect_ids,
                "previous_phase": phase,
                "previous_owner": owner,
                "previous_next_action": STALE_ACTION,
                "next_phase": phase,
                "next_owner": owner,
                "next_action": INSPECT_ACTION,
                "deadline_preserved": true
            }),
        )?;
    }
    // Gate 5 exists now: any remaining (attributed) work on the schema-5 action that claims
    // closure is "not implemented" gets the current wording by an audited, versioned
    // transition. Phase, owner and deadline are preserved; the quarantine repair above ran
    // first, so unattributed confirmations keep their inspection responsibility.
    const CURRENT_ACTION: &str = "action_applied_review_closure_summary_and_close";
    let stale: Vec<(String, String, String)> = {
        let mut q = tx.prepare(
            "SELECT id,phase,next_owner FROM work WHERE mode='active' AND next_action=?1 ORDER BY id",
        )?;
        q.query_map([STALE_ACTION], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    for (work_id, phase, owner) in stale {
        if tx.execute(
            "UPDATE work SET next_action=?1,version=version+1 WHERE id=?2 AND next_action=?3",
            params![CURRENT_ACTION, work_id, STALE_ACTION],
        )? == 0
        {
            continue;
        }
        crate::attention::on_transition(&tx, &work_id)?;
        event(
            &tx,
            &work_id,
            "work.legacy_wording_repaired",
            &work_id,
            json!({
                "previous_next_action": STALE_ACTION,
                "next_action": CURRENT_ACTION,
                "phase": phase, "next_owner": owner,
                "deadline_preserved": true
            }),
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectRow {
    pub id: String,
    pub work_id: String,
    pub review_id: String,
    pub decision_id: String,
    pub repo_id: String,
    pub state: String,
    pub state_reason: String,
    pub action_json: String,
    pub claim_epoch: Option<i64>,
    pub executor_json: Option<String>,
    pub claim_ref: Option<String>,
    pub claim_ref_ident: Option<String>,
    pub claim_reflog_len: Option<i64>,
    pub claim_reflog_hash: Option<String>,
    pub result_observation_id: Option<String>,
}
const EFFECT_COLUMNS: &str = "id,work_id,review_id,decision_id,repo_id,state,state_reason,action_json,claim_epoch,executor_json,claim_ref,claim_ref_ident,claim_reflog_len,claim_reflog_hash,result_observation_id";
fn effect_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EffectRow> {
    Ok(EffectRow {
        id: r.get(0)?,
        work_id: r.get(1)?,
        review_id: r.get(2)?,
        decision_id: r.get(3)?,
        repo_id: r.get(4)?,
        state: r.get(5)?,
        state_reason: r.get(6)?,
        action_json: r.get(7)?,
        claim_epoch: r.get(8)?,
        executor_json: r.get(9)?,
        claim_ref: r.get(10)?,
        claim_ref_ident: r.get(11)?,
        claim_reflog_len: r.get(12)?,
        claim_reflog_hash: r.get(13)?,
        result_observation_id: r.get(14)?,
    })
}
pub(crate) fn load_effect(conn: &Connection, id: &str) -> Result<Option<EffectRow>, Error> {
    Ok(conn
        .query_row(
            &format!("SELECT {EFFECT_COLUMNS} FROM effect WHERE id=?1"),
            [id],
            effect_row,
        )
        .optional()?)
}
pub(crate) fn effects_where(
    conn: &Connection,
    clause: &str,
    arg: &str,
) -> Result<Vec<EffectRow>, Error> {
    let mut q = conn.prepare(&format!(
        "SELECT {EFFECT_COLUMNS} FROM effect WHERE {clause} ORDER BY created_at,id"
    ))?;
    Ok(q.query_map([arg], effect_row)?.collect::<Result<_, _>>()?)
}

/// Exclusive fixture mutation lock, held by the executor from before the claim until
/// after its result commit. Kernel releases it on process death (O_CLOEXEC, no forks).
pub(crate) struct MutationLock(File);
impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
pub(crate) fn mutation_lock_path(root: &Path, repo_id: &str) -> std::path::PathBuf {
    root.join("locks").join(format!("fixture-{repo_id}.lock"))
}
/// Ok(Some) = acquired; Ok(None) = held by another open file description.
pub(crate) fn try_mutation_lock(root: &Path, repo_id: &str) -> Result<Option<MutationLock>, Error> {
    let dir = root.join("locks");
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => File::open(root)?.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(Error::Invalid("lock directory must not be a symlink"));
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(mutation_lock_path(root, repo_id))?;
    match f.try_lock_exclusive() {
        Ok(()) => Ok(Some(MutationLock(f))),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    ConfirmedApplied,
    ConfirmedNotApplied,
    Conflict,
    Unknown,
}
impl Classification {
    fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedApplied => "confirmed_applied",
            Self::ConfirmedNotApplied => "confirmed_not_applied",
            Self::Conflict => "conflict",
            Self::Unknown => "unknown",
        }
    }
    fn effect_state(self) -> &'static str {
        match self {
            Self::ConfirmedApplied => "confirmed",
            Self::ConfirmedNotApplied => "not_applied",
            Self::Conflict => "conflict",
            Self::Unknown => "unknown",
        }
    }
    fn next(self) -> (&'static str, &'static str) {
        match self {
            Self::ConfirmedApplied => (
                "finalizing",
                "action_applied_review_closure_summary_and_close",
            ),
            Self::ConfirmedNotApplied => ("blocked", "effect_not_applied_new_review_required"),
            Self::Conflict => (
                "blocked",
                "inspect_conflicting_target_no_automatic_rollback",
            ),
            Self::Unknown => ("blocked", "inspect_ambiguous_effect_no_retry"),
        }
    }
}
pub(crate) fn reflog_prefix_hash(raw: &[u8], entries: usize) -> String {
    let mut end = 0;
    for _ in 0..entries {
        match raw[end..].iter().position(|b| *b == b'\n') {
            Some(p) => end += p + 1,
            None => break,
        }
    }
    crate::artifact::sha256_hex(&raw[..end])
}

/// The executor's own durable CAS report (explicitly sourced, never independent evidence).
#[derive(Debug, Clone)]
pub(crate) struct ExecRecord {
    pub result: Value,
    pub recorded_at: i64,
}
pub(crate) fn load_exec(conn: &Connection, effect_id: &str) -> Result<Option<ExecRecord>, Error> {
    let r: Option<(String, i64)> = conn
        .query_row(
            "SELECT result_json,recorded_at FROM effect_executor_result WHERE effect_id=?1",
            [effect_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match r {
        Some((j, t)) => Some(ExecRecord {
            result: serde_json::from_str(&j)?,
            recorded_at: t,
        }),
        None => None,
    })
}

/// Classify an effect from INDEPENDENT readback of target state and audit history,
/// kept separate from the executor's own recorded CAS result (explicitly sourced).
///
/// Rules: a desired state alone never establishes attributable
/// execution. `confirmed_applied` needs attribution evidence: an audit entry for this
/// effect, or the executor's own durable CAS result. Otherwise the result is `unknown`
/// with `attribution: unattributed`. A lost executor result is never reconstructed.
/// `executor_live` must be established by the caller; a live executor is never
/// concluded not-applied.
pub(crate) fn classify(
    e: &EffectRow,
    action: &Action,
    executor_live: bool,
    exec: Option<&ExecRecord>,
) -> (Classification, Value) {
    use Classification::*;
    let (a, b) = (action.expected_old.as_str(), action.new_commit.as_str());
    let mut obs = json!({"expected_old": a, "new_commit": b, "executor_may_be_live": executor_live,
        "attribution": "none", "desired_state_observed": false, "audit_entry_missing": false, "warnings": []});
    obs["executor_result"] = match exec {
        Some(x) => {
            json!({"source": "executor_in_process", "cas": x.result, "recorded_at": x.recorded_at,
            "note": "the executor's own report; not independent evidence"})
        }
        None => json!({"source": "none", "status": "not_recorded",
            "note": "no durable executor result exists; a lost result is not reconstructed as fact"}),
    };
    obs["readback"] = json!({"source": "coordinator_readback"});
    if executor_live {
        obs["reason"] = json!("executor may still be in flight; no conclusion drawn");
        return (Unknown, obs);
    }
    let repo = action.repo();
    if let Err(err) = gitfixture::verify(&repo) {
        obs["reason"] = json!(format!("repository unavailable or changed: {err}"));
        return (Unknown, obs);
    }
    let current = match gitfixture::read_ref(&repo) {
        Ok(v) => v,
        Err(err) => {
            obs["reason"] = json!(err);
            return (Unknown, obs);
        }
    };
    obs["readback"]["observed_ref"] = json!(current);
    obs["desired_state_observed"] = json!(current == b);
    let (raw, log) = match gitfixture::read_log(&repo) {
        Ok(x) => x,
        Err(err) => {
            obs["reason"] = json!(format!("audit history unavailable: {err}"));
            return (Unknown, obs);
        }
    };
    let lock = gitfixture::lock_contents(&repo);
    obs["readback"]["ref_lock"] = json!(lock.clone().unwrap_or_else(Some));
    let claimed = e.claim_reflog_len.unwrap_or(-1);
    if claimed < 0
        || (log.len() as i64) < claimed
        || reflog_prefix_hash(&raw, claimed as usize)
            != e.claim_reflog_hash.clone().unwrap_or_default()
    {
        obs["reason"] = json!("audit history truncated or rewritten since the dispatch claim");
        return (Unknown, obs);
    }
    let new = &log[claimed as usize..];
    let msg = gitfixture::effect_message(&e.id);
    let ours: Vec<_> = new.iter().filter(|l| l.message == msg).collect();
    let others = new.len() - ours.len();
    obs["readback"]["entries_since_claim"] = json!(new);
    obs["readback"]["competing_entries_since_claim"] = json!(others);
    if ours.len() > 1 || ours.first().is_some_and(|l| l.old != a || l.new != b) {
        obs["reason"] = json!("audit entries for this effect are inconsistent");
        return (Unknown, obs);
    }
    let audited = ours.len() == 1;
    obs["readback"]["audited_by_this_effect"] = json!(audited);
    let cas = exec.and_then(|x| x.result["cas"].as_str()).unwrap_or("");
    let ex_applied = cas == "applied";
    // These executor outcomes report that THIS operation did not change the ref.
    let ex_no_change = matches!(cas, "mismatch" | "locked" | "aborted");
    let evidence = if audited {
        Some("audit_entry")
    } else if ex_applied {
        Some("executor_reported_cas")
    } else {
        None
    };
    if current == b {
        if ex_no_change {
            obs["attribution"] = json!("unattributed");
            obs["reason"] = json!(
                "executor reported that its operation did not change the ref, yet the target holds the proposed commit: not attributed to this effect"
            );
            return (Unknown, obs);
        }
        let Some(ev) = evidence else {
            obs["attribution"] = json!("unattributed");
            obs["reason"] = json!(format!(
                "desired state observed (target holds the proposed commit) but not attributed to this effect: no audit entry and no executor-reported CAS result{}; cause not established; no retry",
                if others > 0 {
                    format!("; {others} competing write(s) since the claim")
                } else {
                    String::new()
                }
            ));
            return (Unknown, obs);
        };
        obs["attribution"] = json!(ev);
        obs["audit_entry_missing"] = json!(!audited);
        if !audited {
            obs["warnings"] = json!([
                "audit entry for this effect is missing; attribution rests on the executor's own recorded CAS report, not independent evidence"
            ]);
        }
        if others > 0 {
            obs["readback"]["note"] = json!("later competing writes observed");
        }
        obs["summary"] = json!(format!("applied; attributed by {ev}"));
        return (ConfirmedApplied, obs);
    }
    if current == a {
        if ex_applied {
            obs["reason"] = json!(
                "executor reported an applied update but the target holds the old value: contradictory evidence; applied status not established"
            );
            return (Unknown, obs);
        }
        if audited {
            if others > 0 {
                obs["reason"] = json!(
                    "audit entry for this effect followed by competing writes that returned the ref to its old value; no automatic rollback or retry"
                );
                return (Conflict, obs);
            }
            obs["reason"] = json!(
                "audit entry for this effect exists but the ref still holds the old value: cannot distinguish a partially completed update from an unlogged revert or a forged entry; applied status not established"
            );
            return (Unknown, obs);
        }
        if others > 0 {
            obs["reason"] =
                json!("competing writes since the claim make the prior effect ambiguous");
            return (Unknown, obs);
        }
        let ident = gitfixture::ref_identity(&repo);
        obs["readback"]["ref_identity_unchanged_since_claim"] =
            json!(ident.as_ref().ok() == e.claim_ref_ident.as_ref());
        if ident.as_ref().ok() != e.claim_ref_ident.as_ref() {
            obs["reason"] = json!(
                "ref file was rewritten since the claim without an explaining audit entry; prior effect ambiguous"
            );
            return (Unknown, obs);
        }
        return match lock {
            Ok(None) => {
                obs["reason"] = json!("ref unchanged, no audit entry, no executor and no ref lock");
                (ConfirmedNotApplied, obs)
            }
            Ok(Some(l)) if l == b => {
                obs["reason"] = json!(
                    "interrupted executor left its own ref lock before rename; ref unchanged. Lock left in place for the owner; no automatic deletion"
                );
                (ConfirmedNotApplied, obs)
            }
            Ok(Some(_)) => {
                obs["reason"] = json!("another writer holds the ref lock");
                (Unknown, obs)
            }
            Err(err) => {
                obs["reason"] = json!(err);
                (Unknown, obs)
            }
        };
    }
    if let Some(ev) = evidence {
        obs["attribution"] = json!(ev);
    }
    obs["reason"] = json!(if evidence.is_some() {
        "this effect is recorded as applied, but the target later holds another value; no automatic rollback"
    } else {
        "target changed to a third value; this effect's contribution is unproven; no automatic rollback"
    });
    (Conflict, obs)
}

/// Structured uncertainty for an effect, from its latest recorded observation. An effect is
/// closure-eligible only when `confirmed` AND attributed by an audit entry or the
/// executor's own CAS report. Legacy or unattributed results are never eligible.
pub(crate) fn effect_flags(conn: &Connection, e: &EffectRow) -> Result<Value, Error> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM effect_observation WHERE effect_id=?1 ORDER BY observed_at DESC,rowid DESC LIMIT 1",
            [&e.id],
            |r| r.get(0),
        )
        .optional()?;
    let p: Value = match payload {
        Some(j) => serde_json::from_str(&j)?,
        None => Value::Null,
    };
    let attribution = p["attribution"].as_str().unwrap_or("none").to_string();
    let eligible = e.state == "confirmed"
        && matches!(
            attribution.as_str(),
            "audit_entry" | "executor_reported_cas"
        );
    Ok(json!({
        "attribution": attribution,
        "desired_state_observed": p["desired_state_observed"] == true,
        "audit_entry_missing": p["audit_entry_missing"] == true,
        "executor_result_recorded": load_exec(conn, &e.id)?.is_some(),
        "closure_eligible": eligible,
        "warnings": p["warnings"].clone(),
    }))
}

fn executor_live(root: &Path, e: &EffectRow) -> Result<(bool, Option<MutationLock>), Error> {
    let lock = try_mutation_lock(root, &e.repo_id)?;
    let ident_live = e
        .executor_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| serde_json::from_value::<Identity>(v["identity"].clone()).ok())
        .is_some_and(|i| i.pid != std::process::id() as i32 && i.live());
    Ok((lock.is_none() || ident_live, lock))
}

impl Owner {
    /// T7: record an independently obtained result. Writes state only on transitions.
    fn record_result(
        &mut self,
        e: &EffectRow,
        kind: &str,
        class: Classification,
        payload: Value,
    ) -> Result<bool, Error> {
        let mut g =
            crate::storage::Guard::new(crate::storage::Boundary::T7, &e.id).step("result_record");
        let r = self.record_result_in(e, kind, class, payload, &mut g);
        g.finish(r)
    }
    fn record_result_in(
        &mut self,
        e: &EffectRow,
        kind: &str,
        class: Classification,
        payload: Value,
        g: &mut crate::storage::Guard,
    ) -> Result<bool, Error> {
        let epoch = self.epoch;
        barrier("before_result_commit")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        let cur = load_effect(&tx, &e.id)?.ok_or(Error::Invalid("missing effect"))?;
        if !matches!(cur.state.as_str(), "dispatching" | "unknown") {
            return Ok(false);
        }
        let current_epoch: i64 =
            tx.query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))?;
        if current_epoch != epoch {
            return Err(Error::Rejected(
                "coordinator ownership lost; result not recorded".into(),
            ));
        }
        let new_state = class.effect_state();
        if cur.state == new_state {
            tx.commit()?;
            return Ok(false);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let body = payload.to_string();
        tx.execute(
            "INSERT INTO effect_observation VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                e.id,
                kind,
                class.as_str(),
                now,
                fingerprint(&body),
                body,
                epoch
            ],
        )?;
        let reason = payload["reason"]
            .as_str()
            .or(payload["summary"].as_str())
            .unwrap_or(class.as_str())
            .to_string();
        tx.execute(
            "UPDATE effect SET state=?1,state_reason=?2,result_observation_id=?3 WHERE id=?4",
            params![new_state, reason, id, e.id],
        )?;
        let (phase, mut action) = class.next();
        if class == Classification::Unknown
            && payload["attribution"] == "unattributed"
            && payload["desired_state_observed"] == true
        {
            action = "inspect_desired_state_unattributed_no_retry";
        }
        set_responsibility(&tx, &e.work_id, phase, "local-owner", action)?;
        event(
            &tx,
            &e.work_id,
            "effect.result",
            &e.id,
            json!({"classification": class.as_str(), "observation_id": id, "kind": kind}),
        )?;
        g.commit(tx)?;
        barrier("after_result_commit")?;
        Ok(true)
    }

    /// T6 + external action + T7. Dispatch happens at most once per approval.
    /// A failure is classified by the stage reached (`storage`): the CAS runs only after the
    /// T6 claim commit returned OK, so an unknown claim outcome never runs the action.
    pub fn dispatch(&mut self, effect_id: &str) -> Result<Value, Error> {
        let mut g = crate::storage::Guard::new(crate::storage::Boundary::T6, effect_id);
        let r = self.dispatch_in(effect_id, &mut g);
        g.finish(r)
    }
    fn dispatch_in(
        &mut self,
        effect_id: &str,
        g: &mut crate::storage::Guard,
    ) -> Result<Value, Error> {
        if !valid_id(effect_id) {
            return Err(Error::Invalid("invalid effect ID"));
        }
        // A restored ledger may have forgotten a dispatch made after its backup; restoration
        // authorizes no replay, so it never dispatches.
        crate::recovery::restore_fence(&self.conn, "dispatch")?;
        let e = load_effect(&self.conn, effect_id)?.ok_or(Error::Invalid("unknown effect"))?;
        if e.state == "cancelled" {
            return Err(Error::Rejected(format!(
                "approval cancelled before any claim and never dispatched: {}; a new review is required",
                e.state_reason
            )));
        }
        if e.state == "intent" {
            crate::attention::progress_fence(&self.conn, &e.work_id)?;
        }
        if e.state != "intent" {
            return Ok(
                json!({"effect_id": e.id, "dispatched": false, "state": e.state, "state_reason": e.state_reason,
                "reason": "approval already claimed or cancelled; a consumed approval is never re-dispatched", "work_success": false}),
            );
        }
        let Some(_lock) = try_mutation_lock(self.root(), &e.repo_id)? else {
            return Err(Error::Rejected(
                "fixture mutation lock held by another executor".into(),
            ));
        };
        let r = load_review(&self.conn, &e.review_id)?.ok_or(Error::Invalid("missing review"))?;
        let action: Action = serde_json::from_str(&e.action_json)?;
        action.validate()?;
        let retire = |o: &mut Owner, s: Stale| -> Result<Value, Error> {
            o.retire(&r.id, &s)?;
            Err(Error::Rejected(format!(
                "dispatch refused before claim: {}",
                s.reason
            )))
        };
        let (env, _) = match self.assess(&r) {
            Ok(x) => x,
            Err(s) => return retire(self, s),
        };
        if env.action != action
            || fingerprint(&e.action_json) != fingerprint(&serde_json::to_string(&env.action)?)
        {
            return retire(
                self,
                crate::review::Stale {
                    state: "invalidated",
                    reason: "effect action differs from reviewed envelope".into(),
                },
            );
        }
        let repo = action.repo();
        let claim_ref = gitfixture::read_ref(&repo).map_err(Error::Rejected)?;
        let claim_ident = gitfixture::ref_identity(&repo).map_err(Error::Rejected)?;
        let (raw, log) = gitfixture::read_log(&repo).map_err(Error::Rejected)?;
        let identity = Identity::current()?;
        let lock_path = mutation_lock_path(self.root(), &e.repo_id);
        let epoch = self.epoch;
        // Named point before the claim transaction opens (no SQLite lock held here).
        barrier("before_effect_claim")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let t = crate::store::authority_time(&tx)?;
        let now = t.stamp();
        let cur = load_effect(&tx, effect_id)?.ok_or(Error::Invalid("missing effect"))?;
        let rev = load_review(&tx, &r.id)?.ok_or(Error::Invalid("missing review"))?;
        let current_epoch: i64 =
            tx.query_row("SELECT epoch FROM coordinator_epoch", [], |x| x.get(0))?;
        if cur.state != "intent" || rev.state != "authorized" || current_epoch != epoch {
            return Err(Error::Rejected(
                "effect or review changed concurrently; not claimed".into(),
            ));
        }
        crate::recovery::restore_fence(&tx, "dispatch")?;
        // Serialized with revocation: a revocation committed before this point blocks.
        if let Err(s) = authority_current(&tx, &env, &t) {
            drop(tx);
            return retire(self, s);
        }
        if claim_ref != action.expected_old {
            drop(tx);
            return retire(
                self,
                crate::review::Stale {
                    state: "invalidated",
                    reason: "target ref changed before claim".into(),
                },
            );
        }
        let executor = json!({"identity": identity, "mutation_lock": lock_path, "epoch": epoch});
        tx.execute(
            "UPDATE review SET state='consumed',state_reason='dispatch claimed' WHERE id=?1",
            [&r.id],
        )?;
        tx.execute(
            "UPDATE effect SET state='dispatching',state_reason='claimed; result not yet observed',claim_epoch=?1,dispatched_at=?2,executor_json=?3,claim_ref=?4,claim_ref_ident=?5,claim_reflog_len=?6,claim_reflog_hash=?7 WHERE id=?8",
            params![epoch, now, executor.to_string(), claim_ref, claim_ident, log.len() as i64, reflog_prefix_hash(&raw, log.len()), effect_id],
        )?;
        set_responsibility(
            &tx,
            &e.work_id,
            "applying",
            "coordinator",
            "observe_effect_result_or_reconcile",
        )?;
        event(
            &tx,
            &e.work_id,
            "effect.claimed",
            effect_id,
            json!({"review_id": r.id, "epoch": epoch, "claim_ref": claim_ref,
            "limit": "no atomic boundary between this claim and the external ref update"}),
        )?;
        barrier("before_claim_commit")?;
        g.commit(tx)?;
        barrier("after_effect_claim")?;
        let claimed =
            load_effect(&self.conn, effect_id)?.ok_or(Error::Invalid("missing effect"))?;

        // Post-claim checks may abort, but cannot guarantee prevention in general.
        let revoked_after: Option<i64> = self.conn.query_row(
            "SELECT p.revoked_at FROM permit p WHERE p.id=?1",
            [&env.permit.id],
            |x| x.get(0),
        )?;
        let candidate_ok = self.candidate_still_bound(&env, &repo);
        barrier("before_ref_cas")?;
        let cas: Result<Cas, Error> = match candidate_ok {
            Err(why) => Ok(Cas::Aborted { reason: why.into() }),
            Ok(()) => {
                // Final recheck runs inside the ref lock, immediately before the update:
                // permit expiry/revocation/scope, review expiry and evidence freshness.
                // It is NOT atomic with the rename; see docs/OPERATIONS.md.
                let conn = &self.conn;
                let mut recheck = || -> Result<(), String> {
                    let now = crate::store::authority_time(conn)
                        .map_err(|e| format!("clock unavailable: {e}"))?;
                    authority_current(conn, &env, &now).map_err(|s| s.reason)
                };
                gitfixture::compare_and_swap(
                    &repo,
                    &action.expected_old,
                    &action.new_commit,
                    effect_id,
                    &mut recheck,
                )
            }
        };
        journal(&format!(
            "cas_end {effect_id} {}",
            cas.as_ref()
                .map(|x| format!("{x:?}"))
                .unwrap_or_else(|e| e.to_string())
        ));
        barrier("after_ref_cas")?;
        // Retain the executor's own result, separately sourced, BEFORE any readback.
        let exec_json = match &cas {
            Ok(c) => serde_json::to_value(c)?,
            Err(err) => json!({"cas": "error", "error": err.to_string()}),
        };
        {
            // T7 (first part): the executor's own report, committed before any readback.
            let mut g7 = crate::storage::Guard::new(crate::storage::Boundary::T7, effect_id)
                .step("executor_result");
            let r = (|| -> Result<(), Error> {
                let tx = self
                    .conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT INTO effect_executor_result VALUES(?1,'executor_in_process',?2,?3,?4,?5)",
                    params![
                        effect_id,
                        executor.to_string(),
                        exec_json.to_string(),
                        now_ms()?,
                        epoch
                    ],
                )?;
                g7.commit(tx)
            })();
            g7.finish(r)?;
        }
        barrier("after_executor_result_commit")?;
        // Independent readback; never success from the CAS response alone.
        let exec = load_exec(&self.conn, effect_id)?;
        let (class, mut obs) = classify(&claimed, &action, false, exec.as_ref());
        let revoked_now: Option<i64> = self.conn.query_row(
            "SELECT revoked_at FROM permit WHERE id=?1",
            [&env.permit.id],
            |x| x.get(0),
        )?;
        obs["revoked_after_claim"] = json!(revoked_now.is_some());
        obs["revocation_seen_before_ref_update"] = json!(revoked_after.is_some());
        if revoked_now.is_some() {
            obs["revocation_limit"] = json!("revocation after claim cannot guarantee prevention");
        }
        self.record_result(&claimed, "dispatch_readback", class, obs.clone())?;
        let after = load_effect(&self.conn, effect_id)?.ok_or(Error::Invalid("missing effect"))?;
        Ok(
            json!({"effect_id": effect_id, "dispatched": true, "classification": class.as_str(), "state": after.state,
            "state_reason": after.state_reason, "observation": obs, "meaning": RESULT_NOT_COMPLETION,
            "work_success": false, "work_closed": false}),
        )
    }
    fn candidate_still_bound(&mut self, env: &Envelope, repo: &Repo) -> Result<(), &'static str> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM artifact WHERE id=?1",
                    crate::validation::ARTIFACT_COLUMNS
                ),
                [&env.candidate_artifact.id],
                crate::validation::artifact_row,
            )
            .map_err(|_| "candidate artifact unreadable after claim; ref update not attempted")?;
        let scan = self
            .verified(&row)
            .map_err(|_| "candidate artifact changed after claim; ref update not attempted")?;
        let (tree, blobs, commit) = candidate_objects(&scan, &env.action);
        if commit.id != env.action.new_commit
            || !blobs
                .iter()
                .chain([&tree, &commit])
                .all(|o| gitfixture::object_present(repo, o))
        {
            return Err("proposed commit objects changed after claim; ref update not attempted");
        }
        Ok(())
    }

    /// T10 for effects: classify every dispatching/unknown effect with no live executor.
    /// `reprobe` records a fresh observation even when the classification is unchanged.
    pub fn reconcile_effects(&mut self) -> Result<Vec<Value>, Error> {
        let rows = effects_where(
            &self.conn,
            "state IN ('dispatching','unknown') AND ?1=?1",
            "x",
        )?;
        let mut out = Vec::new();
        for e in rows {
            let action: Action = serde_json::from_str(&e.action_json)?;
            let (live, _guard) = executor_live(self.root(), &e)?;
            let exec = load_exec(&self.conn, &e.id)?;
            let (class, obs) = classify(&e, &action, live, exec.as_ref());
            if live {
                out.push(json!({"effect_id": e.id, "state": e.state, "executor_may_be_live": true,
                    "reason": "executor may still be in flight; reconciliation deferred, no new dispatch"}));
                continue;
            }
            let changed = self.record_result(&e, "reconciliation", class, obs.clone())?;
            let after = load_effect(&self.conn, &e.id)?.ok_or(Error::Invalid("missing effect"))?;
            out.push(
                json!({"effect_id": e.id, "classification": class.as_str(), "state": after.state,
                "transitioned": changed, "observation": obs, "meaning": RESULT_NOT_COMPLETION}),
            );
        }
        Ok(out)
    }

    /// Every effect with its state and latest independent classification.
    pub fn effect_summaries(&mut self) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        for e in effects_where(&self.conn, "?1=?1", "x")? {
            let latest: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT classification,kind FROM effect_observation WHERE effect_id=?1 ORDER BY observed_at DESC,rowid DESC LIMIT 1",
                    [&e.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let mut row = json!({"effect_id": e.id, "work_id": e.work_id, "state": e.state, "state_reason": e.state_reason,
                "classification": latest.as_ref().map(|l| l.0.clone()), "observed_by": latest.map(|l| l.1),
                "meaning": RESULT_NOT_COMPLETION});
            if let (Some(r), Some(f)) = (
                row.as_object_mut(),
                effect_flags(&self.conn, &e)?.as_object(),
            ) {
                r.extend(f.clone());
            }
            out.push(row);
        }
        Ok(out)
    }
    /// Explicit owner-visible Task 4 state: pending reviews, effects and responsibility.
    pub fn task4_status(&mut self, attempt: &str) -> Result<Value, Error> {
        reconcile_legacy_quarantine(&mut self.conn)?;
        self.sweep_reviews()?;
        let c = crate::validation::ctx(&self.conn, attempt)?;
        let now = now_ms()?;
        let mut reviews = Vec::new();
        {
            let mut q = self.conn.prepare(&format!(
                "SELECT {} FROM review WHERE work_id=?1 ORDER BY created_at,id",
                crate::review::REVIEW_COLUMNS
            ))?;
            let rows: Vec<_> = q
                .query_map([&c.work], crate::review::review_row)?
                .collect::<Result<_, _>>()?;
            for r in rows {
                let action: Value =
                    serde_json::from_str::<Value>(&r.envelope_json)?["action"].clone();
                reviews.push(json!({"review_id": r.id, "state": r.state, "state_reason": r.state_reason,
                    "envelope_hash": r.envelope_hash, "work_version": r.work_version, "expires_at": r.expires_at,
                    "expired": now >= r.expires_at, "action": action}));
            }
        }
        let mut effects = Vec::new();
        for e in effects_where(&self.conn, "work_id=?1", &c.work)? {
            let obs: Option<Value> = match &e.result_observation_id {
                Some(id) => Some(serde_json::from_str(&self.conn.query_row(
                    "SELECT payload_json FROM effect_observation WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )?)?),
                None => None,
            };
            let mut row = json!({"effect_id": e.id, "review_id": e.review_id, "decision_id": e.decision_id,
                "state": e.state, "state_reason": e.state_reason, "result": obs});
            if let (Some(r), Some(f)) = (
                row.as_object_mut(),
                effect_flags(&self.conn, &e)?.as_object(),
            ) {
                r.extend(f.clone());
            }
            effects.push(row);
        }
        let (mode, phase, owner, action, due, version): (String, String, String, String, i64, i64) =
            self.conn.query_row(
                "SELECT mode,phase,next_owner,next_action,due_at,version FROM work WHERE id=?1",
                [&c.work],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?;
        let outcome: Option<String> = self
            .conn
            .query_row(
                "SELECT outcome FROM work_outcome WHERE work_id=?1",
                [&c.work],
                |r| r.get(0),
            )
            .optional()?;
        let pending = reviews
            .iter()
            .find(|r| r["state"] == "pending")
            .map(|r| r["review_id"].clone());
        Ok(json!({
            "attempt_id": attempt, "work_id": c.work, "pending_review": pending, "reviews": reviews, "effects": effects,
            "work": {"mode": mode, "phase": phase, "next_owner": owner, "next_action": action, "due_at": due,
                     "overdue": mode == "active" && due < now, "version": version},
            "action_result_is_work_completion": false, "meaning": RESULT_NOT_COMPLETION,
            "work_closed": mode == "closed", "outcome": outcome,
            "work_success": outcome.as_deref() == Some("succeeded"),
            "attention": "see `inbox`; delivery happens only during an explicit `sweep`",
        }))
    }
}

/// Owner revocation without the coordinator lock, so it can race (and be serialized by
/// SQLite against) a dispatch claim. It only tightens authority. If an effect for this
/// permit is already claimed, the event records that prevention can no longer be guaranteed.
pub fn revoke_permit(root: &Path, id: &str) -> Result<Value, Error> {
    let mut c = crate::execution::open_ledger(root, crate::execution::Access::Act("revoke"))?;
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms()?;
    if tx.execute(
        "UPDATE permit SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
        params![now, id],
    )? != 1
    {
        return Err(Error::Permit);
    }
    let mut q = tx.prepare(
        "SELECT e.id FROM effect e JOIN work w ON w.id=e.work_id WHERE w.permit_id=?1 AND e.state IN ('dispatching','unknown') ORDER BY e.id",
    )?;
    let late: Vec<String> = q.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    drop(q);
    let payload = json!({"claimed_effects": late, "too_late_to_guarantee_prevention": !late.is_empty(),
        "note": if late.is_empty() { "revoked before any dispatch claim; unclaimed approvals under this permit cannot dispatch" }
                else { "revocation after dispatch claim cannot guarantee prevention; effect is reconciled, never retried" }});
    tx.execute(
        "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(NULL,'local-owner','permit.revoked',?1,?2,?3)",
        params![id, now, payload.to_string()],
    )?;
    tx.commit()?;
    Ok(payload)
}

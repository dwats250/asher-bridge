//! Gate 5: supported terminal outcomes. A closure is an explicit authenticated owner command
//! against the evidence summary computed here; nothing closes work automatically. Exit status,
//! an agent claim, approval alone, or backend acceptance are never sufficient.
//!
//! The summary has a DB-derived part (recomputed inside the closing transaction and required
//! to be unchanged) and a filesystem-derived part (artifact verification, writer quiescence and
//! a fresh ref readback), checked immediately before the transaction. `unknown` is never a
//! success, and cancellation/abandonment never undo an effect, drop an unknown attempt, free
//! the live-attempt slot or move a deadline: they close only when nothing live remains.
use crate::{
    Error, Owner,
    effect::{EffectRow, effect_flags, effects_where},
    evidence, gitfixture,
    protocol::fingerprint,
    review::{Action, Envelope, REVIEW_COLUMNS, ReviewRow, review_row},
    validation::{artifact_for, quiescence},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub const OUTCOMES: [&str; 5] = ["succeeded", "failed", "rejected", "cancelled", "abandoned"];

pub(crate) struct Summary {
    pub eligible: bool,
    pub blockers: Vec<String>,
    pub db_evidence: Value,
    pub evidence: Value,
    pub digest: String,
}
impl Summary {
    pub fn json(&self, outcome: &str, work: &str) -> Value {
        json!({
            "work_id": work, "outcome": outcome, "eligible": self.eligible, "blockers": self.blockers,
            "evidence": self.evidence, "digest": self.digest,
            "meaning": "closure needs an authenticated owner `close`/`abandon` command; this summary is read-only and closes nothing",
            "work_success": false, "work_closed": false,
        })
    }
}

struct Facts {
    id: String,
    mode: String,
    phase: String,
    owner: String,
    action: String,
    version: i64,
    attempt: Option<(String, String, bool)>,
    review: Option<ReviewRow>,
    open_reviews: i64,
    decision: Option<(String, String)>,
    effect: Option<EffectRow>,
    flags: Value,
    observation: Option<(String, String)>,
    cancel: Option<(String, String)>,
}

fn gather(conn: &Connection, work: &str) -> Result<Facts, Error> {
    let (mode, phase, owner, action, version): (String, String, String, String, i64) = conn
        .query_row(
            "SELECT mode,phase,next_owner,next_action,version FROM work WHERE id=?1",
            [work],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .ok_or(Error::Invalid("unknown work"))?;
    let attempt: Option<(String, String)> = conn
        .query_row(
            "SELECT id,state FROM attempt WHERE work_id=?1 ORDER BY created_at DESC,id LIMIT 1",
            [work],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let attempt = match attempt {
        Some((id, state)) => {
            let receipt: bool = conn.query_row(
                "SELECT count(*)>0 FROM supervisor_command WHERE attempt_id=?1",
                [&id],
                |r| r.get(0),
            )?;
            Some((id, state, receipt))
        }
        None => None,
    };
    let review: Option<ReviewRow> = conn
        .query_row(
            &format!("SELECT {REVIEW_COLUMNS} FROM review WHERE work_id=?1 ORDER BY created_at DESC,id DESC LIMIT 1"),
            [work],
            review_row,
        )
        .optional()?;
    let open_reviews: i64 = conn.query_row(
        "SELECT count(*) FROM review WHERE work_id=?1 AND state IN ('pending','authorized')",
        [work],
        |r| r.get(0),
    )?;
    let decision = match &review {
        Some(r) => conn
            .query_row(
                "SELECT id,choice FROM decision WHERE review_id=?1",
                [&r.id],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?,
        None => None,
    };
    let effect = effects_where(conn, "work_id=?1", work)?.into_iter().last();
    let (flags, observation) = match &effect {
        Some(e) => {
            let obs: Option<(String, String)> = conn
                .query_row(
                    "SELECT id,classification FROM effect_observation WHERE effect_id=?1 ORDER BY observed_at DESC,rowid DESC LIMIT 1",
                    [&e.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            (effect_flags(conn, e)?, obs)
        }
        None => (Value::Null, None),
    };
    let cancel = conn
        .query_row(
            "SELECT command_id,reason FROM cancel_request WHERE work_id=?1",
            [work],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Facts {
        id: work.into(),
        mode,
        phase,
        owner,
        action,
        version,
        attempt,
        review,
        open_reviews,
        decision,
        effect,
        flags,
        observation,
        cancel,
    })
}

/// Unresolved activity that must keep the work open for every outcome.
fn hazards(f: &Facts, conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((id, state, receipt)) = &f.attempt {
        match state.as_str() {
            "cleaned" => {
                if let Err(r) = quiescence(conn, id) {
                    out.push(format!("writer quiescence not established: {r}"));
                }
            }
            "intent" if !receipt => {}
            s => out.push(format!(
                "attempt {s}: unresolved activity retains the live-attempt slot"
            )),
        }
    }
    if let Some(e) = &f.effect {
        match e.state.as_str() {
            "intent" => out.push("approved action was neither dispatched nor cancelled".into()),
            "dispatching" => out.push(
                "effect dispatching: result not established (unknown); an executor may be in flight"
                    .into(),
            ),
            "unknown" => out.push(
                "effect unknown: possible consequential effect unresolved; cause not established"
                    .into(),
            ),
            "conflict" if !attributed(&f.flags) => out.push(
                "effect conflict with unproven contribution: the target changed and this effect's part is not attributed; its reconciliation responsibility stays open and no outcome can close it".into(),
            ),
            _ => {}
        }
    }
    if f.open_reviews > 0 {
        out.push("an open review exists: decide it, let it expire, or cancel first".into());
    }
    out
}

/// DB-derived part only (also recomputed inside the closing transaction).
fn db_part(
    conn: &Connection,
    work: &str,
    outcome: &str,
) -> Result<(Facts, Vec<String>, Value), Error> {
    let f = gather(conn, work)?;
    let mut blockers = Vec::new();
    let mut residual: Vec<String> = Vec::new();
    if f.mode != "active" {
        blockers.push("work is already closed".into());
    }
    blockers.extend(hazards(&f, conn));
    let attempt_cleaned = f.attempt.as_ref().is_some_and(|a| a.1 == "cleaned");
    let eff_state = f.effect.as_ref().map(|e| e.state.as_str());
    let eligible_effect = f.flags["closure_eligible"] == true;
    match outcome {
        "succeeded" => {
            if f.cancel.is_some() {
                blockers.push("cancellation was requested: an ordinary success is not available after a cancellation request (never withdrawn implicitly); close as cancelled, failed or abandoned".into());
            }
            if !attempt_cleaned {
                blockers
                    .push("no cleaned preparation attempt: cleanup scope not established".into());
            }
            match &f.review {
                Some(r) if r.state == "consumed" => {}
                _ => blockers.push(
                    "no consumed review: the exact action was not authorized and claimed".into(),
                ),
            }
            if !f.decision.as_ref().is_some_and(|d| d.1 == "approve") {
                blockers.push("no authenticated approving decision on the exact review".into());
            }
            match (&f.effect, eff_state) {
                (Some(_), Some("confirmed")) => {
                    if !eligible_effect {
                        blockers.push("confirmed effect lacks attribution evidence (legacy-quarantined or unattributed): cannot be a success".into());
                    }
                    if f.observation.as_ref().map(|o| o.1.as_str()) != Some("confirmed_applied") {
                        blockers
                            .push("effect result is not a confirmed applied observation".into());
                    }
                }
                (Some(_), Some(s)) => {
                    blockers.push(format!("effect is {s}, not a confirmed attributed result"))
                }
                _ => blockers
                    .push("no confirmed fixture effect, required by the success criteria".into()),
            }
        }
        "failed" => {
            if !attempt_cleaned {
                blockers.push(
                    "no cleaned preparation attempt: failure and cleanup state not established"
                        .into(),
                );
            }
            if eff_state == Some("confirmed") {
                blockers
                    .push("an applied effect exists; closing as failed would conceal it".into());
            }
            if eff_state == Some("conflict") && attributed(&f.flags) {
                blockers.push("an applied effect exists (later overwritten by another writer); closing as failed would conceal it".into());
            }
            // Failure evidence must itself meet the evidence-trust contract: the latest
            // candidate validation must be trusted (validator authority, intact, conflict-free,
            // artifact still valid) and fresh. A cancellation does not excuse this.
            let mut validation_failed = false;
            if let Some((a, _, _)) = &f.attempt {
                let rows = evidence::rows(conn, a, crate::store::freshness_clock(conn)?)?;
                if let Some(v) = rows
                    .iter()
                    .filter(|r| {
                        r.kind == "candidate_validation" && r.authority == "trusted_validator"
                    })
                    .max_by_key(|r| r.source_seq)
                {
                    if v.payload["validation_passed"] == false {
                        if !(v.trusted && v.intact && v.conflicts == 0) {
                            blockers.push(format!("failure evidence {} is not trusted (its artifact is invalid, or it was altered or conflicted)", v.id));
                        } else if !v.fresh {
                            blockers.push(format!("failure evidence {} is not fresh: stale trusted evidence is not excused", v.id));
                        } else {
                            validation_failed = true;
                        }
                    }
                }
            }
            let effect_failed = eff_state == Some("not_applied");
            if !validation_failed
                && !effect_failed
                && !blockers.iter().any(|b| b.starts_with("failure evidence"))
            {
                blockers.push("no evidence of failure: neither a trusted, fresh failing validation nor a not-applied effect (an unproven or overwritten effect is not evidence of failure)".into());
            }
        }
        "rejected" => {
            if !attempt_cleaned {
                blockers.push("no cleaned preparation attempt to account for".into());
            }
            let ok = f.review.as_ref().is_some_and(|r| r.state == "rejected")
                && f.decision.as_ref().is_some_and(|d| d.1 == "reject");
            if !ok {
                blockers.push("no authenticated rejection tied to the exact review".into());
            }
            if f.effect.is_some() {
                blockers.push(
                    "an effect exists for this work; rejection does not reverse an effect".into(),
                );
            }
        }
        "cancelled" => {
            if f.cancel.is_none() {
                blockers.push("cancellation not requested by the owner".into());
            }
            if eff_state == Some("confirmed") {
                residual.push(format!(
                    "applied effect {} is not undone by cancellation",
                    f.effect.as_ref().map(|e| e.id.clone()).unwrap_or_default()
                ));
            }
        }
        "abandoned" => {
            if matches!(eff_state, Some("confirmed")) {
                residual
                    .push("applied effect remains in place; abandonment does not undo it".into());
            }
        }
        _ => blockers.push("unsupported outcome".into()),
    }
    // Legacy/unattributed confirmed effects remain a disclosed residual risk for non-success.
    if outcome != "succeeded" && eff_state == Some("confirmed") && !eligible_effect {
        residual.push("a confirmed effect lacks attribution evidence".into());
    }
    // A conflict whose application is ATTRIBUTED (audit entry or executor-reported CAS) is a
    // known applied effect that another writer later overwrote: closable only by an outcome
    // that discloses it. An UNATTRIBUTED conflict is a hazard above and never reaches here.
    if outcome != "succeeded" && eff_state == Some("conflict") && attributed(&f.flags) {
        residual.push(format!(
            "effect {} was applied (attributed by {}) and the target was later overwritten by another writer; the target is not restored",
            f.effect.as_ref().map(|e| e.id.clone()).unwrap_or_default(),
            f.flags["attribution"].as_str().unwrap_or("?")
        ));
    }
    // Open evidence obligations (invalid artifacts, conflicted evidence). Outcomes that rely on
    // evidence (succeeded, failed) cannot close over them; the others carry each one forward as
    // a post-closure discrepancy responsibility in the closing transaction, never erasing it.
    let obligations = evidence_obligations(conn, work)?;
    let mut carried: Vec<Value> = Vec::new();
    // An attributed applied-then-overwritten effect is a consequential fact that closure does
    // not resolve: the closing transaction carries it forward as its own blocking discrepancy.
    if let (Some(e), Some("conflict")) = (&f.effect, eff_state) {
        if attributed(&f.flags) && matches!(outcome, "cancelled" | "abandoned") {
            let (fact, detail) = overwritten_fact(conn, work, e, &f.flags)?;
            carried.push(json!({"fact": fact, "detail": detail}));
        }
    }
    for o in &obligations {
        if matches!(outcome, "succeeded" | "failed") {
            blockers.push(format!("unresolved evidence obligation: {}", o.1));
        } else {
            carried.push(json!({"fact": o.0, "detail": o.1}));
        }
    }
    // An un-launched intent (no supervisor receipt) is retired by the closing transaction and
    // can never launch afterwards. Disclosed here so the owner reviews it before closing.
    let retire: Vec<String> = match &f.attempt {
        Some((id, state, false)) if state == "intent" && f.mode == "active" => vec![id.clone()],
        _ => vec![],
    };
    let gaps: Vec<String> = match &f.attempt {
        Some((a, _, _)) => {
            let g: Option<String> = conn
                .query_row(
                    "SELECT gaps_json FROM evidence WHERE attempt_id=?1 AND kind='capture_coverage' AND authority='trusted_validator' ORDER BY source_seq DESC LIMIT 1",
                    [a],
                    |r| r.get(0),
                )
                .optional()?;
            let mut v: Vec<String> = g
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            v.insert(
                0,
                "capture coverage remains incomplete (capture_restored=false)".into(),
            );
            v
        }
        None => vec![],
    };
    let evid: Vec<Value> = match (&f.review, &f.attempt) {
        (Some(r), Some((a, _, _))) => {
            let env = Envelope::parse(&r.envelope_json, &r.envelope_hash).ok();
            let rows = evidence::rows(conn, a, crate::store::freshness_clock(conn)?)?;
            env.map(|e| {
                e.evidence
                    .iter()
                    .map(|er| {
                        let row = rows.iter().find(|x| x.id == er.id);
                        json!({"id": er.id, "kind": er.kind, "envelope_hash": er.envelope_hash,
                            "trusted": row.is_some_and(|x| x.trusted && x.intact && x.conflicts == 0),
                            "fresh_until_at_review": er.fresh_until})
                    })
                    .collect()
            })
            .unwrap_or_default()
        }
        _ => vec![],
    };
    let db = json!({
        "outcome": outcome,
        "work": {"id": f.id, "version": f.version, "mode": f.mode, "phase": f.phase,
                 "next_owner": f.owner, "next_action": f.action},
        "attempt": f.attempt.as_ref().map(|a| json!({"id": a.0, "state": a.1})),
        "review": f.review.as_ref().map(|r| json!({"id": r.id, "state": r.state, "envelope_hash": r.envelope_hash})),
        "decision": f.decision.as_ref().map(|d| json!({"id": d.0, "choice": d.1})),
        "effect": f.effect.as_ref().map(|e| json!({"id": e.id, "state": e.state,
            "observation_id": f.observation.as_ref().map(|o| o.0.clone()),
            "attribution": f.flags["attribution"], "closure_eligible": f.flags["closure_eligible"],
            "dispatched": e.claim_epoch.is_some()})),
        "evidence": evid,
        "cancel_request": f.cancel.as_ref().map(|c| json!({"command_id": c.0, "reason": c.1})),
        "known_gaps": gaps,
        "residual_risks": residual,
        "carried_forward_discrepancies": carried,
        "pending_launch_intents_retired_by_closure": retire,
    });
    Ok((f, blockers, db))
}

impl Owner {
    pub(crate) fn work_for_key(&self, key: &str) -> Result<String, Error> {
        self.conn
            .query_row(
                "SELECT id FROM work WHERE principal='local-owner' AND request_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::Invalid("unknown work request key"))
    }
    /// Full read-only closure summary for one outcome (does not close anything).
    pub(crate) fn summary_for(&mut self, work: &str, outcome: &str) -> Result<Summary, Error> {
        if !OUTCOMES.contains(&outcome) {
            return Err(Error::Invalid("unsupported outcome"));
        }
        let (f, mut blockers, db) = db_part(&self.conn, work, outcome)?;
        let mut fs_part = json!({});
        if f.mode == "active" {
            // Filesystem-derived checks: artifact identity, then a fresh readback of the ref.
            if let Some((a, _, _)) = &f.attempt {
                let mut arts = Vec::new();
                for role in ["initial", "candidate"] {
                    if let Some(row) = artifact_for(&self.conn, a, role)? {
                        let ok = self.verified(&row).is_ok();
                        if !ok && matches!(outcome, "succeeded" | "failed") {
                            blockers.push(format!("{role} artifact {} cannot be verified (missing, changed or invalid)", row.id));
                        }
                        arts.push(json!({"role": role, "id": row.id, "manifest_hash": row.manifest_hash, "verified": ok}));
                    } else if outcome == "succeeded" {
                        blockers.push(format!("no {role} artifact"));
                    }
                }
                fs_part["artifacts"] = json!(arts);
            }
            if outcome == "succeeded" {
                if let Some(e) = &f.effect {
                    let action: Action = serde_json::from_str(&e.action_json)?;
                    let repo = action.repo();
                    match gitfixture::verify(&repo).and_then(|_| gitfixture::read_ref(&repo)) {
                        Ok(cur) => {
                            if cur != action.new_commit {
                                blockers.push(format!("readback inconsistent with the recorded result: target now holds {cur}, not the applied commit"));
                            }
                            fs_part["readback"] =
                                json!({"observed_ref": cur, "expected_ref": action.new_commit});
                        }
                        Err(err) => {
                            blockers.push(format!("readback unavailable: {err}"));
                            fs_part["readback"] = json!({"error": err});
                        }
                    }
                }
                // Required evidence: trusted, intact and fresh AS OF the claimed dispatch.
                if let (Some(r), Some(e)) = (&f.review, &f.effect) {
                    if let Ok(env) = Envelope::parse(&r.envelope_json, &r.envelope_hash) {
                        let at: Option<i64> = self.conn.query_row(
                            "SELECT dispatched_at FROM effect WHERE id=?1",
                            [&e.id],
                            |x| x.get(0),
                        )?;
                        let rows = evidence::rows(
                            &self.conn,
                            &env.attempt_id,
                            crate::store::freshness_clock(&self.conn)?,
                        )?;
                        for er in &env.evidence {
                            match rows.iter().find(|x| x.id == er.id) {
                                Some(x) if x.trusted && x.intact && x.conflicts == 0 => {
                                    if at.is_some_and(|d| x.fresh_until < d) {
                                        blockers.push(format!("required evidence {} was stale when the action was dispatched", er.id));
                                    }
                                }
                                _ => blockers.push(format!("required trusted evidence {} is missing, altered or conflicted", er.id)),
                            }
                        }
                    } else {
                        blockers.push("review envelope unreadable".into());
                    }
                }
            }
        }
        let evidence = json!({"db": db, "fs": fs_part});
        let digest = fingerprint(&json!({"outcome": outcome, "evidence": evidence}).to_string());
        let mut flat = db.clone();
        flat["fs"] = fs_part.clone();
        flat["readback"] = fs_part["readback"].clone();
        Ok(Summary {
            eligible: blockers.is_empty(),
            blockers,
            db_evidence: db,
            evidence: flat,
            digest,
        })
    }

    /// CLI: read-only closure summary by request key.
    pub fn closure_summary(&mut self, request_key: &str, outcome: &str) -> Result<Value, Error> {
        let work = self.work_for_key(request_key)?;
        let s = self.summary_for(&work, outcome)?;
        Ok(s.json(outcome, &work))
    }
}

/// DB-only recomputation used inside the closing transaction.
pub(crate) fn db_evidence_now(
    conn: &Connection,
    work: &str,
    outcome: &str,
) -> Result<(Vec<String>, Value), Error> {
    let (_f, b, db) = db_part(conn, work, outcome)?;
    Ok((b, db))
}

fn attributed(flags: &Value) -> bool {
    matches!(
        flags["attribution"].as_str(),
        Some("audit_entry" | "executor_reported_cas")
    )
}

/// (fact key, description) for every open evidence obligation of a work.
fn evidence_obligations(conn: &Connection, work: &str) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    let mut q = conn.prepare(
        "SELECT id,role,COALESCE(invalid_reason,'') FROM artifact WHERE work_id=?1 AND state='invalid' ORDER BY sealed_at,id",
    )?;
    for r in q.query_map([work], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, role, why) = r?;
        out.push((
            format!("artifact:{id}"),
            format!("{role} artifact {id} is invalid: {why}"),
        ));
    }
    let mut q = conn.prepare(
        "SELECT DISTINCT e.id,e.kind FROM evidence e JOIN event c ON c.kind='evidence.conflict' AND c.entity_id=e.id WHERE e.work_id=?1 ORDER BY e.id",
    )?;
    for r in q.query_map([work], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (id, kind) = r?;
        out.push((
            format!("evidence:{id}"),
            format!("{kind} evidence {id} has a recorded content conflict"),
        ));
    }
    Ok(out)
}

/// Every adverse fact currently observable for a CLOSED work: (fact key, description).
/// Derived only from durable rows plus, for a succeeded outcome, a fresh readback of the
/// fixture ref, so it can be re-derived at every open without a separate recovery framework.
fn adverse_facts(
    conn: &Connection,
    work: &str,
    outcome: &str,
) -> Result<(Vec<(String, String)>, Value), Error> {
    let mut facts = evidence_obligations(conn, work)?;
    let mut observed = json!({});
    let mut q = conn.prepare(
        "SELECT a.id,a.state,EXISTS(SELECT 1 FROM supervisor_command c WHERE c.attempt_id=a.id) FROM attempt a WHERE a.work_id=?1 ORDER BY a.created_at,a.id",
    )?;
    for r in q.query_map([work], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, bool>(2)?,
        ))
    })? {
        let (id, state, receipt) = r?;
        observed[format!("attempt:{id}")] = json!(state);
        if state != "cleaned" && !(state == "intent" && !receipt) {
            facts.push((
                format!("attempt:{id}:{state}"),
                format!("attempt {id} is {state} after closure: unresolved supervised activity"),
            ));
        }
    }
    for e in effects_where(conn, "work_id=?1", work)? {
        let flags = effect_flags(conn, &e)?;
        observed[format!("effect:{}", e.id)] = json!(e.state);
        match e.state.as_str() {
            "dispatching" | "unknown" => facts.push((
                format!("effect:{}:{}", e.id, e.state),
                format!(
                    "effect {} is {} after closure: possible consequential effect unresolved",
                    e.id, e.state
                ),
            )),
            "conflict" if !attributed(&flags) => facts.push((
                format!("effect:{}:conflict_unproven", e.id),
                format!("effect {} is in conflict with unproven contribution", e.id),
            )),
            "conflict" => facts.push(overwritten_fact(conn, work, &e, &flags)?),
            "confirmed" if outcome == "succeeded" => {
                let action: Action = serde_json::from_str(&e.action_json)?;
                let repo = action.repo();
                match gitfixture::verify(&repo).and_then(|_| gitfixture::read_ref(&repo)) {
                    Ok(cur) => {
                        observed[format!("ref:{}", e.id)] = json!(cur);
                        if cur != action.new_commit {
                            facts.push((
                                format!("ref:{}:{cur}", e.id),
                                format!("the succeeded outcome's effect {} applied {} but a fresh readback of the fixture ref shows {cur}", e.id, action.new_commit),
                            ));
                        }
                    }
                    Err(err) => {
                        observed[format!("ref:{}", e.id)] = json!({"error": err});
                        facts.push((
                            format!("ref:{}:unreadable", e.id),
                            format!(
                                "the succeeded outcome's effect {} cannot be read back: {err}",
                                e.id
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok((facts, observed))
}

/// Post-closure discrepancy accounting for one CLOSED work, inside the caller's transaction.
/// The closed row and its outcome are never changed. Each adverse fact gets its own durable,
/// BLOCKING, urgent responsibility (attention `discrepancy:WORK:FACT`): an owner `resolve`
/// cannot clear it and an acknowledgment of one fact never covers another. It is resolved
/// only by the coordinator, when re-derivation from durable rows / a fresh readback shows the
/// fact no longer holds; that observation is recorded as the resolution evidence.
pub(crate) fn sync_discrepancies(tx: &Connection, work: &str) -> Result<(), Error> {
    let mode: String = tx.query_row("SELECT mode FROM work WHERE id=?1", [work], |r| r.get(0))?;
    if mode != "closed" {
        return Ok(());
    }
    let outcome: String = tx.query_row(
        "SELECT outcome FROM work_outcome WHERE work_id=?1",
        [work],
        |r| r.get(0),
    )?;
    let (facts, observed) = adverse_facts(tx, work, &outcome)?;
    let prefix = format!("discrepancy:{work}:");
    let live: Vec<(String, String)> = {
        let mut q = tx.prepare(
            "SELECT id,source_key FROM attention WHERE work_id=?1 AND state<>'resolved' AND substr(source_key,1,length(?2))=?2 ORDER BY created_at,id",
        )?;
        q.query_map(params![work, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, key) in &live {
        let fact = &key[prefix.len()..];
        if facts.iter().any(|f| f.0 == fact) {
            continue;
        }
        let evidence = json!({"fact": fact, "re_derived_observation": observed});
        crate::attention::resolve_attention(
            tx,
            id,
            &format!(
                "discrepancy no longer observed (coordinator re-derivation and readback): {evidence}"
            ),
            "coordinator",
            None,
        )?;
        crate::validation::event(
            tx,
            work,
            "work.discrepancy_resolved",
            id,
            json!({"fact": fact, "evidence": evidence, "outcome_unchanged": outcome}),
        )?;
    }
    for (fact, desc) in facts {
        let reason = format!(
            "DISCREPANCY after closure (recorded outcome {outcome}): {desc}. The outcome record is unchanged; this responsibility stays until the coordinator observes the fact no longer holds."
        );
        if let Some(id) = crate::attention::create_attention(
            tx,
            work,
            None,
            "responsibility",
            &format!("{prefix}{fact}"),
            &reason,
            "urgent",
            true,
        )? {
            crate::validation::event(
                tx,
                work,
                "work.discrepancy_opened",
                &id,
                json!({"fact": fact, "detail": desc, "outcome": outcome, "observed": observed}),
            )?;
        }
    }
    Ok(())
}

/// The post-closure fact for an ATTRIBUTED applied-then-overwritten effect: identity,
/// attribution, the classifying observation and the deadline the work held at closure.
fn overwritten_fact(
    conn: &Connection,
    work: &str,
    e: &EffectRow,
    flags: &Value,
) -> Result<(String, String), Error> {
    let due: Option<i64> = conn
        .query_row(
            "SELECT json_extract(evidence_json,'$.pre_closure_due_at') FROM work_outcome WHERE work_id=?1",
            [work],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let due = match due {
        Some(d) => d,
        None => conn.query_row("SELECT due_at FROM work WHERE id=?1", [work], |r| r.get(0))?,
    };
    Ok((
        format!("effect:{}:conflict_applied_overwritten", e.id),
        format!(
            "effect {} was applied (attribution {}, observation {}) and the target was later overwritten by another writer; inspect the fixture target (responsibility deadline at closure {due})",
            e.id,
            flags["attribution"].as_str().unwrap_or("?"),
            e.result_observation_id.as_deref().unwrap_or("none"),
        ),
    ))
}

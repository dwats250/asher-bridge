//! Gate 5: attributable owner commands (acknowledge, snooze, resolve, cancel, abandon, close).
//!
//! Same boundary as Task 4 decisions: an HMAC over the canonical command under the private owner
//! credential (distinct domain string), a stable command ID, an exact target, a typed action, an
//! expected state version and a canonical payload. Replay is looked up BEFORE current-state
//! validation: identical bytes return the recorded receipt, changed bytes under the same ID are
//! a conflict. The command row and its transition commit together. Agent identity, claims,
//! notification metadata, delivery callbacks and role labels have no path into this module.
use crate::{
    Error, Owner,
    attention::{self, resolve_attention},
    evidence::NoDuplicates,
    protocol::{fingerprint, valid_id},
    review::{Stale, constant_eq, hmac, retire_in},
    store::{barrier, now_ms},
    validation::{event, set_responsibility},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const COMMAND_DOMAIN: &str = "asher-command-v1\n";
pub const MAX_COMMAND_BYTES: usize = 16384;
pub const MAX_SNOOZE_MS: i64 = 3_600_000;
pub const MIN_SNOOZE_MS: i64 = 1_000;
const ACTOR: &str = "local-owner";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandCore {
    pub protocol_version: u32,
    pub command_id: String,
    pub action: String,
    pub target: String,
    pub expected_version: i64,
    pub payload: Value,
    pub actor: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub protocol_version: u32,
    pub command_id: String,
    pub action: String,
    pub target: String,
    pub expected_version: i64,
    pub payload: Value,
    pub actor: String,
    pub owner_proof: String,
}
impl CommandRequest {
    fn core(&self) -> CommandCore {
        CommandCore {
            protocol_version: self.protocol_version,
            command_id: self.command_id.clone(),
            action: self.action.clone(),
            target: self.target.clone(),
            expected_version: self.expected_version,
            payload: self.payload.clone(),
            actor: self.actor.clone(),
        }
    }
}
fn proof(key: &[u8], core: &CommandCore) -> Result<String, Error> {
    Ok(hmac(
        key,
        format!("{COMMAND_DOMAIN}{}", serde_json::to_string(core)?).as_bytes(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckPayload {
    #[serde(default)]
    note: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnoozePayload {
    reason: String,
    duration_ms: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonPayload {
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonPayload {
    reason: String,
    residual_risk: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosePayload {
    outcome: String,
    reason: String,
    evidence_digest: String,
}
fn text(s: &str, what: &str) -> Result<String, Error> {
    let t = s.trim();
    if t.is_empty() || t.len() > 512 {
        return Err(Error::Rejected(format!(
            "payload {what} must be 1-512 characters"
        )));
    }
    Ok(t.to_string())
}
fn parse_payload<T: for<'de> Deserialize<'de>>(v: &Value, action: &str) -> Result<T, Error> {
    serde_json::from_value(v.clone())
        .map_err(|e| Error::Rejected(format!("{action} payload invalid: {e}")))
}
fn split_target(t: &str) -> Option<(&str, &str)> {
    let (k, id) = t.split_once(':')?;
    (matches!(k, "attention" | "work") && valid_id(id)).then_some((k, id))
}

impl Owner {
    /// Owner console: build a signed command (needs a valid credential).
    pub fn command_template(
        &mut self,
        action: &str,
        target: &str,
        command_id: &str,
        expected_version: i64,
        payload: Value,
    ) -> Result<Value, Error> {
        let core = CommandCore {
            protocol_version: 1,
            command_id: command_id.into(),
            action: action.into(),
            target: target.into(),
            expected_version,
            payload,
            actor: ACTOR.into(),
        };
        let p = proof(&self.owner_key()?, &core)?;
        let mut v = serde_json::to_value(&core)?;
        v["owner_proof"] = json!(p);
        Ok(v)
    }

    /// Apply one authenticated owner command.
    pub fn command(&mut self, body: &[u8]) -> Result<Value, Error> {
        let digest = crate::artifact::sha256_hex(body);
        let reject = |o: &mut Owner, kind: &str, entity: &str, why: &str| -> Error {
            match o.audit(
                None,
                kind,
                entity,
                json!({"reason": why, "body_sha256": digest}),
            ) {
                Ok(()) => Error::Rejected(why.into()),
                Err(e) => e,
            }
        };
        if body.len() > MAX_COMMAND_BYTES {
            return Err(reject(
                self,
                "command.rejected",
                "command",
                "command exceeds size bound",
            ));
        }
        if serde_json::from_slice::<NoDuplicates>(body).is_err() {
            return Err(reject(
                self,
                "command.rejected",
                "command",
                "malformed command or duplicate keys",
            ));
        }
        let Ok(req) = serde_json::from_slice::<CommandRequest>(body) else {
            return Err(reject(
                self,
                "command.rejected",
                "command",
                "invalid command envelope (unknown or missing fields)",
            ));
        };
        if req.protocol_version != 1
            || !valid_id(&req.command_id)
            || !matches!(
                req.action.as_str(),
                "ack" | "snooze" | "resolve" | "cancel" | "abandon" | "close"
            )
            || req.owner_proof.len() != 64
        {
            return Err(reject(
                self,
                "command.rejected",
                "command",
                "invalid command fields",
            ));
        }
        let entity = req.command_id.clone();
        if req.actor != ACTOR {
            return Err(reject(
                self,
                "command.forgery",
                &entity,
                "only the authenticated local owner may issue commands",
            ));
        }
        let core = req.core();
        let expected = proof(&self.owner_key()?, &core)?;
        if !constant_eq(&expected, &req.owner_proof) {
            return Err(reject(
                self,
                "command.forgery",
                &entity,
                "owner authentication failed",
            ));
        }
        let Some((tkind, tid)) = split_target(&core.target) else {
            return Err(reject(
                self,
                "command.rejected",
                &entity,
                "target must be attention:ID or work:ID",
            ));
        };
        let (tkind, tid) = (tkind.to_string(), tid.to_string());
        let wanted = if matches!(core.action.as_str(), "ack" | "snooze" | "resolve") {
            "attention"
        } else {
            "work"
        };
        if tkind != wanted {
            return Err(reject(
                self,
                "command.rejected",
                &entity,
                "action does not apply to this target kind",
            ));
        }
        let request_hash = fingerprint(&serde_json::to_string(&core)?);
        // Replay BEFORE current-state validation.
        let prior: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT request_hash,receipt_json FROM owner_command WHERE id=?1",
                [&core.command_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((h, receipt)) = prior {
            if h == request_hash {
                let mut v: Value = serde_json::from_str(&receipt)?;
                v["disposition"] = json!("existing");
                return Ok(v);
            }
            return Err(reject(
                self,
                "command.conflict",
                &entity,
                "command ID was already recorded with different content",
            ));
        }
        let result = match core.action.as_str() {
            "ack" => self.do_ack(&core, &request_hash, &tid),
            "snooze" => self.do_snooze(&core, &request_hash, &tid),
            "resolve" => self.do_resolve(&core, &request_hash, &tid),
            "cancel" => self.do_cancel(&core, &request_hash, &tid),
            "abandon" => self.do_close(&core, &request_hash, &tid, true),
            _ => self.do_close(&core, &request_hash, &tid, false),
        };
        match result {
            Err(Error::Rejected(why)) => {
                let kind = if why.starts_with("stale") {
                    "command.stale"
                } else {
                    "command.refused"
                };
                let work: Option<String> = if tkind == "work" {
                    Some(tid.clone())
                } else {
                    None
                };
                let _ = self.audit(
                    work.as_deref(),
                    kind,
                    &entity,
                    json!({"reason": why, "action": core.action, "target": core.target,
                           "request_hash": request_hash, "authenticated_payload": core.payload}),
                );
                Err(Error::Rejected(why))
            }
            other => other,
        }
    }
}

fn insert_command(
    tx: &Connection,
    core: &CommandCore,
    hash: &str,
    work: Option<&str>,
    receipt: &Value,
    epoch: i64,
) -> Result<(), Error> {
    let (kind, id) = split_target(&core.target).ok_or(Error::Invalid("bad target"))?;
    tx.execute(
        "INSERT INTO owner_command VALUES(?1,?2,?3,?4,?5,'local-owner',?6,?7,?8,?9,?10)",
        params![
            core.command_id,
            core.action,
            kind,
            id,
            work,
            core.expected_version,
            hash,
            receipt.to_string(),
            now_ms()?,
            epoch
        ],
    )?;
    Ok(())
}
fn receipt(core: &CommandCore, result: Value, resolved: bool, closed: bool) -> Value {
    json!({
        "command_id": core.command_id, "action": core.action, "target": core.target,
        "disposition": "recorded", "recorded_at": now_ms().unwrap_or(0),
        "result": result, "approved": false, "resolved": resolved, "work_closed": closed,
        "meaning": "an owner command was authenticated and applied; acknowledgment is not approval and snooze is not resolution",
    })
}
type AttentionRow = (String, String, i64, i64, String);
fn attention_row(tx: &Connection, id: &str) -> Result<AttentionRow, Error> {
    tx.query_row(
        "SELECT state,work_id,version,blocking,kind FROM attention WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Rejected("unknown attention item".into()))
}
fn check_version(have: i64, core: &CommandCore, what: &str) -> Result<(), Error> {
    if have != core.expected_version {
        return Err(Error::Rejected(format!(
            "stale: {what} version is {have}, the command expected {}",
            core.expected_version
        )));
    }
    Ok(())
}

impl Owner {
    fn do_ack(&mut self, core: &CommandCore, hash: &str, id: &str) -> Result<Value, Error> {
        let p: AckPayload = parse_payload(&core.payload, "ack")?;
        if let Some(n) = &p.note {
            text(n, "note")?;
        }
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, work, ver, _b, _k) = attention_row(&tx, id)?;
        check_version(ver, core, "attention")?;
        if state != "open" {
            return Err(Error::Rejected(format!(
                "attention is not open (it is {state})"
            )));
        }
        let r = receipt(
            core,
            json!({"attention_id": id, "attention_state": "acknowledged"}),
            false,
            false,
        );
        insert_command(&tx, core, hash, Some(&work), &r, epoch)?;
        tx.execute(
            "UPDATE attention SET state='acknowledged',acknowledged_by='local-owner',acknowledged_at=?1,acknowledged_command=?2,version=version+1 WHERE id=?3",
            params![now_ms()?, core.command_id, id],
        )?;
        event(
            &tx,
            &work,
            "attention.acknowledged",
            id,
            json!({"command": core.command_id, "note": p.note,
            "meaning": "owner saw it; not approval, not resolution"}),
        )?;
        tx.commit()?;
        Ok(r)
    }

    fn do_snooze(&mut self, core: &CommandCore, hash: &str, id: &str) -> Result<Value, Error> {
        let p: SnoozePayload = parse_payload(&core.payload, "snooze")?;
        let reason = text(&p.reason, "reason")?;
        if !(MIN_SNOOZE_MS..=MAX_SNOOZE_MS).contains(&p.duration_ms) {
            return Err(Error::Rejected(format!(
                "snooze duration_ms must be {MIN_SNOOZE_MS}..={MAX_SNOOZE_MS} (at most one hour)"
            )));
        }
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, work, ver, _b, _k) = attention_row(&tx, id)?;
        check_version(ver, core, "attention")?;
        if state == "resolved" {
            return Err(Error::Rejected(
                "attention is resolved; nothing to snooze".into(),
            ));
        }
        let until = now_ms()? + p.duration_ms;
        let r = receipt(
            core,
            json!({"attention_id": id, "snoozed_until": until, "reason": reason}),
            false,
            false,
        );
        insert_command(&tx, core, hash, Some(&work), &r, epoch)?;
        // ONLY attention timing changes. No work, attempt, review, permit, evidence, cleanup or
        // recovery deadline is read or written here.
        tx.execute(
            "UPDATE attention SET snoozed_until=?1,followup_at=?1,version=version+1 WHERE id=?2",
            params![until, id],
        )?;
        event(
            &tx,
            &work,
            "attention.snoozed",
            id,
            json!({"command": core.command_id, "until": until, "reason": reason,
            "unchanged": "work/attempt/review/permit/evidence/cleanup/recovery deadlines and authority"}),
        )?;
        tx.commit()?;
        Ok(r)
    }

    fn do_resolve(&mut self, core: &CommandCore, hash: &str, id: &str) -> Result<Value, Error> {
        let p: ReasonPayload = parse_payload(&core.payload, "resolve")?;
        let reason = text(&p.reason, "reason")?;
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, work, ver, blocking, kind) = attention_row(&tx, id)?;
        check_version(ver, core, "attention")?;
        if state == "resolved" {
            return Err(Error::Rejected("attention is already resolved".into()));
        }
        if blocking == 1 {
            return Err(Error::Rejected(format!(
                "obligation still live: a blocking {kind} item cannot be resolved by command; change the underlying work (decide, cancel, close)"
            )));
        }
        let r = receipt(core, json!({"attention_id": id}), true, false);
        insert_command(&tx, core, hash, Some(&work), &r, epoch)?;
        resolve_attention(
            &tx,
            id,
            &format!("owner resolved: {reason}"),
            ACTOR,
            Some(&core.command_id),
        )?;
        tx.commit()?;
        Ok(r)
    }

    fn do_cancel(&mut self, core: &CommandCore, hash: &str, work: &str) -> Result<Value, Error> {
        let p: ReasonPayload = parse_payload(&core.payload, "cancel")?;
        let reason = text(&p.reason, "reason")?;
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64)> = tx
            .query_row("SELECT mode,version FROM work WHERE id=?1", [work], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((mode, ver)) = row else {
            return Err(Error::Rejected("unknown work".into()));
        };
        check_version(ver, core, "work")?;
        if mode != "active" {
            return Err(Error::Rejected("work is closed".into()));
        }
        let exists: bool = tx.query_row(
            "SELECT count(*)>0 FROM cancel_request WHERE work_id=?1",
            [work],
            |r| r.get(0),
        )?;
        if exists {
            return Err(Error::Rejected(
                "cancellation was already requested for this work".into(),
            ));
        }
        // Retire an open review, which cancels an un-dispatched approval. Dispatch afterwards is
        // refused. A claimed or unknown effect is NOT touched: it keeps its live obligation.
        let open: Vec<String> = {
            let mut q = tx.prepare(
                "SELECT id FROM review WHERE work_id=?1 AND state IN ('pending','authorized')",
            )?;
            q.query_map([work], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let mut retired = false;
        for rid in &open {
            retired |= retire_in(
                &tx,
                rid,
                &Stale {
                    state: "invalidated",
                    reason: "cancellation requested by the owner".into(),
                },
            )?;
        }
        let mut hazards: Vec<String> = Vec::new();
        let att: Option<(String, String)> = tx
            .query_row(
                "SELECT id,state FROM attempt WHERE work_id=?1 ORDER BY created_at DESC LIMIT 1",
                [work],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((a, s)) = att {
            if !matches!(s.as_str(), "cleaned" | "intent") {
                hazards.push(format!("attempt {a} is {s}: live-attempt slot retained"));
            }
        }
        let eff: Option<(String, String)> = tx
            .query_row(
                "SELECT id,state FROM effect WHERE work_id=?1 ORDER BY created_at DESC LIMIT 1",
                [work],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((e, s)) = eff {
            if matches!(s.as_str(), "dispatching" | "unknown") {
                hazards.push(format!("effect {e} is {s}: possible effect unresolved"));
            }
            if s == "conflict" {
                let row = crate::effect::effects_where(&tx, "id=?1", &e)?;
                if let Some(r) = row.first() {
                    let fl = crate::effect::effect_flags(&tx, r)?;
                    if !matches!(
                        fl["attribution"].as_str(),
                        Some("audit_entry" | "executor_reported_cas")
                    ) {
                        hazards.push(format!("effect {e} is in conflict with unproven contribution: reconciliation responsibility retained"));
                    }
                }
            }
        }
        // The request itself becomes the owner's visible, blocking responsibility (with a fresh
        // deadline) unless a live hazard already owns the work; then the hazard's responsibility
        // stays, and once it settles no forward-progress responsibility can replace the
        // cancellation (see attention::route). Every forward command is fenced from here on.
        if hazards.is_empty() {
            set_responsibility(&tx, work, "blocked", ACTOR, attention::CANCEL_ACTION)?;
        }
        let r = receipt(
            core,
            json!({"cancel_requested": true, "terminal": false, "hazards": hazards,
                   "review_retired": retired, "reason": reason,
                   "note": "requesting cancellation does not undo any effect and is not itself a terminal transition"}),
            false,
            false,
        );
        insert_command(&tx, core, hash, Some(work), &r, epoch)?;
        tx.execute(
            "INSERT INTO cancel_request VALUES(?1,?2,?3,?4)",
            params![work, core.command_id, reason, now_ms()?],
        )?;
        event(
            &tx,
            work,
            "work.cancel_requested",
            work,
            json!({"command": core.command_id, "reason": reason, "hazards": hazards}),
        )?;
        tx.commit()?;
        Ok(r)
    }

    /// `close` (explicit outcome with the reviewed evidence digest) and `abandon`.
    fn do_close(
        &mut self,
        core: &CommandCore,
        hash: &str,
        work: &str,
        abandon: bool,
    ) -> Result<Value, Error> {
        let (outcome, reason, residual, digest) = if abandon {
            let p: AbandonPayload = parse_payload(&core.payload, "abandon")?;
            (
                "abandoned".to_string(),
                text(&p.reason, "reason")?,
                Some(text(&p.residual_risk, "residual_risk")?),
                None,
            )
        } else {
            let p: ClosePayload = parse_payload(&core.payload, "close")?;
            if p.outcome == "abandoned" {
                return Err(Error::Rejected(
                    "use the abandon command for abandonment".into(),
                ));
            }
            if !crate::outcome::OUTCOMES.contains(&p.outcome.as_str()) {
                return Err(Error::Rejected(
                    "close payload invalid: unsupported outcome".into(),
                ));
            }
            (
                p.outcome,
                text(&p.reason, "reason")?,
                None,
                Some(p.evidence_digest),
            )
        };
        let version: i64 = self
            .conn
            .query_row("SELECT version FROM work WHERE id=?1", [work], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::Rejected("unknown work".into()))?;
        check_version(version, core, "work")?;
        let s = self.summary_for(work, &outcome)?;
        if !s.eligible {
            return Err(Error::Rejected(format!(
                "closure refused: {}",
                s.blockers.join("; ")
            )));
        }
        if let Some(d) = &digest {
            if *d != s.digest {
                return Err(Error::Rejected(
                    "stale: the closure evidence changed since the summary the owner reviewed (digest mismatch)".into(),
                ));
            }
        }
        let epoch = self.epoch;
        barrier("before_closure_commit")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // DB-derived evidence must be unchanged inside the transaction that closes the work.
        let (blockers, db_now) = crate::outcome::db_evidence_now(&tx, work, &outcome)?;
        let v2: i64 = tx.query_row("SELECT version FROM work WHERE id=?1", [work], |r| r.get(0))?;
        if v2 != version || !blockers.is_empty() || db_now != s.db_evidence {
            return Err(Error::Rejected(
                "stale: work or evidence changed while closing; nothing was closed".into(),
            ));
        }
        let now = now_ms()?;
        let mut evidence = s.evidence.clone();
        // The deadline the work held when it closed travels with carried-forward obligations.
        let pre_due: i64 =
            tx.query_row("SELECT due_at FROM work WHERE id=?1", [work], |r| r.get(0))?;
        evidence["pre_closure_due_at"] = json!(pre_due);
        if let Some(r) = &residual {
            evidence["owner_residual_risk"] = json!(r);
        }
        let risks: Vec<String> = db_now["residual_risks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut notice_reason = if outcome == "succeeded" {
            format!("Work closed: SUCCEEDED. {reason}")
        } else {
            format!(
                "NOT SUCCESS: work closed as {}. {reason}",
                outcome.to_uppercase()
            )
        };
        if !risks.is_empty() {
            // The residual obligation travels with the required notice, not only the record.
            notice_reason.push_str(&format!(" RESIDUAL RISK: {}", risks.join("; ")));
        }
        let severity = if outcome == "succeeded" {
            "info"
        } else if risks.is_empty() {
            "action"
        } else {
            "urgent"
        };
        let r = receipt(
            core,
            json!({"closed": true, "outcome": outcome, "evidence_digest": s.digest, "work_success": outcome == "succeeded"}),
            false,
            true,
        );
        insert_command(&tx, core, hash, Some(work), &r, epoch)?;
        // Retire every un-launched intent in the SAME transaction: closed work never launches.
        if let Some(ids) = db_now["pending_launch_intents_retired_by_closure"].as_array() {
            for id in ids.iter().filter_map(|v| v.as_str()) {
                let n = tx.execute(
                    "UPDATE attempt SET reason=?1 WHERE id=?2 AND work_id=?3 AND state='intent'
                       AND NOT EXISTS (SELECT 1 FROM supervisor_command WHERE attempt_id=?2)",
                    params![
                        format!(
                            "{} by owner command {} ({outcome}): never launched; launch refused",
                            crate::execution::RETIRED_PREFIX,
                            core.command_id
                        ),
                        id,
                        work
                    ],
                )?;
                if n != 1 {
                    return Err(Error::Rejected(
                        "stale: launch intent changed while closing; nothing was closed".into(),
                    ));
                }
                event(
                    &tx,
                    work,
                    "attempt.retired",
                    id,
                    json!({"command": core.command_id, "outcome": outcome,
                           "meaning": "un-launched intent retired with the closure; it can never launch"}),
                )?;
            }
        }
        // Blocking attention for the now-closed responsibility is resolved by this owner command.
        let live: Vec<String> = {
            let mut q = tx.prepare(
                "SELECT id FROM attention WHERE work_id=?1 AND blocking=1 AND state<>'resolved'",
            )?;
            q.query_map([work], |x| x.get(0))?
                .collect::<Result<_, _>>()?
        };
        for id in &live {
            resolve_attention(
                &tx,
                id,
                &format!("work closed: {outcome}"),
                ACTOR,
                Some(&core.command_id),
            )?;
        }
        // The required nonblocking outcome notice: if it cannot be written the closure fails.
        let notice = attention::create_attention(
            &tx,
            work,
            None,
            "outcome",
            &format!("outcome:{work}"),
            &notice_reason,
            severity,
            false,
        )?
        .ok_or_else(|| Error::Rejected("outcome notice already exists".into()))?;
        tx.execute(
            "INSERT INTO work_outcome VALUES(?1,?2,'local-owner',?3,?4,?5,?6,?7,?8)",
            params![
                work,
                outcome,
                reason,
                evidence.to_string(),
                s.digest,
                core.command_id,
                notice,
                now
            ],
        )?;
        tx.execute(
            "UPDATE work SET mode='closed',phase='done',next_owner='none',next_action='none',due_at=?1,version=version+1 WHERE id=?2",
            params![now, work],
        )?;
        // Open evidence obligations disclosed as `carried_forward_discrepancies` (and any other
        // adverse fact) become durable discrepancy responsibilities in this same transaction.
        crate::outcome::sync_discrepancies(&tx, work)?;
        event(
            &tx,
            work,
            "work.closed",
            work,
            json!({"outcome": outcome, "command": core.command_id,
            "evidence_digest": s.digest, "notice_attention_id": notice, "reason": reason}),
        )?;
        tx.commit()?;
        Ok(r)
    }
}

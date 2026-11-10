//! Typed evidence envelopes. Authority, source and kind are assigned by coordinator code
//! paths; the public ingestion API admits agent claims only. ACK follows commit.
use crate::{
    Error, Owner,
    protocol::{fingerprint, valid_id},
    store::{barrier, now_ms},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_CLAIM_BYTES: usize = 65536;
/// Freshness is derived here, never granted by a producer.
pub const CLAIM_FRESH_MS: i64 = 15_000;
pub const TRUSTED_FRESH_MS: i64 = 3_600_000;
pub const MAX_FUTURE_SKEW_MS: i64 = 5_000;
pub const VALIDATOR_EPOCH: &str = "fixture-validator-v1";
const MAX_GAPS: usize = 32;
const MAX_GAP_LEN: usize = 512;
/// Envelope fields a caller may never select. Their presence is audited as forgery.
const FORBIDDEN: [&str; 12] = [
    "source",
    "authority",
    "kind",
    "role",
    "trusted",
    "id",
    "evidence_id",
    "work_id",
    "attempt_id",
    "received_at",
    "payload_hash",
    "producer",
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Gap,
    Unknown,
}
impl Coverage {
    pub fn as_str(self) -> &'static str {
        match self {
            Coverage::Complete => "complete",
            Coverage::Partial => "partial",
            Coverage::Gap => "gap",
            Coverage::Unknown => "unknown",
        }
    }
    fn parse(s: &str) -> Result<Self, Error> {
        serde_json::from_value(Value::String(s.into())).map_err(|_| Error::Corrupt)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    source_epoch: String,
    source_seq: i64,
    #[serde(default)]
    artifact_id: Option<String>,
    observed_at: i64,
    coverage: Coverage,
    gaps: Vec<String>,
    payload: Value,
}
/// Full replay identity: associations, time, coverage and gaps as well as payload.
#[derive(Serialize)]
struct Envelope<'a> {
    version: u32,
    authority: &'a str,
    source: &'a str,
    source_epoch: &'a str,
    source_seq: i64,
    kind: &'a str,
    work_id: &'a str,
    attempt_id: &'a str,
    artifact_id: Option<&'a str>,
    observed_at: i64,
    coverage: &'a str,
    gaps: &'a [String],
    payload_hash: &'a str,
}
pub(crate) struct NewEvidence<'a> {
    pub authority: &'a str,
    pub source: String,
    pub source_epoch: String,
    pub source_seq: i64,
    pub kind: &'a str,
    pub work_id: &'a str,
    pub attempt_id: &'a str,
    pub artifact_id: Option<&'a str>,
    pub observed_at: i64,
    pub coverage: Coverage,
    pub gaps: Vec<String>,
    pub payload: Value,
}
impl NewEvidence<'_> {
    fn payload_hash(&self) -> String {
        fingerprint(&self.payload.to_string())
    }
    fn envelope_hash(&self) -> Result<String, Error> {
        let ph = self.payload_hash();
        Ok(fingerprint(&serde_json::to_string(&Envelope {
            version: 1,
            authority: self.authority,
            source: &self.source,
            source_epoch: &self.source_epoch,
            source_seq: self.source_seq,
            kind: self.kind,
            work_id: self.work_id,
            attempt_id: self.attempt_id,
            artifact_id: self.artifact_id,
            observed_at: self.observed_at,
            coverage: self.coverage.as_str(),
            gaps: &self.gaps,
            payload_hash: &ph,
        })?))
    }
}
/// Trusted authority requires the validator capability; claims must not present one.
pub(crate) fn insert(
    tx: &Connection,
    origin: Option<&crate::validation::TrustedOrigin>,
    e: &NewEvidence,
    received: i64,
    epoch: i64,
) -> Result<String, Error> {
    let trusted = e.authority == "trusted_validator";
    if trusted != origin.is_some() || (!trusted && e.authority != "agent_claim") {
        return Err(Error::Invalid(
            "evidence authority requires matching origin capability",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO evidence(id,work_id,attempt_id,artifact_id,authority,source,source_epoch,source_seq,kind,observed_at,received_at,coverage,gaps_json,payload_hash,payload_json,envelope_hash,coordinator_epoch) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![id,e.work_id,e.attempt_id,e.artifact_id,e.authority,e.source,e.source_epoch,e.source_seq,e.kind,e.observed_at,received,e.coverage.as_str(),serde_json::to_string(&e.gaps)?,e.payload_hash(),e.payload.to_string(),e.envelope_hash()?,epoch],
    )?;
    tx.execute(
        "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'coordinator','evidence.committed',?2,?3,?4)",
        params![e.work_id,id,received,json!({"authority":e.authority,"kind":e.kind,"source":e.source,"source_epoch":e.source_epoch,"source_seq":e.source_seq,"artifact_id":e.artifact_id}).to_string()],
    )?;
    Ok(id)
}
pub(crate) fn next_trusted_seq(tx: &Connection, source: &str) -> Result<i64, Error> {
    Ok(tx.query_row(
        "SELECT COALESCE(MAX(source_seq),0)+1 FROM evidence WHERE source=?1 AND source_epoch=?2",
        params![source, VALIDATOR_EPOCH],
        |r| r.get(0),
    )?)
}

#[derive(Debug, Serialize)]
pub struct ClaimAck {
    pub id: String,
    pub disposition: &'static str,
    pub authority: &'static str,
    pub trusted: bool,
    pub committed: bool,
    pub work_success: bool,
}
fn audit_reject(
    tx: Transaction<'_>,
    work: Option<&str>,
    entity: &str,
    reason: &str,
    body: &[u8],
    forged: &[&str],
) -> Result<ClaimAck, Error> {
    tx.execute(
        "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'untrusted-claimant','evidence.rejected',?2,?3,?4)",
        params![work,entity,now_ms()?,json!({"reason":reason,"forged_fields":forged,"body_sha256":crate::artifact::sha256_hex(body),"body_len":body.len()}).to_string()],
    )?;
    tx.commit()?;
    Err(Error::Rejected(reason.into()))
}

impl Owner {
    /// Ingest one untrusted agent claim. Never produces trusted evidence.
    pub fn ingest_claim(&mut self, attempt: &str, body: &[u8]) -> Result<ClaimAck, Error> {
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        let entity = if valid_id(attempt) {
            attempt
        } else {
            "invalid-attempt-id"
        };
        let work: Option<String> = if valid_id(attempt) {
            tx.query_row("SELECT work_id FROM attempt WHERE id=?1", [attempt], |r| {
                r.get(0)
            })
            .optional()?
        } else {
            None
        };
        let Some(work) = work else {
            return audit_reject(tx, None, entity, "unknown attempt", body, &[]);
        };
        let w = Some(work.as_str());
        if body.len() > MAX_CLAIM_BYTES {
            return audit_reject(tx, w, entity, "claim exceeds size bound", body, &[]);
        }
        if serde_json::from_slice::<NoDuplicates>(body).is_err() {
            return audit_reject(
                tx,
                w,
                entity,
                "malformed JSON claim or duplicate keys",
                body,
                &[],
            );
        }
        let Ok(raw) = serde_json::from_slice::<Value>(body) else {
            return audit_reject(tx, w, entity, "malformed JSON claim", body, &[]);
        };
        let forged: Vec<&str> = raw
            .as_object()
            .map(|o| {
                FORBIDDEN
                    .into_iter()
                    .filter(|k| o.contains_key(*k))
                    .collect()
            })
            .unwrap_or_default();
        if !forged.is_empty() {
            return audit_reject(
                tx,
                w,
                entity,
                "caller-selected authority, source, kind or identity fields are refused",
                body,
                &forged,
            );
        }
        let Ok(c) = serde_json::from_value::<ClaimBody>(raw) else {
            return audit_reject(tx, w, entity, "invalid claim envelope", body, &[]);
        };
        let gaps_ok = c.gaps.len() <= MAX_GAPS
            && c.gaps
                .iter()
                .all(|g| !g.trim().is_empty() && g.len() <= MAX_GAP_LEN);
        let coverage_ok = (c.coverage == Coverage::Complete) == c.gaps.is_empty();
        if !valid_id(&c.source_epoch)
            || c.source_seq <= 0
            || c.observed_at < 0
            || !gaps_ok
            || !coverage_ok
        {
            return audit_reject(
                tx,
                w,
                entity,
                "invalid sequence, time, coverage or gap disclosure",
                body,
                &[],
            );
        }
        if let Some(a) = &c.artifact_id {
            let owner: Option<(String, String)> = if valid_id(a) {
                tx.query_row(
                    "SELECT work_id,attempt_id FROM artifact WHERE id=?1",
                    [a],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
            } else {
                None
            };
            if owner != Some((work.clone(), attempt.to_string())) {
                return audit_reject(
                    tx,
                    w,
                    entity,
                    "artifact is not associated with this work and attempt",
                    body,
                    &[],
                );
            }
        }
        let e = NewEvidence {
            authority: "agent_claim",
            source: format!("agent:{attempt}"),
            source_epoch: c.source_epoch,
            source_seq: c.source_seq,
            kind: "agent_claim",
            work_id: &work,
            attempt_id: attempt,
            artifact_id: c.artifact_id.as_deref(),
            observed_at: c.observed_at,
            coverage: c.coverage,
            gaps: c.gaps,
            payload: c.payload,
        };
        let hash = e.envelope_hash()?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT id,envelope_hash FROM evidence WHERE source=?1 AND source_epoch=?2 AND source_seq=?3",
                params![e.source, e.source_epoch, e.source_seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, old)) = prior {
            if old == hash {
                tx.commit()?;
                return Ok(ClaimAck {
                    id,
                    disposition: "existing",
                    authority: "agent_claim",
                    trusted: false,
                    committed: true,
                    work_success: false,
                });
            }
            tx.execute(
                "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'untrusted-claimant','evidence.conflict',?2,?3,?4)",
                params![work,id,now,json!({"source":e.source,"source_epoch":e.source_epoch,"source_seq":e.source_seq,"committed_envelope":old,"submitted_envelope":hash}).to_string()],
            )?;
            tx.commit()?;
            return Err(Error::EvidenceConflict);
        }
        if e.observed_at > now + MAX_FUTURE_SKEW_MS {
            return audit_reject(
                tx,
                w,
                entity,
                "observation time is in the future",
                body,
                &[],
            );
        }
        let id = insert(&tx, None, &e, now, epoch)?;
        barrier("before_evidence_commit")?;
        tx.commit()?;
        barrier("after_evidence_commit")?;
        Ok(ClaimAck {
            id,
            disposition: "accepted",
            authority: "agent_claim",
            trusted: false,
            committed: true,
            work_success: false,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceRow {
    pub id: String,
    pub kind: String,
    pub authority: String,
    pub source: String,
    pub source_epoch: String,
    pub source_seq: i64,
    pub artifact_id: Option<String>,
    pub artifact_state: Option<String>,
    pub observed_at: i64,
    pub received_at: i64,
    pub coverage: String,
    pub gaps: Vec<String>,
    pub payload: Value,
    pub fresh: bool,
    pub fresh_until: i64,
    pub sequence_complete: bool,
    pub intact: bool,
    pub conflicts: i64,
    pub trusted: bool,
}
/// Coordinator-derived view. `trusted` requires validator authority, an intact envelope
/// and a still-valid associated artifact. Claims are never trusted.
pub fn rows(conn: &Connection, attempt: &str, now: i64) -> Result<Vec<EvidenceRow>, Error> {
    let mut q = conn.prepare(
        "SELECT e.id,e.kind,e.authority,e.source,e.source_epoch,e.source_seq,e.artifact_id,a.state,e.observed_at,e.received_at,e.coverage,e.gaps_json,e.payload_json,e.payload_hash,e.envelope_hash,e.work_id,e.attempt_id
         FROM evidence e LEFT JOIN artifact a ON a.id=e.artifact_id WHERE e.attempt_id=?1 ORDER BY e.source,e.source_epoch,e.source_seq",
    )?;
    type Raw = (
        String,
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let raw: Vec<Raw> = q
        .query_map([attempt], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
                r.get(16)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (
        id,
        kind,
        authority,
        source,
        epoch,
        seq,
        art,
        art_state,
        obs,
        rec,
        cov,
        gaps,
        payload,
        ph,
        eh,
        work,
        att,
    ) in raw
    {
        let gaps: Vec<String> = serde_json::from_str(&gaps)?;
        let payload: Value = serde_json::from_str(&payload)?;
        let recomputed = NewEvidence {
            authority: &authority,
            source: source.clone(),
            source_epoch: epoch.clone(),
            source_seq: seq,
            kind: &kind,
            work_id: &work,
            attempt_id: &att,
            artifact_id: art.as_deref(),
            observed_at: obs,
            coverage: Coverage::parse(&cov)?,
            gaps: gaps.clone(),
            payload: payload.clone(),
        };
        let intact = recomputed.payload_hash() == ph && recomputed.envelope_hash()? == eh;
        let window = if authority == "trusted_validator" {
            TRUSTED_FRESH_MS
        } else {
            CLAIM_FRESH_MS
        };
        let below: i64 = conn.query_row(
            "SELECT count(*) FROM evidence WHERE source=?1 AND source_epoch=?2 AND source_seq<=?3",
            params![source, epoch, seq],
            |r| r.get(0),
        )?;
        let conflicts: i64 = conn.query_row(
            "SELECT count(*) FROM event WHERE kind='evidence.conflict' AND entity_id=?1",
            [&id],
            |r| r.get(0),
        )?;
        let trusted = authority == "trusted_validator"
            && intact
            && art_state.as_deref() == Some("valid")
            && conflicts == 0;
        out.push(EvidenceRow {
            fresh: obs <= now && now - obs <= window,
            fresh_until: obs.saturating_add(window),
            sequence_complete: below == seq,
            id,
            kind,
            authority,
            source,
            source_epoch: epoch,
            source_seq: seq,
            artifact_id: art,
            artifact_state: art_state,
            observed_at: obs,
            received_at: rec,
            coverage: cov,
            gaps,
            payload,
            intact,
            conflicts,
            trusted,
        });
    }
    Ok(out)
}

/// Strict pre-parse: duplicate object keys are refused rather than last-one-wins.
pub(crate) struct NoDuplicates;
impl<'de> Deserialize<'de> for NoDuplicates {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NoDupVisitor)
    }
}
struct NoDupVisitor;
impl<'de> serde::de::Visitor<'de> for NoDupVisitor {
    type Value = NoDuplicates;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON without duplicate keys")
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoDuplicates)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<NoDuplicates>()?.is_some() {}
        Ok(NoDuplicates)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(k) = map.next_key::<String>()? {
            if !seen.insert(k) {
                return Err(serde::de::Error::custom("duplicate key"));
            }
            map.next_value::<NoDuplicates>()?;
        }
        Ok(NoDuplicates)
    }
}

#[cfg(test)]
mod tests {
    use super::NoDuplicates;
    #[test]
    fn duplicate_keys_are_refused_at_any_depth() {
        let ok = br#"{"a":1,"b":{"c":[{"d":null,"e":true}]}}"#;
        assert!(serde_json::from_slice::<NoDuplicates>(ok).is_ok());
        for bad in [
            &br#"{"a":1,"a":2}"#[..],
            br#"{"p":{"x":1,"x":1}}"#,
            br#"[{"k":"v","k":"w"}]"#,
        ] {
            assert!(serde_json::from_slice::<NoDuplicates>(bad).is_err());
        }
    }
}

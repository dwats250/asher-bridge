//! Trusted in-process fixture checks over frozen snapshots. No subprocess, no repository
//! scripts, no Git. Readiness here is a future review boundary, never work success.
use crate::{
    Error, Owner,
    artifact::{self, Scan},
    evidence::{self, Coverage, NewEvidence, VALIDATOR_EPOCH},
    execution::{FakeMode, Spec, fixture_path},
    process_scope::{self, Identity},
    protocol::valid_id,
    store::{barrier, now_ms},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};

pub const INITIAL_SYMBOLS: &[u8] = b"symbol,scope\nBAD SYMBOL,core\n";
pub const PREPARED_SYMBOLS: &[u8] = b"symbol,scope\nFIXTURE_OK,core\n";
/// Explicitly incomplete synthetic capture. Nothing in the prototype can restore it.
pub const CAPTURE: &[u8] = b"{\"version\":1,\"interval\":{\"from\":\"2026-01-01T00:00:00Z\",\"to\":\"2026-01-01T06:00:00Z\"},\"complete\":false,\"missing\":[{\"from\":\"2026-01-01T01:00:00Z\",\"to\":\"2026-01-01T01:05:00Z\",\"reason\":\"simulated collection gap\"}]}\n";
pub const VALIDATOR_VERSION: &str = "fixture-symbol-check-v1";
const RESPONSE_MS: i64 = 10_000;
/// Hard bound on owner-requested trusted revalidations per attempt (no scheduling).
pub const MAX_REVALIDATIONS: i64 = 3;
pub(crate) const MALFORMED: &str = "malformed symbol";

pub fn symbol_spec() -> Spec {
    Spec {
        mode: FakeMode::PrepareSymbol,
        runtime_ms: 1000,
        output_limit: 16384,
    }
}
/// Proof-of-origin capability for validator evidence; constructible only in this module.
pub(crate) struct TrustedOrigin {
    _private: (),
}

/// Disposable fixed layout: `initial/` and `candidate/`, each `symbols.csv` + `capture.json`.
pub(crate) fn create_symbol_layout(dir: &Path) -> Result<(), Error> {
    for sub in ["initial", "candidate"] {
        let d = dir.join(sub);
        fs::DirBuilder::new().mode(artifact::DIR_MODE).create(&d)?;
        for (name, data) in [("symbols.csv", INITIAL_SYMBOLS), ("capture.json", CAPTURE)] {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(artifact::FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(d.join(name))?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        fs::File::open(&d)?.sync_all()?;
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SymbolCheck {
    pub valid: bool,
    pub symbol: Option<String>,
    pub finding: &'static str,
}
/// Strict fixed-format check: header, exactly one row, symbol charset, scope, expected value.
pub fn check_symbols(bytes: &[u8]) -> SymbolCheck {
    let fail = |symbol: Option<&str>, finding| SymbolCheck {
        valid: false,
        symbol: symbol.map(str::to_string),
        finding,
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return fail(None, "not UTF-8");
    };
    let Some(body) = text.strip_suffix('\n') else {
        return fail(None, "missing final newline");
    };
    let lines: Vec<&str> = body.split('\n').collect();
    if lines.first() != Some(&"symbol,scope") {
        return fail(None, "unexpected header");
    }
    if lines.len() != 2 {
        return fail(None, "expected exactly one symbol row");
    }
    let fields: Vec<&str> = lines[1].split(',').collect();
    if fields.len() != 2 {
        return fail(None, "malformed row");
    }
    let (symbol, scope) = (fields[0], fields[1]);
    let well_formed = (1..=32).contains(&symbol.len())
        && symbol.as_bytes()[0].is_ascii_uppercase()
        && symbol
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if !well_formed {
        return fail(Some(symbol), MALFORMED);
    }
    if scope != "core" {
        return fail(Some(symbol), "unsupported scope");
    }
    if symbol != "FIXTURE_OK" {
        return fail(
            Some(symbol),
            "well-formed symbol is not the expected correction",
        );
    }
    SymbolCheck {
        valid: true,
        symbol: Some(symbol.into()),
        finding: "symbol well-formed and equals FIXTURE_OK in core",
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Interval {
    from: String,
    to: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Missing {
    from: String,
    to: String,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Capture {
    version: u32,
    interval: Interval,
    complete: bool,
    missing: Vec<Missing>,
}
/// Capture coverage is observed, never inferred from symbol repair. Returns coverage
/// and disclosed gaps; `restored` is always false: no independent capture source exists.
pub(crate) fn observe_capture(bytes: &[u8]) -> (Coverage, Vec<String>) {
    match serde_json::from_slice::<Capture>(bytes) {
        Ok(c) if c.version == 1 && !c.complete && !c.missing.is_empty() => (
            Coverage::Gap,
            c.missing
                .iter()
                .take(16)
                .map(|m| {
                    format!(
                        "capture interval {}..{} missing: {}",
                        m.from, m.to, m.reason
                    )
                    .chars()
                    .take(256)
                    .collect()
                })
                .collect(),
        ),
        Ok(c) if c.version == 1 && c.complete => (
            Coverage::Unknown,
            vec![
                "fixture asserts complete capture but no independent capture evidence exists"
                    .into(),
            ],
        ),
        _ => (
            Coverage::Unknown,
            vec!["capture description unreadable or unsupported".into()],
        ),
    }
}

pub(crate) struct Ctx {
    pub(crate) work: String,
    pub(crate) spec: Spec,
    pub(crate) state: String,
    pub(crate) receipt: bool,
}
pub(crate) fn ctx(conn: &Connection, attempt: &str) -> Result<Ctx, Error> {
    if !valid_id(attempt) {
        return Err(Error::Invalid("invalid attempt ID"));
    }
    let (work, spec, state): (String, String, String) = conn
        .query_row(
            "SELECT work_id,spec_json,state FROM attempt WHERE id=?1",
            [attempt],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(Error::Invalid("unknown attempt"))?;
    let receipt: bool = conn.query_row(
        "SELECT count(*)>0 FROM supervisor_command WHERE attempt_id=?1",
        [attempt],
        |r| r.get(0),
    )?;
    Ok(Ctx {
        work,
        spec: serde_json::from_str(&spec)?,
        state,
        receipt,
    })
}
#[derive(Debug, Clone)]
pub(crate) struct ArtifactRow {
    pub(crate) id: String,
    pub(crate) work: String,
    pub(crate) attempt: String,
    pub(crate) role: String,
    pub(crate) manifest_json: String,
    pub(crate) manifest_hash: String,
    pub(crate) identity_json: String,
    pub(crate) state: String,
    pub(crate) invalid_reason: Option<String>,
}
pub(crate) const ARTIFACT_COLUMNS: &str =
    "id,work_id,attempt_id,role,manifest_json,manifest_hash,identity_json,state,invalid_reason";
pub(crate) fn artifact_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: r.get(0)?,
        work: r.get(1)?,
        attempt: r.get(2)?,
        role: r.get(3)?,
        manifest_json: r.get(4)?,
        manifest_hash: r.get(5)?,
        identity_json: r.get(6)?,
        state: r.get(7)?,
        invalid_reason: r.get(8)?,
    })
}
pub(crate) fn artifact_for(
    conn: &Connection,
    attempt: &str,
    role: &str,
) -> Result<Option<ArtifactRow>, Error> {
    Ok(conn
        .query_row(
            &format!("SELECT {ARTIFACT_COLUMNS} FROM artifact WHERE attempt_id=?1 AND role=?2"),
            params![attempt, role],
            artifact_row,
        )
        .optional()?)
}
/// Changes responsibility only on a genuine transition: repeated calls never postpone.
pub(crate) fn set_responsibility(
    tx: &Connection,
    work: &str,
    phase: &str,
    owner: &str,
    action: &str,
) -> Result<(), Error> {
    let Some((phase, owner, action)) = crate::attention::route(tx, work, phase, owner, action)?
    else {
        return Ok(()); // closed: recorded as post-closure evidence, row untouched
    };
    let (p, o, a): (String, String, String) = tx.query_row(
        "SELECT phase,next_owner,next_action FROM work WHERE id=?1",
        [work],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if (p, o, a) != (phase.clone(), owner.clone(), action.clone()) {
        tx.execute(
            "UPDATE work SET phase=?1,next_owner=?2,next_action=?3,due_at=?4,version=version+1 WHERE id=?5",
            params![phase, owner, action, now_ms()? + RESPONSE_MS, work],
        )?;
        crate::attention::on_transition(tx, work)?;
    }
    Ok(())
}
pub(crate) fn event(
    tx: &Connection,
    work: &str,
    kind: &str,
    entity: &str,
    payload: Value,
) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'coordinator',?2,?3,?4,?5)",
        params![work, kind, entity, now_ms()?, payload.to_string()],
    )?;
    Ok(())
}
fn reason(e: Error) -> String {
    match e {
        Error::Rejected(s) => s,
        e => e.to_string(),
    }
}
fn proc_identity(v: &Value) -> Result<Identity, String> {
    serde_json::from_value(v["identity"].clone()).map_err(|_| "unreadable recorded identity".into())
}
/// The supervisor's captured output against what it recorded at cleanup (outcome 2): size and
/// the SHA-256 it took of the bytes as it wrote them. `None` when they agree, or when another
/// check owns the question: no cleanup record yet, an unreadable record (a corrupt fact), or a
/// fixture directory that is gone (M1 artifact loss). A missing capture, one of a different
/// size or type, one whose bytes differ, and one whose cleanup record carries no usable digest
/// are each an evidence gap. The gap kinds are the ones migration 008 already allows:
/// everything but absence is `capture_torn`, told apart by `why` in its detail. The digest is only ever compared here, never produced: nothing on
/// this path hashes the file and stores the result as if the supervisor had seen it.
pub(crate) fn capture_gap(
    conn: &Connection,
    attempt: &str,
) -> Result<Option<(&'static str, Value)>, Error> {
    let mut q = conn.prepare(
        "SELECT payload_json FROM supervisor_fact WHERE attempt_id=?1 AND kind='cleanup_completed' ORDER BY seq DESC",
    )?;
    let payloads = q
        .query_map([attempt], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let Some(Ok(v)) = payloads.first().map(|p| serde_json::from_str::<Value>(p)) else {
        return Ok(None);
    };
    let (dir, spec): (String, String) = conn.query_row(
        "SELECT fixture_path,spec_json FROM attempt WHERE id=?1",
        [attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let dir = Path::new(&dir);
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let limit = serde_json::from_str::<Spec>(&spec)?.output_limit;
    // Records written before M2 carry only the observed byte count; the capture holds at most
    // the output limit of it. A record naming no size at all can only be checked for presence.
    let recorded = match (v["bytes_captured"].as_u64(), v["bytes_observed"].as_u64()) {
        (Some(c), _) => Some(c),
        (None, Some(o)) => Some(o.min(limit)),
        (None, None) => None,
    };
    let path = dir.join("captured.log");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some((
                "capture_missing",
                json!({"recorded_bytes":recorded}),
            )));
        }
        Err(e) => return Err(e.into()),
        Ok(m) if !m.is_file() => {
            return Ok(Some((
                "capture_torn",
                json!({"recorded_bytes":recorded,"why":"capture is not a regular file"}),
            )));
        }
        Ok(m) if recorded.is_some_and(|r| m.len() != r) => {
            return Ok(Some((
                "capture_torn",
                json!({"recorded_bytes":recorded,"found_bytes":m.len()}),
            )));
        }
        Ok(_) => (),
    }
    // One supervisor run writes one cleanup record. A second one that names a different
    // capture leaves no way to tell which describes the bytes the supervisor wrote.
    let named = |x: &Value| (x["bytes_captured"].clone(), x["capture_sha256"].clone());
    if payloads[1..]
        .iter()
        .any(|p| serde_json::from_str::<Value>(p).map_or(true, |other| named(&other) != named(&v)))
    {
        return Ok(Some((
            "capture_torn",
            json!({"why":"cleanup records disagree about the capture","records":payloads.len()}),
        )));
    }
    let Some(want) = v["capture_sha256"]
        .as_str()
        .filter(|d| d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    else {
        return Ok(Some((
            "capture_torn",
            json!({"recorded_bytes":recorded,"why":"cleanup record carries no capture digest"}),
        )));
    };
    // Bounded read without following a link: one byte past the output limit is already proof
    // of a capture the supervisor did not write. An unreadable capture is an error, which every
    // caller treats as unknown, never as agreement.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Ok(Some((
            "capture_torn",
            json!({"recorded_bytes":recorded,"why":"capture is not a regular file"}),
        )));
    }
    let mut hash = Sha256::new();
    let mut found = 0u64;
    let mut bounded = file.take(limit.saturating_add(1));
    let mut buf = [0u8; 65536];
    loop {
        match bounded.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hash.update(&buf[..n]);
                found += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e.into()),
        }
    }
    if found > limit || recorded.is_some_and(|r| found != r) {
        return Ok(Some((
            "capture_torn",
            json!({"recorded_bytes":recorded,"found_bytes":found}),
        )));
    }
    let got = format!("{:x}", hash.finalize());
    Ok((got != want).then(|| {
        (
            "capture_torn",
            json!({"recorded_bytes":recorded,"why":"content differs from the recorded digest","recorded_sha256":want,"found_sha256":got}),
        )
    }))
}
/// Durable cleanup AND current supported-scope quiescence. Any uncertainty blocks.
/// Cooperative group census + known identities only: not whole-descendant isolation.
pub(crate) fn quiescence(conn: &Connection, attempt: &str) -> Result<(), String> {
    let q = || -> Result<Result<(), String>, Error> {
        let receipt: Option<(i32, String, String)> = conn
            .query_row(
                "SELECT supervisor_pid,supervisor_start,supervisor_boot FROM supervisor_command WHERE attempt_id=?1",
                [attempt],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((pid, start, boot)) = receipt else {
            return Ok(Err(
                "no supervisor receipt: preparation never launched".into()
            ));
        };
        let mut s = conn.prepare(
            "SELECT kind,payload_json FROM supervisor_fact WHERE attempt_id=?1 ORDER BY seq",
        )?;
        let facts: Vec<(String, String)> = s
            .query_map([attempt], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if facts.last().map(|f| f.0.as_str()) != Some("cleanup_completed") {
            return Ok(Err(
                "durable cleanup completion is not the latest lifecycle fact".into(),
            ));
        }
        if facts.iter().any(|f| f.0 == "unknown") {
            return Ok(Err("lifecycle recorded unknown supervision".into()));
        }
        let mut stop_ok = false;
        let mut wrapper = None;
        let mut known = Vec::new();
        for (kind, payload) in &facts {
            let v: Value = serde_json::from_str(payload)?;
            match kind.as_str() {
                "stop_confirmed" => stop_ok |= v["census_empty"] == true,
                "starting" => match proc_identity(&v) {
                    Ok(i) if wrapper.is_none() => wrapper = Some(i),
                    Ok(_) => return Ok(Err("multiple wrapper identities recorded".into())),
                    Err(e) => return Ok(Err(e)),
                },
                "descendant_observed" | "started" | "parent_exit_observed" => {
                    match proc_identity(&v) {
                        Ok(i) => known.push(i),
                        Err(e) => return Ok(Err(e)),
                    }
                }
                _ => (),
            }
        }
        if !stop_ok {
            return Ok(Err("no durable empty stop census".into()));
        }
        let supervisor = Identity {
            pid,
            start,
            boot,
            group: 0,
        };
        if supervisor.live() {
            return Ok(Err(format!("supervisor {pid} may still be live")));
        }
        if let Some(w) = &wrapper {
            known.push(w.clone());
        }
        for i in &known {
            if i.live() {
                return Ok(Err(format!("known process {} may still be live", i.pid)));
            }
        }
        if let Some(w) = wrapper {
            if w.boot == crate::host::boot_id()? {
                match process_scope::group_members(w.group) {
                    Ok(v) if v.is_empty() => (),
                    Ok(v) => {
                        return Ok(Err(format!(
                            "supported process group {} has {} live member(s)",
                            w.group,
                            v.len()
                        )));
                    }
                    Err(e) => return Ok(Err(format!("group census unavailable: {e}"))),
                }
            }
        }
        // Outcome 2: lost, truncated or torn evidence is never completeness. Checked after
        // every M1 condition so an M1 refusal keeps its reason.
        let gap_table: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='evidence_gap'",
            [],
            |r| r.get(0),
        )?;
        if gap_table > 0 {
            let mut s = conn.prepare(
                "SELECT DISTINCT kind FROM evidence_gap WHERE attempt_id=?1 ORDER BY kind",
            )?;
            let kinds: Vec<String> = s
                .query_map([attempt], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            if !kinds.is_empty() {
                return Ok(Err(format!("evidence gap recorded: {}", kinds.join(", "))));
            }
        }
        for (kind, payload) in &facts {
            let v: Value = serde_json::from_str(payload)?;
            if (kind == "stop_requested"
                && v["reason"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("output limit exceeded")))
                || (kind == "cleanup_completed" && v["output_truncated"] == true)
            {
                return Ok(Err(
                    "evidence gap: output_truncated (capture exceeded its limit)".into(),
                ));
            }
        }
        if let Some((kind, detail)) = capture_gap(conn, attempt)? {
            return Ok(Err(format!("evidence gap: {kind} {detail}")));
        }
        Ok(Ok(()))
    };
    q().unwrap_or_else(|e| Err(format!("quiescence unknown: {e}")))
}

impl Owner {
    pub fn symbol_intent(
        &mut self,
        key: &str,
        attempt: &str,
    ) -> Result<crate::execution::Attempt, Error> {
        self.intent(key, attempt, &symbol_spec())
    }
    /// Audited refusal. Optional responsibility transition (never postponed if unchanged).
    fn refuse(
        &mut self,
        work: &str,
        attempt: &str,
        kind: &str,
        reason: String,
        block: Option<&str>,
    ) -> Error {
        let r = (|| -> Result<(), Error> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            event(&tx, work, kind, attempt, json!({"reason":reason}))?;
            if let Some(action) = block {
                set_responsibility(&tx, work, "blocked", "local-owner", action)?;
            }
            tx.commit()?;
            Ok(())
        })();
        match r {
            Ok(()) => Error::Rejected(reason),
            Err(e) => e,
        }
    }
    fn invalidate(&mut self, a: &ArtifactRow, reason: &str) -> Result<(), Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE artifact SET state='invalid',invalid_reason=?1 WHERE id=?2 AND state='valid'",
            params![reason, a.id],
        )? == 1
        {
            event(
                &tx,
                &a.work,
                "artifact.invalidated",
                &a.id,
                json!({"role":a.role,"attempt_id":a.attempt,"reason":reason}),
            )?;
            set_responsibility(
                &tx,
                &a.work,
                "blocked",
                "local-owner",
                "inspect_invalid_artifact",
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Verify a stored artifact; any missing/changed identity invalidates it durably.
    pub(crate) fn verified(&mut self, a: &ArtifactRow) -> Result<Scan, Error> {
        if a.state != "valid" {
            return Err(Error::Rejected(format!(
                "{} artifact {} is invalid: {}",
                a.role,
                a.id,
                a.invalid_reason.as_deref().unwrap_or("unknown")
            )));
        }
        match artifact::verify(
            self.root(),
            &a.id,
            &a.manifest_json,
            &a.manifest_hash,
            &a.identity_json,
        ) {
            Ok(s) => Ok(s),
            Err(reason) => {
                self.invalidate(a, &reason)?;
                Err(Error::Rejected(format!(
                    "{} artifact {} invalidated: {reason}",
                    a.role, a.id
                )))
            }
        }
    }
    /// Run on every mutating open and status: missing/changed snapshots become invalid.
    pub fn reconcile_artifacts(&mut self) -> Result<(), Error> {
        let rows: Vec<ArtifactRow> = {
            let mut q = self.conn.prepare(&format!(
                "SELECT {ARTIFACT_COLUMNS} FROM artifact WHERE state='valid' ORDER BY sealed_at,id"
            ))?;
            q.query_map([], artifact_row)?.collect::<Result<_, _>>()?
        };
        for a in rows {
            if let Err(reason) = artifact::verify(
                self.root(),
                &a.id,
                &a.manifest_json,
                &a.manifest_hash,
                &a.identity_json,
            ) {
                self.invalidate(&a, &reason)?;
            }
        }
        Ok(())
    }
    fn commit_artifact(
        tx: &Connection,
        a: &ArtifactRow,
        epoch: i64,
        now: i64,
    ) -> Result<(), Error> {
        tx.execute(
            "INSERT INTO artifact(id,work_id,attempt_id,role,layout,manifest_hash,manifest_json,identity_json,content_path,coordinator_epoch,sealed_at,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'valid')",
            params![a.id,a.work,a.attempt,a.role,artifact::LAYOUT,a.manifest_hash,a.manifest_json,a.identity_json,artifact::content_path(&a.id),epoch,now],
        )?;
        event(
            tx,
            &a.work,
            "artifact.sealed",
            &a.id,
            json!({"role":a.role,"attempt_id":a.attempt,"manifest_hash":a.manifest_hash}),
        )?;
        Ok(())
    }
    fn artifact_json(&self, a: &ArtifactRow, existing: bool) -> Result<Value, Error> {
        Ok(json!({
            "id": a.id, "attempt_id": a.attempt, "role": a.role, "state": a.state,
            "path": self.root().join(artifact::content_path(&a.id)),
            "layout": artifact::LAYOUT, "manifest_hash": a.manifest_hash,
            "manifest": serde_json::from_str::<Value>(&a.manifest_json)?,
            "existing": existing, "work_success": false,
        }))
    }
    fn new_artifact(
        &mut self,
        work: &str,
        attempt: &str,
        role: &str,
        source: &Scan,
    ) -> Result<(ArtifactRow, Scan), Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let (_, identity) = artifact::freeze(self.root(), &id, source)?;
        let row = ArtifactRow {
            id,
            work: work.into(),
            attempt: attempt.into(),
            role: role.into(),
            manifest_json: source.manifest.canonical()?,
            manifest_hash: source.manifest.hash()?,
            identity_json: serde_json::to_string(&identity)?,
            state: "valid".into(),
            invalid_reason: None,
        };
        let snap = artifact::verify(
            self.root(),
            &row.id,
            &row.manifest_json,
            &row.manifest_hash,
            &row.identity_json,
        )
        .map_err(Error::Rejected)?;
        Ok((row, snap))
    }
    fn open_fixture(&self, attempt: &str, allowed: &[&str]) -> Result<std::os::fd::OwnedFd, Error> {
        let path = fixture_path(&self.conn, attempt)?;
        let fd = artifact::open_dir(&path)?;
        // Bind the OPENED descriptor (not just the earlier path lookup) to the recorded identity.
        let (dev, ino): (i64, i64) = self.conn.query_row(
            "SELECT fixture_dev,fixture_ino FROM attempt WHERE id=?1",
            [attempt],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let m = std::fs::File::from(fd.try_clone()?).metadata()?;
        {
            use std::os::unix::fs::MetadataExt;
            if m.dev() as i64 != dev || m.ino() as i64 != ino {
                return Err(Error::Rejected("fixture directory identity changed".into()));
            }
        }
        let names = artifact::list(&fd)?;
        for n in &names {
            if !allowed.contains(&n.as_str()) {
                return Err(Error::Rejected(format!(
                    "unsupported fixture root entry {:?}",
                    n.chars().take(64).collect::<String>()
                )));
            }
            let (kind, nlink) = artifact::entry_type(&fd, n)?;
            let ok = match n.as_str() {
                "initial" | "candidate" => kind == libc::S_IFDIR,
                _ => kind == libc::S_IFREG && nlink == 1,
            };
            if !ok {
                return Err(Error::Rejected(format!(
                    "fixture root entry {n} has unsupported type"
                )));
            }
        }
        for required in ["initial", "candidate"] {
            if !names.iter().any(|n| n == required) {
                return Err(Error::Rejected(format!("fixture root missing {required}/")));
            }
        }
        Ok(fd)
    }

    /// Freeze `initial/` before any launch and record the trusted defect reproduction.
    pub fn reproduce(&mut self, attempt: &str) -> Result<Value, Error> {
        let c = ctx(&self.conn, attempt)?;
        crate::attention::progress_fence(&self.conn, &c.work)?;
        if c.spec.mode != FakeMode::PrepareSymbol {
            return Err(self.refuse(
                &c.work,
                attempt,
                "reproduce.refused",
                "attempt is not symbol preparation".into(),
                None,
            ));
        }
        if let Some(a) = artifact_for(&self.conn, attempt, "initial")? {
            self.verified(&a)?;
            let ev: Option<String> = self.conn.query_row("SELECT id FROM evidence WHERE attempt_id=?1 AND kind='initial_defect' AND authority='trusted_validator'", [attempt], |r| r.get(0)).optional()?;
            return Ok(
                json!({"artifact": self.artifact_json(&a, true)?, "evidence_id": ev, "reproduced": ev.is_some(), "work_success": false}),
            );
        }
        if c.state != "intent" || c.receipt {
            return Err(self.refuse(
                &c.work,
                attempt,
                "reproduce.refused",
                "initial reproduction must precede any launch".into(),
                Some("inspect_missing_initial_reproduction"),
            ));
        }
        let scanned = (|| -> Result<Scan, Error> {
            let fix = self.open_fixture(attempt, &["initial", "candidate"])?;
            let initial = artifact::open_subdir(&fix, "initial")?;
            let s1 = artifact::scan_dir(&initial)?;
            let cand = artifact::scan_dir(&artifact::open_subdir(&fix, "candidate")?)?;
            if cand.manifest != s1.manifest {
                return Err(Error::Rejected(
                    "candidate differs from initial before launch".into(),
                ));
            }
            Ok(s1)
        })();
        let s1 = match scanned {
            Ok(s) => s,
            Err(e) => {
                return Err(self.refuse(
                    &c.work,
                    attempt,
                    "reproduce.refused",
                    reason(e),
                    Some("inspect_initial_fixture"),
                ));
            }
        };
        let (row, snap) = self.new_artifact(&c.work, attempt, "initial", &s1)?;
        let s2 = artifact::scan_dir(&artifact::open_subdir(
            &self.open_fixture(attempt, &["initial", "candidate"])?,
            "initial",
        )?);
        if s2.map(|s| s.identity).ok() != Some(s1.identity.clone()) {
            return Err(self.refuse(
                &c.work,
                attempt,
                "reproduce.refused",
                "initial fixture changed during freeze".into(),
                Some("inspect_initial_fixture"),
            ));
        }
        // Checker consumes the exact verified snapshot bytes, then the snapshot is rescanned.
        let sym = check_symbols(&snap.bytes["symbols.csv"]);
        let (cov, gaps) = observe_capture(&snap.bytes["capture.json"]);
        if let Err(e) = artifact::verify(
            self.root(),
            &row.id,
            &row.manifest_json,
            &row.manifest_hash,
            &row.identity_json,
        ) {
            return Err(self.refuse(
                &c.work,
                attempt,
                "reproduce.refused",
                format!("initial snapshot changed during checks: {e}"),
                Some("inspect_initial_fixture"),
            ));
        }
        let reproduced = !sym.valid && sym.finding == MALFORMED;
        if !reproduced {
            return Err(self.refuse(
                &c.work,
                attempt,
                "reproduce.refused",
                format!("initial defect not reproduced: {}", sym.finding),
                Some("inspect_initial_fixture"),
            ));
        }
        let origin = TrustedOrigin { _private: () };
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        let again = ctx(&tx, attempt)?;
        if again.state != "intent" || again.receipt {
            return Err(Error::Rejected(
                "attempt launched during reproduction".into(),
            ));
        }
        Self::commit_artifact(&tx, &row, epoch, now)?;
        let id = insert_trusted(
            &tx,
            &origin,
            &c.work,
            attempt,
            &row,
            "initial_defect",
            now,
            Coverage::Complete,
            vec![],
            json!({
                "check": VALIDATOR_VERSION, "reproduced": true, "finding": sym.finding, "observed_symbol": sym.symbol,
                "manifest_hash": row.manifest_hash, "capture_coverage": cov.as_str(), "capture_gaps": gaps,
            }),
            epoch,
        )?;
        set_responsibility(
            &tx,
            &c.work,
            "preparing",
            "coordinator",
            "request_fake_launch",
        )?;
        tx.commit()?;
        Ok(
            json!({"artifact": self.artifact_json(&row, false)?, "evidence_id": id, "reproduced": true, "finding": sym.finding, "work_success": false}),
        )
    }

    /// Seal the prepared candidate only after durable cleanup and current quiescence.
    pub fn seal(&mut self, attempt: &str) -> Result<Value, Error> {
        let c = ctx(&self.conn, attempt)?;
        crate::attention::progress_fence(&self.conn, &c.work)?;
        if c.spec.mode != FakeMode::PrepareSymbol {
            return Err(self.refuse(
                &c.work,
                attempt,
                "seal.refused",
                "attempt is not symbol preparation".into(),
                None,
            ));
        }
        if let Some(a) = artifact_for(&self.conn, attempt, "candidate")? {
            self.verified(&a)?;
            return self.artifact_json(&a, true);
        }
        let Some(initial) = artifact_for(&self.conn, attempt, "initial")? else {
            return Err(self.refuse(
                &c.work,
                attempt,
                "seal.refused",
                "no frozen initial reproduction".into(),
                None,
            ));
        };
        self.verified(&initial)?;
        if c.state != "cleaned" {
            return Err(self.refuse(
                &c.work,
                attempt,
                "seal.refused",
                format!("attempt lifecycle is {}; cleanup not established", c.state),
                None,
            ));
        }
        if let Err(reason) = quiescence(&self.conn, attempt) {
            return Err(self.refuse(&c.work, attempt, "seal.refused", reason, None));
        }
        // A rejected candidate is terminal for this attempt: later edits after supervised
        // preparation must not become sealable content. Use a new work/attempt instead.
        let prior: i64 = self.conn.query_row(
            "SELECT count(*) FROM event WHERE kind='seal.rejected' AND entity_id=?1 AND work_id=?2",
            params![attempt, c.work],
            |r| r.get(0),
        )?;
        if prior > 0 {
            return Err(self.refuse(
                &c.work,
                attempt,
                "seal.refused",
                "candidate was previously rejected; rejection is terminal for this attempt".into(),
                Some("inspect_candidate_layout"),
            ));
        }
        let allowed = ["candidate", "captured.log", "initial"];
        let scanned = (|| -> Result<Scan, Error> {
            let fix = self.open_fixture(attempt, &allowed)?;
            let init = artifact::scan_dir(&artifact::open_subdir(&fix, "initial")?)?;
            if init.manifest.hash()? != initial.manifest_hash {
                return Err(Error::Rejected(
                    "preparation modified initial/ (only candidate may change)".into(),
                ));
            }
            artifact::scan_dir(&artifact::open_subdir(&fix, "candidate")?)
        })();
        let c1 = match scanned {
            Ok(s) => s,
            Err(e) => {
                return Err(self.refuse(
                    &c.work,
                    attempt,
                    "seal.rejected",
                    format!("candidate layout rejected: {}", reason(e)),
                    Some("inspect_candidate_layout"),
                ));
            }
        };
        let (row, _) = self.new_artifact(&c.work, attempt, "candidate", &c1)?;
        let c2 = self
            .open_fixture(attempt, &allowed)
            .and_then(|fix| artifact::scan_dir(&artifact::open_subdir(&fix, "candidate")?));
        if c2.map(|s| s.identity).ok() != Some(c1.identity.clone()) {
            return Err(self.refuse(
                &c.work,
                attempt,
                "seal.rejected",
                "candidate changed during sealing".into(),
                Some("inspect_candidate_layout"),
            ));
        }
        if let Err(reason) = quiescence(&self.conn, attempt) {
            return Err(self.refuse(&c.work, attempt, "seal.refused", reason, None));
        }
        barrier("before_seal_commit")?;
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::commit_artifact(&tx, &row, epoch, now_ms()?)?;
        set_responsibility(&tx, &c.work, "blocked", "local-owner", "validate_candidate")?;
        tx.commit()?;
        barrier("after_seal_commit")?;
        self.artifact_json(&row, false)
    }

    /// Trusted checks on the frozen candidate, verified before and after the checks.
    pub fn validate(&mut self, artifact_id: &str) -> Result<Value, Error> {
        if !valid_id(artifact_id) {
            return Err(Error::Invalid("invalid artifact ID"));
        }
        let a = self
            .conn
            .query_row(
                &format!("SELECT {ARTIFACT_COLUMNS} FROM artifact WHERE id=?1"),
                [artifact_id],
                artifact_row,
            )
            .optional()?
            .ok_or(Error::Invalid("unknown artifact"))?;
        crate::attention::progress_fence(&self.conn, &a.work)?;
        if a.role != "candidate" {
            return Err(self.refuse(
                &a.work,
                &a.attempt,
                "validate.refused",
                "only a sealed candidate can be validated".into(),
                None,
            ));
        }
        let snap = self.verified(&a)?;
        let initial = artifact_for(&self.conn, &a.attempt, "initial")?
            .ok_or(Error::Invalid("missing initial artifact"))?;
        self.verified(&initial)?;
        let prior: Option<String> = self.conn.query_row("SELECT payload_json FROM evidence WHERE attempt_id=?1 AND kind='candidate_validation' AND authority='trusted_validator' ORDER BY source_seq DESC LIMIT 1", [&a.attempt], |r| r.get(0)).optional()?;
        if let Some(p) = prior {
            let mut v: Value = serde_json::from_str(&p)?;
            v["existing"] = json!(true);
            return Ok(v);
        }
        if let Err(reason) = quiescence(&self.conn, &a.attempt) {
            return Err(self.refuse(&a.work, &a.attempt, "validate.refused", reason, None));
        }
        let initial_manifest = artifact::Manifest::parse(&initial.manifest_json)?;
        let sym = check_symbols(&snap.bytes["symbols.csv"]);
        let (cov, gaps) = observe_capture(&snap.bytes["capture.json"]);
        let capture_unchanged =
            snap.manifest.entry("capture.json") == initial_manifest.entry("capture.json");
        barrier("after_validation_checks")?;
        // Rescan the complete frozen snapshot; a change discards these results.
        let after = self.verified(&a)?;
        if after.identity != snap.identity || after.bytes != snap.bytes {
            return Err(Error::Rejected("snapshot changed during validation".into()));
        }
        let passed = sym.valid && capture_unchanged;
        let result = json!({
            "artifact_id": a.id, "attempt_id": a.attempt, "manifest_hash": a.manifest_hash,
            "initial_artifact_id": initial.id, "validator": VALIDATOR_VERSION,
            "symbol_validated": sym.valid, "finding": sym.finding, "observed_symbol": sym.symbol,
            "capture_unchanged": capture_unchanged, "capture_restored": false,
            "capture_coverage": cov.as_str(), "capture_gaps": gaps,
            "validation_passed": passed, "work_success": false, "existing": false,
        });
        let origin = TrustedOrigin { _private: () };
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        insert_trusted(
            &tx,
            &origin,
            &a.work,
            &a.attempt,
            &a,
            "candidate_validation",
            now,
            Coverage::Complete,
            vec![],
            result.clone(),
            epoch,
        )?;
        insert_trusted(
            &tx,
            &origin,
            &a.work,
            &a.attempt,
            &a,
            "capture_coverage",
            now,
            cov,
            gaps.clone(),
            json!({"capture_restored": false, "capture_unchanged": capture_unchanged, "manifest_hash": a.manifest_hash, "reason": "symbol repair cannot establish restoration of incomplete capture intervals"}),
            epoch,
        )?;
        set_responsibility(
            &tx,
            &a.work,
            "blocked",
            "local-owner",
            if passed {
                "create_exact_review"
            } else {
                "inspect_failed_candidate_validation"
            },
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Readiness for a FUTURE review boundary only. Never successful work or closure.
    pub fn task3_status(&mut self, attempt: &str) -> Result<Value, Error> {
        self.reconcile_artifacts()?;
        let c = ctx(&self.conn, attempt)?;
        let now = now_ms()?;
        let mut blockers: Vec<String> = Vec::new();
        if c.spec.mode != FakeMode::PrepareSymbol {
            blockers.push(
                "attempt is not symbol preparation; exit status and claims are not evidence".into(),
            );
        }
        if let Err(r) = quiescence(&self.conn, attempt) {
            blockers.push(format!("writer quiescence not established: {r}"));
        }
        let mut arts = serde_json::Map::new();
        for role in ["initial", "candidate"] {
            match artifact_for(&self.conn, attempt, role)? {
                None => blockers.push(format!("no {role} artifact")),
                Some(a) => {
                    if a.state != "valid" {
                        blockers.push(format!(
                            "{role} artifact invalid: {}",
                            a.invalid_reason.clone().unwrap_or_default()
                        ));
                    }
                    arts.insert(role.into(), json!({"id":a.id,"state":a.state,"manifest_hash":a.manifest_hash,"invalid_reason":a.invalid_reason}));
                }
            }
        }
        let rows = evidence::rows(
            &self.conn,
            attempt,
            crate::store::freshness_clock(&self.conn)?,
        )?;
        let required = |kind: &str, blockers: &mut Vec<String>| -> Option<evidence::EvidenceRow> {
            // Latest trusted row of this kind (bounded revalidation appends newer rows).
            let Some(r) = rows
                .iter()
                .rev()
                .find(|r| r.kind == kind && r.authority == "trusted_validator")
            else {
                blockers.push(format!("missing trusted {kind} evidence"));
                return None;
            };
            let bound = arts
                .get(if kind == "initial_defect" {
                    "initial"
                } else {
                    "candidate"
                })
                .is_some_and(|a| {
                    a["id"].as_str() == r.artifact_id.as_deref()
                        && a["manifest_hash"] == r.payload["manifest_hash"]
                });
            for (bad, why) in [
                (
                    !r.trusted,
                    "not trusted (artifact invalid, envelope altered or conflicted)",
                ),
                (!r.fresh, "stale or future-dated"),
                (!r.sequence_complete, "validator sequence gap"),
                (!bound, "not bound to the current artifact manifest"),
            ] {
                if bad {
                    blockers.push(format!("{kind} evidence {why}"));
                }
            }
            Some(r.clone())
        };
        let initial = required("initial_defect", &mut blockers);
        let candidate = required("candidate_validation", &mut blockers);
        let capture = required("capture_coverage", &mut blockers);
        if let Some(r) = &initial {
            if r.payload["reproduced"] != true || r.coverage != "complete" {
                blockers.push("initial defect not reproduced with complete coverage".into());
            }
        }
        if let Some(r) = &candidate {
            if r.payload["validation_passed"] != true
                || r.payload["symbol_validated"] != true
                || r.coverage != "complete"
            {
                blockers.push(format!(
                    "candidate validation did not pass: {}",
                    r.payload["finding"]
                ));
            }
            if r.payload["capture_unchanged"] != true {
                blockers.push("candidate changed capture data".into());
            }
        }
        let conflicts: i64 = self.conn.query_row(
            "SELECT count(*) FROM event WHERE kind='evidence.conflict' AND entity_id IN (SELECT id FROM evidence WHERE attempt_id=?1)",
            [attempt],
            |r| r.get(0),
        )?;
        if conflicts > 0 {
            blockers.push(format!("{conflicts} evidence conflict(s) audited for this attempt; owner inspection required"));
        }
        let (mode, phase, owner, action, due): (String, String, String, String, i64) =
            self.conn.query_row(
                "SELECT mode,phase,next_owner,next_action,due_at FROM work WHERE id=?1",
                [&c.work],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        Ok(json!({
            "attempt_id": attempt, "ready": blockers.is_empty(), "blockers": blockers,
            "meaning": "ready = reproduced initial defect + trusted validated frozen candidate for a future review boundary; not successful work",
            "work_closed": mode == "closed", "work_success": false, "review_available": blockers.is_empty(),
            "capture_restored": false,
            "capture_gaps": capture.map(|r| r.gaps).unwrap_or_default(),
            "artifacts": arts, "lifecycle": c.state,
            "work": {"mode":mode,"phase":phase,"next_owner":owner,"next_action":action,"due_at":due,"overdue":mode == "active" && due < now},
            "note": "readiness is a precondition for review, not success; see `inbox` and `closure-summary` for attention and closure",
        }))
    }
    /// Bounded trusted revalidation (Task 4). Reruns the trusted checks on the SAME
    /// verified frozen artifacts and appends new evidence; it never extends old
    /// timestamps. Allowed only after the bound evidence went stale, with no open valid
    /// review and no unresolved effect, at most `MAX_REVALIDATIONS` times per attempt.
    /// Any failure blocks with a durable responsibility and explicit reason. A new
    /// review and a new decision are always required afterwards.
    pub fn revalidate(&mut self, artifact_id: &str) -> Result<Value, Error> {
        if !valid_id(artifact_id) {
            return Err(Error::Invalid("invalid artifact ID"));
        }
        self.sweep_reviews()?;
        let a = self
            .conn
            .query_row(
                &format!("SELECT {ARTIFACT_COLUMNS} FROM artifact WHERE id=?1"),
                [artifact_id],
                artifact_row,
            )
            .optional()?
            .ok_or(Error::Invalid("unknown artifact"))?;
        crate::attention::progress_fence(&self.conn, &a.work)?;
        let block = |o: &mut Owner, why: String, action: &str| -> Error {
            o.refuse(
                &a.work,
                &a.attempt,
                "revalidation.refused",
                why,
                Some(action),
            )
        };
        if a.role != "candidate" {
            return Err(Error::Rejected(
                "only a sealed candidate can be revalidated".into(),
            ));
        }
        let open: i64 = self.conn.query_row(
            "SELECT count(*) FROM review WHERE work_id=?1 AND state IN ('pending','authorized')",
            [&a.work],
            |r| r.get(0),
        )?;
        if open > 0 {
            return Err(Error::Rejected(
                "an open review is still valid; revalidation is only for expired evidence".into(),
            ));
        }
        let unresolved: i64 = self.conn.query_row(
            "SELECT count(*) FROM effect WHERE work_id=?1 AND state NOT IN ('not_applied','cancelled')",
            [&a.work],
            |r| r.get(0),
        )?;
        if unresolved > 0 {
            return Err(block(
                self,
                "an effect for this work is applied, conflicting or unresolved; no revalidation"
                    .into(),
                "inspect_effect_before_revalidation",
            ));
        }
        let rows = evidence::rows(
            &self.conn,
            &a.attempt,
            crate::store::freshness_clock(&self.conn)?,
        )?;
        let latest = |kind: &str| {
            rows.iter()
                .rev()
                .find(|r| r.kind == kind && r.authority == "trusted_validator")
                .cloned()
        };
        let (Some(init_ev), Some(cand_ev)) =
            (latest("initial_defect"), latest("candidate_validation"))
        else {
            return Err(Error::Rejected(
                "no prior trusted validation to revalidate; use validate".into(),
            ));
        };
        if init_ev.fresh && cand_ev.fresh {
            return Err(Error::Rejected(
                "trusted evidence is still fresh; no revalidation needed".into(),
            ));
        }
        let done: i64 = self.conn.query_row(
            "SELECT count(*) FROM event WHERE kind='evidence.revalidated' AND entity_id=?1",
            [&a.attempt],
            |r| r.get(0),
        )?;
        if done >= MAX_REVALIDATIONS {
            return Err(block(
                self,
                format!("revalidation budget ({MAX_REVALIDATIONS}) exhausted for this attempt"),
                "revalidation_budget_exhausted_owner_decision",
            ));
        }
        if let Err(q) = quiescence(&self.conn, &a.attempt) {
            return Err(block(
                self,
                format!("writer quiescence not established: {q}"),
                "inspect_writer_quiescence",
            ));
        }
        let initial = artifact_for(&self.conn, &a.attempt, "initial")?
            .ok_or(Error::Invalid("missing initial artifact"))?;
        let isnap = match self.verified(&initial) {
            Ok(s) => s,
            Err(e) => {
                return Err(block(
                    self,
                    format!("revalidation impossible: {e}"),
                    "inspect_invalid_artifact",
                ));
            }
        };
        let csnap = match self.verified(&a) {
            Ok(s) => s,
            Err(e) => {
                return Err(block(
                    self,
                    format!("revalidation impossible: {e}"),
                    "inspect_invalid_artifact",
                ));
            }
        };
        let isym = check_symbols(&isnap.bytes["symbols.csv"]);
        let sym = check_symbols(&csnap.bytes["symbols.csv"]);
        let (cov, gaps) = observe_capture(&csnap.bytes["capture.json"]);
        let capture_unchanged =
            csnap.manifest.entry("capture.json") == isnap.manifest.entry("capture.json");
        barrier("after_revalidation_checks")?;
        let i2 = self.verified(&initial);
        let c2 = self.verified(&a);
        let same = matches!((&i2, &c2), (Ok(i), Ok(c)) if i.identity == isnap.identity && i.bytes == isnap.bytes && c.identity == csnap.identity && c.bytes == csnap.bytes);
        if !same {
            return Err(block(
                self,
                "frozen snapshot changed during revalidation".into(),
                "inspect_invalid_artifact",
            ));
        }
        let reproduced = !isym.valid && isym.finding == MALFORMED;
        let passed = sym.valid && capture_unchanged;
        if !reproduced || !passed {
            return Err(block(
                self,
                format!(
                    "revalidation did not reproduce the same trusted results (initial: {}, candidate: {})",
                    isym.finding, sym.finding
                ),
                "inspect_failed_revalidation",
            ));
        }
        let origin = TrustedOrigin { _private: () };
        let epoch = self.epoch;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        let n = done + 1;
        let iid = insert_trusted(
            &tx,
            &origin,
            &a.work,
            &a.attempt,
            &initial,
            "initial_defect",
            now,
            Coverage::Complete,
            vec![],
            json!({
                "check": VALIDATOR_VERSION, "reproduced": true, "finding": isym.finding, "observed_symbol": isym.symbol,
                "manifest_hash": initial.manifest_hash, "capture_coverage": cov.as_str(), "capture_gaps": gaps,
                "revalidation": n, "supersedes": init_ev.id,
            }),
            epoch,
        )?;
        let result = json!({
            "artifact_id": a.id, "attempt_id": a.attempt, "manifest_hash": a.manifest_hash,
            "initial_artifact_id": initial.id, "validator": VALIDATOR_VERSION,
            "symbol_validated": sym.valid, "finding": sym.finding, "observed_symbol": sym.symbol,
            "capture_unchanged": capture_unchanged, "capture_restored": false,
            "capture_coverage": cov.as_str(), "capture_gaps": gaps,
            "validation_passed": passed, "work_success": false, "existing": false,
            "revalidation": n, "supersedes": cand_ev.id,
        });
        let cid = insert_trusted(
            &tx,
            &origin,
            &a.work,
            &a.attempt,
            &a,
            "candidate_validation",
            now,
            Coverage::Complete,
            vec![],
            result.clone(),
            epoch,
        )?;
        let gid = insert_trusted(
            &tx,
            &origin,
            &a.work,
            &a.attempt,
            &a,
            "capture_coverage",
            now,
            cov,
            gaps.clone(),
            json!({
                "capture_restored": false, "capture_unchanged": capture_unchanged, "manifest_hash": a.manifest_hash,
                "reason": "symbol repair cannot establish restoration of incomplete capture intervals", "revalidation": n,
            }),
            epoch,
        )?;
        event(
            &tx,
            &a.work,
            "evidence.revalidated",
            &a.attempt,
            json!({"revalidation": n, "new_evidence": [iid, cid, gid],
            "superseded": [init_ev.id, cand_ev.id], "rule": "same verified frozen artifacts; timestamps never extended"}),
        )?;
        set_responsibility(
            &tx,
            &a.work,
            "blocked",
            "local-owner",
            "create_exact_review",
        )?;
        tx.commit()?;
        Ok(
            json!({"revalidation": n, "evidence": [iid, cid, gid], "result": result,
            "new_review_required": true, "work_success": false}),
        )
    }
    pub fn evidence_status(&mut self, attempt: &str) -> Result<Vec<evidence::EvidenceRow>, Error> {
        ctx(&self.conn, attempt)?;
        evidence::rows(
            &self.conn,
            attempt,
            crate::store::freshness_clock(&self.conn)?,
        )
    }
    /// Launch guard: a symbol preparation never starts before its frozen reproduction.
    pub(crate) fn require_reproduction(&mut self, attempt: &str) -> Result<(), Error> {
        let c = ctx(&self.conn, attempt)?;
        if c.spec.mode != FakeMode::PrepareSymbol || c.state != "intent" {
            return Ok(());
        }
        let Some(a) = artifact_for(&self.conn, attempt, "initial")? else {
            return Err(Error::Rejected(
                "reproduce the initial defect before launching preparation".into(),
            ));
        };
        self.verified(&a)?;
        let n: i64 = self.conn.query_row("SELECT count(*) FROM evidence WHERE attempt_id=?1 AND kind='initial_defect' AND authority='trusted_validator'", [attempt], |r| r.get(0))?;
        if n != 1 {
            return Err(Error::Rejected(
                "trusted initial reproduction missing".into(),
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_trusted(
    tx: &Connection,
    _origin: &TrustedOrigin,
    work: &str,
    attempt: &str,
    a: &ArtifactRow,
    kind: &'static str,
    now: i64,
    coverage: Coverage,
    gaps: Vec<String>,
    payload: Value,
    epoch: i64,
) -> Result<String, Error> {
    if a.work != work || a.attempt != attempt {
        return Err(Error::Invalid("trusted evidence association mismatch"));
    }
    let source = format!("validator:{attempt}");
    let seq = evidence::next_trusted_seq(tx, &source)?;
    evidence::insert(
        tx,
        Some(_origin),
        &NewEvidence {
            authority: "trusted_validator",
            source,
            source_epoch: VALIDATOR_EPOCH.into(),
            source_seq: seq,
            kind,
            work_id: work,
            attempt_id: attempt,
            artifact_id: Some(&a.id),
            observed_at: now,
            coverage,
            gaps,
            payload,
        },
        now,
        epoch,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symbol_checker_is_strict() {
        assert_eq!(check_symbols(INITIAL_SYMBOLS).finding, MALFORMED);
        assert!(check_symbols(PREPARED_SYMBOLS).valid);
        for bad in [
            &b"symbol,scope\nFIXTURE_OK,core"[..],
            b"symbol,scope\nFIXTURE_OK,core\nFIXTURE_OK,core\n",
            b"sym,scope\nFIXTURE_OK,core\n",
            b"symbol,scope\nFIXTURE_OK,misc\n",
            b"symbol,scope\nOTHER,core\n",
            b"symbol,scope\nFIXTURE_OK,core,extra\n",
            b"symbol,scope\n\xff,core\n",
        ] {
            assert!(!check_symbols(bad).valid);
        }
    }
    #[test]
    fn capture_is_never_restored_by_symbol_repair() {
        let (cov, gaps) = observe_capture(CAPTURE);
        assert_eq!(cov, Coverage::Gap);
        assert_eq!(gaps.len(), 1);
        let forged =
            br#"{"version":1,"interval":{"from":"a","to":"b"},"complete":true,"missing":[]}"#;
        assert_eq!(observe_capture(forged).0, Coverage::Unknown);
        assert_eq!(observe_capture(b"{}").0, Coverage::Unknown);
    }
}

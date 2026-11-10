//! Task 4: canonical exact-review envelope and authenticated owner decisions.
//!
//! Task 3 readiness is NOT authority. A review binds the exact frozen artifacts, trusted
//! evidence IDs/hashes, validator/policy versions, the preparation permit (which never
//! grants the final action), repository identity, expected old ref, proposed commit and
//! the one typed action `fixture.ref_compare_swap`. Only an authenticated owner decision
//! against that exact envelope authorizes the action. There is no shell action router.
use crate::{
    Error, Owner,
    artifact::Scan,
    evidence::{self, NoDuplicates, TRUSTED_FRESH_MS, VALIDATOR_EPOCH},
    gitfixture::{self, Repo},
    protocol::{PermitScope, fingerprint, valid_id},
    store::{barrier, now_ms},
    validation::{
        ARTIFACT_COLUMNS, ArtifactRow, VALIDATOR_VERSION, artifact_for, artifact_row, ctx, event,
        quiescence, set_responsibility,
    },
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const ACTION_KIND: &str = "fixture.ref_compare_swap";
pub const REVIEW_POLICY: &str = "asher-exact-review-v1";
pub const SCOPE_STATEMENT: &str = "update this disposable fixture ref only";
pub const APPROVER_ROLE: &str = "local-owner";
/// Upper bound on review lifetime; evidence freshness and permit expiry bound it further.
pub const REVIEW_MAX_MS: i64 = 86_400_000;
/// A review shorter than this cannot be decided and dispatched reliably; revalidate first.
pub const MIN_REVIEW_MS: i64 = 5_000;
pub const MAX_DECISION_BYTES: usize = 16384;
const DECISION_DOMAIN: &str = "asher-decision-v1\n";
pub(crate) const RESPONSE_MS: i64 = 10_000;
pub const CONTAINMENT_LIMIT: &str = "cooperative process-group containment of a same-user fake only; snapshot hashes detect change and are not isolation";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    pub id: String,
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub nonce: String,
}
/// The single supported typed action. Unknown kinds/fields are refused when parsed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub kind: String,
    pub version: u32,
    pub repository: RepoRef,
    pub ref_name: String,
    pub expected_old: String,
    pub new_commit: String,
    pub new_tree: String,
    pub commit_secs: i64,
    pub commit_message: String,
    pub scope: String,
}
impl Action {
    pub fn validate(&self) -> Result<(), Error> {
        if self.kind != ACTION_KIND
            || self.version != 1
            || self.ref_name != gitfixture::REF_NAME
            || self.scope != SCOPE_STATEMENT
            || !gitfixture::is_oid(&self.expected_old)
            || !gitfixture::is_oid(&self.new_commit)
            || !gitfixture::is_oid(&self.new_tree)
            || self.expected_old == self.new_commit
        {
            return Err(Error::Invalid(
                "unsupported action; only fixture.ref_compare_swap",
            ));
        }
        Ok(())
    }
    pub fn repo(&self) -> Repo {
        Repo {
            path: PathBuf::from(&self.repository.path),
            dev: self.repository.dev,
            ino: self.repository.ino,
            nonce: self.repository.nonce.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermitRef {
    pub id: String,
    pub version: i64,
    pub scope_hash: String,
    pub fixture_id: String,
    pub expires_at: i64,
    pub grants_final_action: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub review_policy: String,
    pub validator: String,
    pub validator_epoch: String,
    pub evidence_fresh_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtRef {
    pub id: String,
    pub manifest_hash: String,
    pub manifest: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvRef {
    pub id: String,
    pub kind: String,
    pub envelope_hash: String,
    pub payload_hash: String,
    pub observed_at: i64,
    pub fresh_until: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub restored: bool,
    pub gaps: Vec<String>,
}
/// Canonical versioned envelope. Field order is the serialization order; the hash is
/// SHA-256 of exactly this JSON encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub review_id: String,
    pub work_id: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub permit: PermitRef,
    pub policy: Policy,
    pub initial_artifact: ArtRef,
    pub candidate_artifact: ArtRef,
    pub evidence: Vec<EvRef>,
    pub capture: Capture,
    pub containment: String,
    pub action: Action,
    pub approver_role: String,
    pub created_at: i64,
    pub expires_at: i64,
}
impl Envelope {
    pub fn canonical(&self) -> Result<String, Error> {
        Ok(serde_json::to_string(self)?)
    }
    pub fn parse(json: &str, hash: &str) -> Result<Self, String> {
        let e: Envelope =
            serde_json::from_str(json).map_err(|_| "envelope unreadable".to_string())?;
        let again = e.canonical().map_err(|e| e.to_string())?;
        if again != json || fingerprint(json) != hash {
            return Err("stored envelope is not canonical or its hash differs".into());
        }
        e.action.validate().map_err(|e| e.to_string())?;
        Ok(e)
    }
}

// ---------------- owner authentication ----------------
pub(crate) fn hmac(key: &[u8], msg: &[u8]) -> String {
    let mut k = [0u8; 64];
    k[..key.len().min(64)].copy_from_slice(&key[..key.len().min(64)]);
    let (mut ipad, mut opad) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(msg)
        .finalize();
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(opad)
            .chain_update(inner)
            .finalize()
    )
}
pub(crate) fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}
pub(crate) const OWNER_KEY: &str = "owner.key";
const KEY_LEN: u64 = 32;

/// Private local owner credential (0600, 32 random bytes). It authenticates the owner
/// console against agents and forged roles that cannot read the state root. It does NOT
/// defend against a compromised same-user account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Valid,
    Missing,
    Invalid(&'static str),
}
/// Inspect the credential without ever modifying it.
pub fn key_state(root: &Path) -> Result<KeyState, Error> {
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(OWNER_KEY))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(KeyState::Missing),
        Err(_) => {
            return Ok(KeyState::Invalid(
                "credential is not a readable regular file",
            ));
        }
    };
    let m = f.metadata()?;
    if !m.is_file() || m.nlink() != 1 {
        return Ok(KeyState::Invalid(
            "credential is not a single-link regular file",
        ));
    }
    if m.mode() & 0o077 != 0 {
        return Ok(KeyState::Invalid(
            "credential is not private (mode must be 0600)",
        ));
    }
    if m.len() != KEY_LEN {
        return Ok(KeyState::Invalid(
            "credential is not exactly 32 bytes (partial or corrupt)",
        ));
    }
    Ok(KeyState::Valid)
}
fn path_c(p: &Path) -> Result<std::ffi::CString, Error> {
    std::ffi::CString::new(p.as_os_str().as_encoded_bytes())
        .map_err(|_| Error::Invalid("NUL in path"))
}
/// Create the credential atomically: write a complete private temporary file, fsync it,
/// then `renameat2(RENAME_NOREPLACE)` it into place and fsync the directory. An interrupted
/// creation therefore leaves NO `owner.key` (at worst an unused temp), never a partial one,
/// and an existing credential is never overwritten.
fn create_key_atomic(root: &Path) -> Result<(), Error> {
    let mut key = [0u8; KEY_LEN as usize];
    fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    let tmp = root.join(format!("{OWNER_KEY}.tmp-{}", uuid::Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)?;
    f.write_all(&key)?;
    f.sync_all()?;
    drop(f);
    barrier("before_owner_key_rename")?;
    let (from, to) = (path_c(&tmp)?, path_c(&root.join(OWNER_KEY))?);
    // SAFETY: valid NUL-terminated absolute paths; NOREPLACE never overwrites.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
fn read_owner_key(root: &Path) -> Result<Vec<u8>, Error> {
    match key_state(root)? {
        KeyState::Valid => (),
        KeyState::Missing => {
            return Err(Error::Invalid(
                "owner credential missing; authorization is unavailable (recovery operations still work); see owner-key-status and owner-key-reset --replace-invalid",
            ));
        }
        KeyState::Invalid(_) => {
            return Err(Error::Invalid(
                "owner credential invalid; authorization is unavailable and the file is left untouched; see owner-key-status and owner-key-reset --replace-invalid",
            ));
        }
    }
    let mut k = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(OWNER_KEY))?
        .take(KEY_LEN + 1)
        .read_to_end(&mut k)?;
    Ok(k)
}

/// Fields the owner signs. Serialization order is canonical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionCore {
    pub protocol_version: u32,
    pub decision_id: String,
    pub review_id: String,
    pub envelope_hash: String,
    pub expected_work_version: i64,
    pub choice: String,
    pub actor: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub protocol_version: u32,
    pub decision_id: String,
    pub review_id: String,
    pub envelope_hash: String,
    pub expected_work_version: i64,
    pub choice: String,
    pub actor: String,
    pub owner_proof: String,
}
impl DecisionRequest {
    fn core(&self) -> DecisionCore {
        DecisionCore {
            protocol_version: self.protocol_version,
            decision_id: self.decision_id.clone(),
            review_id: self.review_id.clone(),
            envelope_hash: self.envelope_hash.clone(),
            expected_work_version: self.expected_work_version,
            choice: self.choice.clone(),
            actor: self.actor.clone(),
        }
    }
}
fn proof(key: &[u8], core: &DecisionCore) -> Result<String, Error> {
    let body = format!("{DECISION_DOMAIN}{}", serde_json::to_string(core)?);
    Ok(hmac(key, body.as_bytes()))
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewRow {
    pub id: String,
    pub work: String,
    pub attempt: String,
    pub candidate: String,
    pub repo_id: String,
    pub work_version: i64,
    pub envelope_hash: String,
    pub envelope_json: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub expires_at: i64,
}
pub(crate) const REVIEW_COLUMNS: &str = "id,work_id,attempt_id,candidate_artifact_id,repo_id,work_version,envelope_hash,envelope_json,state,state_reason,expires_at";
pub(crate) fn review_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewRow> {
    Ok(ReviewRow {
        id: r.get(0)?,
        work: r.get(1)?,
        attempt: r.get(2)?,
        candidate: r.get(3)?,
        repo_id: r.get(4)?,
        work_version: r.get(5)?,
        envelope_hash: r.get(6)?,
        envelope_json: r.get(7)?,
        state: r.get(8)?,
        state_reason: r.get(9)?,
        expires_at: r.get(10)?,
    })
}
pub(crate) fn load_review(conn: &Connection, id: &str) -> Result<Option<ReviewRow>, Error> {
    Ok(conn
        .query_row(
            &format!("SELECT {REVIEW_COLUMNS} FROM review WHERE id=?1"),
            [id],
            review_row,
        )
        .optional()?)
}
pub(crate) fn repo_for(conn: &Connection, id: &str) -> Result<(Repo, String), Error> {
    let (path, dev, ino, nonce, base): (String, i64, i64, String, String) = conn.query_row(
        "SELECT path,dev,ino,nonce,base_commit FROM fixture_repo WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    Ok((
        Repo {
            path: PathBuf::from(path),
            dev: dev as u64,
            ino: ino as u64,
            nonce,
        },
        base,
    ))
}
/// Why a review can no longer authorize anything. `state` is the review's new state.
#[derive(Debug, Clone)]
pub(crate) struct Stale {
    pub state: &'static str,
    pub reason: String,
}
fn stale(state: &'static str, reason: impl Into<String>) -> Stale {
    Stale {
        state,
        reason: reason.into(),
    }
}
pub(crate) struct PermitState {
    pub version: i64,
    pub scope_hash: String,
    pub scope: PermitScope,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub issued_at: i64,
}
pub(crate) fn permit_state(conn: &Connection, id: &str) -> Result<PermitState, Error> {
    let (version, scope_hash, scope, expires_at, revoked_at, issued_at): (
        i64,
        String,
        String,
        i64,
        Option<i64>,
        i64,
    ) = conn.query_row(
        "SELECT version,scope_hash,scope_json,expires_at,revoked_at,issued_at FROM permit WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    Ok(PermitState {
        version,
        scope_hash,
        scope: serde_json::from_str(&scope)?,
        expires_at,
        revoked_at,
        issued_at,
    })
}
/// Authorization-only recheck (DB state). Used inside decision and claim transactions.
/// `now` comes from `store::authority_time`; an uncertain time is handled as expired.
pub(crate) fn authority_current(
    conn: &Connection,
    env: &Envelope,
    now: &crate::store::AuthorityTime,
) -> Result<(), Stale> {
    let uncertain = match now {
        crate::store::AuthorityTime::Uncertain { why, .. } => format!("; time uncertain: {why}"),
        crate::store::AuthorityTime::Known(_) => String::new(),
    };
    let PermitState {
        version,
        scope_hash: hash,
        scope,
        expires_at: expires,
        revoked_at: revoked,
        issued_at: issued,
    } = permit_state(conn, &env.permit.id)
        .map_err(|e| stale("invalidated", format!("permit unreadable: {e}")))?;
    if revoked.is_some() {
        return Err(stale(
            "revoked",
            "preparation permit (parent scope) was revoked",
        ));
    }
    if now.reached(expires) || now.before(issued) {
        return Err(stale(
            "expired",
            format!("preparation permit (parent scope) expired{uncertain}"),
        ));
    }
    if version != env.permit.version
        || hash != env.permit.scope_hash
        || scope.fixture_id != env.permit.fixture_id
        || scope.validate().is_err()
    {
        return Err(stale("invalidated", "permit scope changed after review"));
    }
    if now.reached(env.expires_at) {
        return Err(stale(
            "expired",
            format!("review expired; fresh evidence and a new review are required{uncertain}"),
        ));
    }
    let now = now.stamp();
    let rows = evidence::rows(conn, &env.attempt_id, now)
        .map_err(|e| stale("invalidated", e.to_string()))?;
    for r in &env.evidence {
        match rows.iter().find(|x| x.id == r.id) {
            None => {
                return Err(stale(
                    "invalidated",
                    format!("bound evidence {} missing", r.id),
                ));
            }
            Some(x) if !x.trusted || !x.intact || x.conflicts > 0 => {
                return Err(stale(
                    "invalidated",
                    format!("bound evidence {} no longer trusted", r.id),
                ));
            }
            Some(x) if !x.fresh => {
                return Err(stale(
                    "expired",
                    format!(
                        "bound trusted evidence {} is stale (fresh until {}); revalidate and create a new review",
                        r.id, x.fresh_until
                    ),
                ));
            }
            Some(_) => (),
        }
    }
    Ok(())
}
/// Recompute the commit objects implied by the verified candidate bytes.
pub(crate) fn candidate_objects(
    scan: &Scan,
    action: &Action,
) -> (
    gitfixture::Object,
    Vec<gitfixture::Object>,
    gitfixture::Object,
) {
    let (tree, blobs) = gitfixture::tree(&scan.bytes);
    let commit = gitfixture::commit(
        &tree.id,
        Some(&action.expected_old),
        action.commit_secs,
        &action.commit_message,
    );
    (tree, blobs, commit)
}

pub(crate) fn decision_receipt(
    conn: &Connection,
    id: &str,
) -> Result<Option<(String, Value)>, Error> {
    let r: Option<(String, String)> = conn
        .query_row(
            "SELECT request_hash,receipt_json FROM decision WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match r {
        Some((h, j)) => Some((h, serde_json::from_str(&j)?)),
        None => None,
    })
}
fn set_due(
    tx: &Connection,
    work: &str,
    phase: &str,
    owner: &str,
    action: &str,
    due: i64,
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
            params![phase, owner, action, due, work],
        )?;
        crate::attention::on_transition(tx, work)?;
    }
    Ok(())
}
pub(crate) fn blocked_action(state: &str) -> &'static str {
    match state {
        "revoked" => "permit_revoked_new_authority_and_review_required",
        "expired" => "review_or_evidence_expired_revalidate_then_new_review",
        _ => "review_invalidated_new_review_required",
    }
}

/// `retire` inside a caller's transaction, so a larger atomic change (for example an owner
/// cancellation) can include it. Returns whether a genuine transition happened.
pub(crate) fn retire_in(tx: &Connection, review: &str, s: &Stale) -> Result<bool, Error> {
    let Some(r) = load_review(tx, review)? else {
        return Ok(false);
    };
    if !matches!(r.state.as_str(), "pending" | "authorized") {
        return Ok(false);
    }
    tx.execute(
        "UPDATE review SET state=?1,state_reason=?2 WHERE id=?3",
        params![s.state, s.reason, review],
    )?;
    let cancelled = tx.execute(
        "UPDATE effect SET state='cancelled',state_reason=?1 WHERE review_id=?2 AND state='intent'",
        params![format!("never dispatched: {}", s.reason), review],
    )?;
    event(
        tx,
        &r.work,
        &format!("review.{}", s.state),
        review,
        json!({"reason": s.reason, "effect_intent_cancelled": cancelled == 1}),
    )?;
    set_responsibility(
        tx,
        &r.work,
        "blocked",
        "local-owner",
        blocked_action(s.state),
    )?;
    Ok(true)
}

impl Owner {
    pub(crate) fn owner_key(&self) -> Result<Vec<u8>, Error> {
        read_owner_key(self.root())
    }
    fn decisions_recorded(&self) -> Result<i64, Error> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM decision", [], |r| r.get(0))?)
    }
    /// First-use bootstrap only: creates the credential when it is absent AND no decision
    /// has ever relied on one. A present (even invalid) credential is never touched, and a
    /// missing one after decisions exist is never silently regenerated.
    pub(crate) fn bootstrap_owner_key(&mut self) -> Result<(), Error> {
        if key_state(self.root())? == KeyState::Missing && self.decisions_recorded()? == 0 {
            create_key_atomic(self.root())?;
            self.audit(
                None,
                "owner_key.created",
                "owner.key",
                json!({"reason": "first-use bootstrap; no decisions recorded"}),
            )?;
        }
        Ok(())
    }
    /// Credential state without exposing any key material. Needs no signing authority.
    pub fn owner_key_status(&self) -> Result<Value, Error> {
        let st = key_state(self.root())?;
        let (state, why) = match &st {
            KeyState::Valid => ("valid", None),
            KeyState::Missing => ("missing", None),
            KeyState::Invalid(w) => ("invalid", Some(*w)),
        };
        Ok(json!({
            "state": state, "reason": why, "decisions_recorded": self.decisions_recorded()?,
            "authorization_available": st == KeyState::Valid,
            "recovery_operations_available": true,
            "recovery": if st == KeyState::Valid { Value::Null } else {
                json!("owner-key-reset --replace-invalid (explicit; an invalid file is preserved aside, never overwritten)") },
        }))
    }
    /// Explicit owner recovery. Refuses when a valid credential exists. An invalid one is
    /// moved aside (kept as evidence, never deleted or silently replaced) before a new one
    /// is created. Recorded decisions and their effects are unaffected; an identical retry
    /// of a pre-reset decision can no longer be re-authenticated.
    pub fn owner_key_reset(&mut self, replace_invalid: bool) -> Result<Value, Error> {
        if !replace_invalid {
            return Err(Error::Rejected(
                "owner-key-reset requires the explicit --replace-invalid flag".into(),
            ));
        }
        match key_state(self.root())? {
            KeyState::Valid => {
                return Err(Error::Rejected(
                    "owner credential is valid; refusing to replace it".into(),
                ));
            }
            KeyState::Missing => {
                create_key_atomic(self.root())?;
                let n = self.decisions_recorded()?;
                self.audit(
                    None,
                    "owner_key.created",
                    "owner.key",
                    json!({"reason": "explicit owner reset; credential was missing", "decisions_recorded": n}),
                )?;
            }
            KeyState::Invalid(why) => {
                let old = fs::read(self.root().join(OWNER_KEY)).unwrap_or_default();
                let aside = format!(
                    "{OWNER_KEY}.invalid-{}-{}",
                    now_ms()?,
                    &uuid::Uuid::new_v4().to_string()[..8]
                );
                let (from, to) = (
                    path_c(&self.root().join(OWNER_KEY))?,
                    path_c(&self.root().join(&aside))?,
                );
                // SAFETY: valid NUL-terminated absolute paths; NOREPLACE.
                let rc = unsafe {
                    libc::renameat2(
                        libc::AT_FDCWD,
                        from.as_ptr(),
                        libc::AT_FDCWD,
                        to.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                fs::File::open(self.root())?.sync_all()?;
                create_key_atomic(self.root())?;
                let n = self.decisions_recorded()?;
                self.audit(
                    None,
                    "owner_key.replaced",
                    "owner.key",
                    json!({"reason": why, "preserved_as": aside, "old_len": old.len(),
                        "old_sha256": crate::artifact::sha256_hex(&old), "decisions_recorded": n}),
                )?;
            }
        }
        self.owner_key_status()
    }
    pub(crate) fn audit(
        &mut self,
        work: Option<&str>,
        kind: &str,
        entity: &str,
        payload: Value,
    ) -> Result<(), Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json) VALUES(?1,'coordinator',?2,?3,?4,?5)",
            params![work, kind, entity, now_ms()?, payload.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// One-way retirement of an open review on a genuine transition only. Cancels an
    /// unclaimed effect intent; never resets a decision or a consumed approval.
    pub(crate) fn retire(&mut self, review: &str, s: &Stale) -> Result<bool, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let did = retire_in(&tx, review, s)?;
        tx.commit()?;
        Ok(did)
    }
    /// Full assessment of an open review: authorization AND bound content/target.
    pub(crate) fn assess(&mut self, r: &ReviewRow) -> Result<(Envelope, Scan), Stale> {
        let env = Envelope::parse(&r.envelope_json, &r.envelope_hash)
            .map_err(|e| stale("invalidated", e))?;
        let now = crate::store::authority_time(&self.conn)
            .map_err(|e| stale("expired", format!("clock unavailable: {e}")))?;
        if env.review_id != r.id
            || env.work_id != r.work
            || env.attempt_id != r.attempt
            || env.candidate_artifact.id != r.candidate
            || env.expires_at != r.expires_at
        {
            return Err(stale(
                "invalidated",
                "review row does not match its envelope",
            ));
        }
        authority_current(&self.conn, &env, &now)?;
        if env.policy
            != (Policy {
                review_policy: REVIEW_POLICY.into(),
                validator: VALIDATOR_VERSION.into(),
                validator_epoch: VALIDATOR_EPOCH.into(),
                evidence_fresh_ms: TRUSTED_FRESH_MS,
            })
        {
            return Err(stale(
                "invalidated",
                "validator or review policy version changed",
            ));
        }
        let mut scans = Vec::new();
        for a in [&env.initial_artifact, &env.candidate_artifact] {
            let row: Option<ArtifactRow> = self
                .conn
                .query_row(
                    &format!("SELECT {ARTIFACT_COLUMNS} FROM artifact WHERE id=?1"),
                    [&a.id],
                    artifact_row,
                )
                .optional()
                .map_err(|e| stale("invalidated", e.to_string()))?;
            let Some(row) = row else {
                return Err(stale("invalidated", format!("artifact {} missing", a.id)));
            };
            if row.manifest_hash != a.manifest_hash {
                return Err(stale(
                    "invalidated",
                    "artifact manifest differs from review",
                ));
            }
            match self.verified(&row) {
                Ok(s) => scans.push(s),
                Err(e) => {
                    return Err(stale(
                        "invalidated",
                        format!("frozen artifact unusable: {e}"),
                    ));
                }
            }
        }
        let attempt_state: String = self
            .conn
            .query_row(
                "SELECT state FROM attempt WHERE id=?1",
                [&env.attempt_id],
                |r| r.get(0),
            )
            .map_err(|e| stale("invalidated", e.to_string()))?;
        if attempt_state != "cleaned" {
            return Err(stale(
                "invalidated",
                format!("preparation lifecycle is {attempt_state}; unresolved activity"),
            ));
        }
        if let Err(q) = quiescence(&self.conn, &env.attempt_id) {
            return Err(stale(
                "invalidated",
                format!("candidate writers not quiescent: {q}"),
            ));
        }
        let repo = env.action.repo();
        let (recorded, _) =
            repo_for(&self.conn, &r.repo_id).map_err(|e| stale("invalidated", e.to_string()))?;
        if recorded != repo || env.action.repository.id != r.repo_id {
            return Err(stale(
                "invalidated",
                "repository identity differs from review",
            ));
        }
        gitfixture::verify(&repo).map_err(|e| stale("invalidated", e))?;
        match gitfixture::read_ref(&repo) {
            Ok(v) if v == env.action.expected_old => (),
            Ok(v) => {
                return Err(stale(
                    "invalidated",
                    format!("target ref changed after review (now {v})"),
                ));
            }
            Err(e) => return Err(stale("invalidated", e)),
        }
        match gitfixture::lock_contents(&repo) {
            Ok(None) => (),
            Ok(Some(_)) => {
                return Err(stale(
                    "invalidated",
                    "target ref is locked by another writer",
                ));
            }
            Err(e) => return Err(stale("invalidated", e)),
        }
        let candidate = scans.pop().expect("two scans");
        let (tree, blobs, commit) = candidate_objects(&candidate, &env.action);
        if tree.id != env.action.new_tree || commit.id != env.action.new_commit {
            return Err(stale(
                "invalidated",
                "proposed commit no longer matches the frozen candidate",
            ));
        }
        for o in blobs.iter().chain([&tree, &commit]) {
            if !gitfixture::object_present(&repo, o) {
                return Err(stale(
                    "invalidated",
                    format!("proposed object {} missing or altered", o.id),
                ));
            }
        }
        Ok((env, candidate))
    }

    /// T4: create an exact review for a validated frozen candidate. Idempotent while the
    /// open review remains valid; a stale open review is retired first.
    pub fn create_review(&mut self, attempt: &str) -> Result<Value, Error> {
        // Until the review ID is minted, a failure names the attempt; from then on the fault
        // (and any emergency record) carries the review ID, the one identity a read back of
        // this creation can rely on.
        let mut g = crate::storage::Guard::new(crate::storage::Boundary::T4, attempt)
            .identified_by("attempt_id");
        let r = self.create_review_in(attempt, &mut g);
        g.finish(r)
    }
    fn create_review_in(
        &mut self,
        attempt: &str,
        g: &mut crate::storage::Guard,
    ) -> Result<Value, Error> {
        self.reconcile_artifacts()?;
        let c = ctx(&self.conn, attempt)?;
        crate::attention::progress_fence(&self.conn, &c.work)?;
        let refuse = |o: &mut Owner, why: String, block: Option<&str>| -> Error {
            let r = (|| -> Result<(), Error> {
                let tx = o
                    .conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                event(
                    &tx,
                    &c.work,
                    "review.refused",
                    attempt,
                    json!({"reason": why}),
                )?;
                if let Some(b) = block {
                    set_responsibility(&tx, &c.work, "blocked", "local-owner", b)?;
                }
                tx.commit()?;
                Ok(())
            })();
            match r {
                Ok(()) => Error::Rejected(why),
                Err(e) => e,
            }
        };
        let open: Option<ReviewRow> = self
            .conn
            .query_row(
                &format!("SELECT {REVIEW_COLUMNS} FROM review WHERE work_id=?1 AND state IN ('pending','authorized')"),
                [&c.work],
                review_row,
            )
            .optional()?;
        if let Some(r) = open {
            match self.assess(&r) {
                Ok((env, _)) => return self.review_json(&r, &env, true),
                Err(s) => {
                    self.retire(&r.id, &s)?;
                }
            }
        }
        let unresolved: i64 = self.conn.query_row(
            "SELECT count(*) FROM effect WHERE work_id=?1 AND state IN ('intent','dispatching','unknown','conflict','confirmed')",
            [&c.work],
            |r| r.get(0),
        )?;
        if unresolved > 0 {
            return Err(refuse(
                self,
                "an effect for this work is applied, conflicting or unresolved; no new review"
                    .into(),
                None,
            ));
        }
        let t = crate::store::authority_time(&self.conn)?;
        let now = t.stamp();
        let permit_id: String =
            self.conn
                .query_row("SELECT permit_id FROM work WHERE id=?1", [&c.work], |r| {
                    r.get(0)
                })?;
        let PermitState {
            version: pversion,
            scope_hash: phash,
            scope: pscope,
            expires_at: pexpires,
            revoked_at: prevoked,
            issued_at: pissued,
        } = permit_state(&self.conn, &permit_id)?;
        let request: crate::protocol::SubmitWork = serde_json::from_str(&self.conn.query_row(
            "SELECT request_json FROM work WHERE id=?1",
            [&c.work],
            |r| r.get::<_, String>(0),
        )?)?;
        if prevoked.is_some()
            || t.reached(pexpires)
            || t.before(pissued)
            || pscope.fixture_id != request.fixture_id
        {
            return Err(refuse(
                self,
                "preparation permit revoked, expired or outside scope".into(),
                Some("permit_invalid_new_authority_required"),
            ));
        }
        if c.state != "cleaned" {
            return Err(refuse(
                self,
                format!(
                    "preparation lifecycle is {}; cleanup not established",
                    c.state
                ),
                None,
            ));
        }
        if let Err(q) = quiescence(&self.conn, attempt) {
            return Err(refuse(
                self,
                format!("writer quiescence not established: {q}"),
                None,
            ));
        }
        let mut scans = Vec::new();
        let mut arts = Vec::new();
        for role in ["initial", "candidate"] {
            let Some(a) = artifact_for(&self.conn, attempt, role)? else {
                return Err(refuse(self, format!("no {role} artifact"), None));
            };
            let s = match self.verified(&a) {
                Ok(s) => s,
                Err(e) => {
                    return Err(refuse(
                        self,
                        e.to_string(),
                        Some("inspect_invalid_artifact"),
                    ));
                }
            };
            scans.push(s);
            arts.push(a);
        }
        let rows = evidence::rows(&self.conn, attempt, now)?;
        let mut evrefs = Vec::new();
        let mut gaps = Vec::new();
        for (kind, art) in [
            ("initial_defect", &arts[0]),
            ("candidate_validation", &arts[1]),
            ("capture_coverage", &arts[1]),
        ] {
            let Some(r) = rows
                .iter()
                .rev()
                .find(|r| r.kind == kind && r.authority == "trusted_validator")
            else {
                return Err(refuse(
                    self,
                    format!("missing trusted {kind} evidence"),
                    None,
                ));
            };
            if !r.fresh {
                return Err(refuse(
                    self,
                    format!(
                        "trusted {kind} evidence is stale (fresh until {}); bounded revalidation required",
                        r.fresh_until
                    ),
                    Some("revalidate_trusted_evidence"),
                ));
            }
            let bound = r.artifact_id.as_deref() == Some(art.id.as_str())
                && r.payload["manifest_hash"] == json!(art.manifest_hash);
            if !r.trusted || !r.sequence_complete || r.conflicts > 0 || !bound {
                return Err(refuse(
                    self,
                    format!("trusted {kind} evidence is not usable for review"),
                    None,
                ));
            }
            match kind {
                "initial_defect" if r.payload["reproduced"] != true => {
                    return Err(refuse(self, "initial defect not reproduced".into(), None));
                }
                "candidate_validation"
                    if r.payload["validation_passed"] != true
                        || r.payload["capture_unchanged"] != true =>
                {
                    return Err(refuse(
                        self,
                        "candidate validation did not pass".into(),
                        None,
                    ));
                }
                "capture_coverage" => gaps = r.gaps.clone(),
                _ => (),
            }
            let eh: String = self.conn.query_row(
                "SELECT envelope_hash FROM evidence WHERE id=?1",
                [&r.id],
                |x| x.get(0),
            )?;
            let ph: String = self.conn.query_row(
                "SELECT payload_hash FROM evidence WHERE id=?1",
                [&r.id],
                |x| x.get(0),
            )?;
            evrefs.push(EvRef {
                id: r.id.clone(),
                kind: kind.into(),
                envelope_hash: eh,
                payload_hash: ph,
                observed_at: r.observed_at,
                fresh_until: r.fresh_until,
            });
        }
        let conflicts: i64 = self.conn.query_row(
            "SELECT count(*) FROM event WHERE kind='evidence.conflict' AND entity_id IN (SELECT id FROM evidence WHERE attempt_id=?1)",
            [attempt],
            |r| r.get(0),
        )?;
        if conflicts > 0 {
            return Err(refuse(
                self,
                "evidence conflicts audited for this attempt".into(),
                None,
            ));
        }
        // Repository: existing identity verified, or a new disposable one created with
        // its base commit from the frozen INITIAL bytes. Files precede the DB reference.
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM fixture_repo WHERE work_id=?1",
                [&c.work],
                |r| r.get(0),
            )
            .optional()?;
        let sealed_secs = |a: &ArtifactRow, o: &Owner| -> Result<i64, Error> {
            Ok(o.conn
                .query_row("SELECT sealed_at FROM artifact WHERE id=?1", [&a.id], |r| {
                    r.get::<_, i64>(0)
                })?
                / 1000)
        };
        let (repo_id, repo, base, new_repo) = match existing {
            Some(id) => {
                let (repo, base) = repo_for(&self.conn, &id)?;
                if let Err(e) = gitfixture::verify(&repo) {
                    return Err(refuse(self, e, Some("inspect_fixture_repository")));
                }
                (id, repo, base, false)
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let repo = gitfixture::create(self.root(), &id)?;
                let (tree, blobs) = gitfixture::tree(&scans[0].bytes);
                let base = gitfixture::commit(
                    &tree.id,
                    None,
                    sealed_secs(&arts[0], self)?,
                    &format!(
                        "fixture base\nwork {}\ninitial artifact {} manifest {}",
                        c.work, arts[0].id, arts[0].manifest_hash
                    ),
                );
                for o in blobs.iter().chain([&tree, &base]) {
                    gitfixture::write_object(&repo, o)?;
                }
                gitfixture::initialize_ref(&repo, &base.id)?;
                (id, repo, base.id, true)
            }
        };
        match gitfixture::read_ref(&repo) {
            Ok(v) if v == base => (),
            Ok(v) => {
                return Err(refuse(
                    self,
                    format!(
                        "target ref is {v}, not the recorded base; only base->candidate is supported"
                    ),
                    Some("inspect_fixture_repository"),
                ));
            }
            Err(e) => return Err(refuse(self, e, Some("inspect_fixture_repository"))),
        }
        let review_id = uuid::Uuid::new_v4().to_string();
        g.set_identity("review_id", &review_id);
        let mut action = Action {
            kind: ACTION_KIND.into(),
            version: 1,
            repository: RepoRef {
                id: repo_id.clone(),
                path: repo.path.to_string_lossy().into(),
                dev: repo.dev,
                ino: repo.ino,
                nonce: repo.nonce.clone(),
            },
            ref_name: gitfixture::REF_NAME.into(),
            expected_old: base.clone(),
            new_commit: String::new(),
            new_tree: String::new(),
            commit_secs: sealed_secs(&arts[1], self)?,
            commit_message: format!(
                "fixture candidate\nreview {review_id}\ncandidate artifact {} manifest {}",
                arts[1].id, arts[1].manifest_hash
            ),
            scope: SCOPE_STATEMENT.into(),
        };
        let (tree, blobs, commit) = candidate_objects(&scans[1], &action);
        for o in blobs.iter().chain([&tree, &commit]) {
            gitfixture::write_object(&repo, o)?;
        }
        action.new_tree = tree.id.clone();
        action.new_commit = commit.id.clone();
        action.validate()?;
        let freshest = evrefs.iter().map(|e| e.fresh_until).min().unwrap_or(now);
        let expires = (now + REVIEW_MAX_MS).min(freshest).min(pexpires);
        if expires < now + MIN_REVIEW_MS {
            return Err(refuse(
                self,
                "evidence or permit expires too soon for a reviewable decision".into(),
                Some("revalidate_trusted_evidence"),
            ));
        }
        let manifest = |a: &ArtifactRow| -> Result<ArtRef, Error> {
            Ok(ArtRef {
                id: a.id.clone(),
                manifest_hash: a.manifest_hash.clone(),
                manifest: serde_json::from_str(&a.manifest_json)?,
            })
        };
        let env = Envelope {
            version: 1,
            review_id: review_id.clone(),
            work_id: c.work.clone(),
            attempt_id: attempt.into(),
            request_hash: fingerprint(&serde_json::to_string(&request)?),
            permit: PermitRef {
                id: permit_id.clone(),
                version: pversion,
                scope_hash: phash,
                fixture_id: pscope.fixture_id.clone(),
                expires_at: pexpires,
                grants_final_action: false,
            },
            policy: Policy {
                review_policy: REVIEW_POLICY.into(),
                validator: VALIDATOR_VERSION.into(),
                validator_epoch: VALIDATOR_EPOCH.into(),
                evidence_fresh_ms: TRUSTED_FRESH_MS,
            },
            initial_artifact: manifest(&arts[0])?,
            candidate_artifact: manifest(&arts[1])?,
            evidence: evrefs,
            capture: Capture {
                restored: false,
                gaps,
            },
            containment: CONTAINMENT_LIMIT.into(),
            action,
            approver_role: APPROVER_ROLE.into(),
            created_at: now,
            expires_at: expires,
        };
        let json_env = env.canonical()?;
        let hash = fingerprint(&json_env);
        let epoch = self.epoch;
        barrier("before_review_begin")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if new_repo {
            tx.execute(
                "INSERT INTO fixture_repo VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    repo_id,
                    c.work,
                    attempt,
                    repo.path.to_string_lossy(),
                    repo.dev as i64,
                    repo.ino as i64,
                    repo.nonce,
                    gitfixture::REF_NAME,
                    base,
                    arts[0].id,
                    epoch,
                    now
                ],
            )?;
            event(
                &tx,
                &c.work,
                "fixture_repo.created",
                &repo_id,
                json!({"path": repo.path, "base_commit": base, "no_remote": true, "hooks": "empty"}),
            )?;
        }
        authority_current(&tx, &env, &crate::store::authority_time(&tx)?)
            .map_err(|s| Error::Rejected(s.reason))?;
        set_due(
            &tx,
            &c.work,
            "reviewing",
            "local-owner",
            "decide_exact_review",
            expires,
        )?;
        let version: i64 =
            tx.query_row("SELECT version FROM work WHERE id=?1", [&c.work], |r| {
                r.get(0)
            })?;
        tx.execute(
            "INSERT INTO review VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',NULL,?9,?10,?11)",
            params![
                review_id, c.work, attempt, arts[1].id, repo_id, version, hash, json_env, expires,
                now, epoch
            ],
        )?;
        crate::attention::link_review(&tx, &c.work, &review_id)?;
        for e in &env.evidence {
            tx.execute(
                "INSERT INTO review_evidence VALUES(?1,?2,?3)",
                params![review_id, e.id, e.envelope_hash],
            )?;
        }
        event(
            &tx,
            &c.work,
            "review.created",
            &review_id,
            json!({"envelope_hash": hash, "expires_at": expires, "action": ACTION_KIND}),
        )?;
        g.commit(tx)?;
        barrier("after_review_commit")?;
        let r = load_review(&self.conn, &review_id)?.ok_or(Error::Invalid("missing review"))?;
        self.review_json(&r, &env, false)
    }
    pub(crate) fn review_json(
        &self,
        r: &ReviewRow,
        env: &Envelope,
        existing: bool,
    ) -> Result<Value, Error> {
        Ok(json!({
            "review_id": r.id, "state": r.state, "state_reason": r.state_reason,
            "envelope_hash": r.envelope_hash, "work_version": r.work_version, "expires_at": r.expires_at,
            "action": {"kind": env.action.kind, "repository": env.action.repository.path, "ref": env.action.ref_name,
                       "expected_old": env.action.expected_old, "new_commit": env.action.new_commit, "scope": env.action.scope},
            "envelope": env, "existing": existing,
            "approval_grants": "exactly this one fixture ref compare-and-swap; preparation permit grants no final action",
            "work_success": false, "work_closed": false,
        }))
    }

    /// Build an owner-signed decision for the attended local owner console.
    pub fn decision_template(
        &mut self,
        review: &str,
        decision_id: &str,
        choice: &str,
    ) -> Result<DecisionRequest, Error> {
        let r = load_review(&self.conn, review)?.ok_or(Error::Invalid("unknown review"))?;
        let core = DecisionCore {
            protocol_version: 1,
            decision_id: decision_id.into(),
            review_id: r.id,
            envelope_hash: r.envelope_hash,
            expected_work_version: r.work_version,
            choice: choice.into(),
            actor: APPROVER_ROLE.into(),
        };
        let p = proof(&self.owner_key()?, &core)?;
        Ok(DecisionRequest {
            protocol_version: core.protocol_version,
            decision_id: core.decision_id,
            review_id: core.review_id,
            envelope_hash: core.envelope_hash,
            expected_work_version: core.expected_work_version,
            choice: core.choice,
            actor: core.actor,
            owner_proof: p,
        })
    }

    /// T5: record one immutable owner decision. Replay lookup precedes current-state
    /// validation; approval and effect intent commit atomically. Recording is not execution.
    pub fn decide(&mut self, body: &[u8]) -> Result<Value, Error> {
        let mut g = crate::storage::Guard::new(crate::storage::Boundary::T5, "");
        let r = self.decide_in(body, &mut g);
        g.finish(r)
    }
    fn decide_in(&mut self, body: &[u8], g: &mut crate::storage::Guard) -> Result<Value, Error> {
        let digest = crate::artifact::sha256_hex(body);
        let reject =
            |o: &mut Owner, work: Option<&str>, entity: &str, kind: &str, why: &str| -> Error {
                match o.audit(
                    work,
                    kind,
                    entity,
                    json!({"reason": why, "body_sha256": digest}),
                ) {
                    Ok(()) => Error::Rejected(why.into()),
                    Err(e) => e,
                }
            };
        if body.len() > MAX_DECISION_BYTES {
            return Err(reject(
                self,
                None,
                "decision",
                "decision.rejected",
                "decision exceeds size bound",
            ));
        }
        if serde_json::from_slice::<NoDuplicates>(body).is_err() {
            return Err(reject(
                self,
                None,
                "decision",
                "decision.rejected",
                "malformed decision or duplicate keys",
            ));
        }
        let Ok(req) = serde_json::from_slice::<DecisionRequest>(body) else {
            return Err(reject(
                self,
                None,
                "decision",
                "decision.rejected",
                "invalid decision envelope (unknown or missing fields)",
            ));
        };
        if req.protocol_version != 1
            || !valid_id(&req.decision_id)
            || !valid_id(&req.review_id)
            || !matches!(req.choice.as_str(), "approve" | "reject")
            || req.envelope_hash.len() != 64
        {
            return Err(reject(
                self,
                None,
                "decision",
                "decision.rejected",
                "invalid decision fields",
            ));
        }
        let entity = req.decision_id.clone();
        g.set_id(&entity);
        // Authentication: the owner role and the owner credential, before anything else.
        if req.actor != APPROVER_ROLE {
            return Err(reject(
                self,
                None,
                &entity,
                "decision.forgery",
                "only the authenticated local owner may decide",
            ));
        }
        let core = req.core();
        let expected = proof(&self.owner_key()?, &core)?;
        if !constant_eq(&expected, &req.owner_proof) {
            return Err(reject(
                self,
                None,
                &entity,
                "decision.forgery",
                "owner authentication failed",
            ));
        }
        let request_hash = fingerprint(&serde_json::to_string(&core)?);
        // Replay BEFORE current-state validation.
        if let Some((h, mut receipt)) = decision_receipt(&self.conn, &req.decision_id)? {
            if h == request_hash {
                receipt["disposition"] = json!("existing");
                return Ok(receipt);
            }
            let w: Option<String> = self.conn.query_row("SELECT r.work_id FROM decision d JOIN review r ON r.id=d.review_id WHERE d.id=?1", [&req.decision_id], |r| r.get(0)).optional()?;
            return Err(reject(
                self,
                w.as_deref(),
                &entity,
                "decision.conflict",
                "decision ID was already recorded with different content",
            ));
        }
        let Some(r) = load_review(&self.conn, &req.review_id)? else {
            return Err(reject(
                self,
                None,
                &entity,
                "decision.refused",
                "unknown review",
            ));
        };
        if let Err(e) = crate::attention::progress_fence(&self.conn, &r.work) {
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.refused",
                &e.to_string(),
            ));
        }
        let other: Option<String> = self
            .conn
            .query_row("SELECT id FROM decision WHERE review_id=?1", [&r.id], |x| {
                x.get(0)
            })
            .optional()?;
        if let Some(o) = other {
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.conflict",
                &format!("review already has effective decision {o}; competing decision refused"),
            ));
        }
        if r.state != "pending" {
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.refused",
                &format!("review is {}; a new review is required", r.state),
            ));
        }
        if req.envelope_hash != r.envelope_hash {
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.refused",
                "decision names a different envelope than the current review",
            ));
        }
        let work_version: i64 =
            self.conn
                .query_row("SELECT version FROM work WHERE id=?1", [&r.work], |x| {
                    x.get(0)
                })?;
        if req.expected_work_version != r.work_version || work_version != r.work_version {
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.refused",
                "expected work/review version is not current",
            ));
        }
        let env = match self.assess(&r) {
            Ok((env, _)) => env,
            Err(s) => {
                self.retire(&r.id, &s)?;
                return Err(reject(
                    self,
                    Some(&r.work),
                    &entity,
                    "decision.refused",
                    &format!("review can no longer be decided: {}", s.reason),
                ));
            }
        };
        let epoch = self.epoch;
        barrier("before_decision_begin")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let t = crate::store::authority_time(&tx)?;
        let now = t.stamp();
        let again = load_review(&tx, &r.id)?.ok_or(Error::Invalid("missing review"))?;
        let v: i64 = tx.query_row("SELECT version FROM work WHERE id=?1", [&r.work], |x| {
            x.get(0)
        })?;
        if again.state != "pending" || v != r.work_version {
            return Err(Error::Rejected(
                "review changed concurrently; refused".into(),
            ));
        }
        if let Err(s) = authority_current(&tx, &env, &t) {
            drop(tx);
            self.retire(&r.id, &s)?;
            return Err(reject(
                self,
                Some(&r.work),
                &entity,
                "decision.refused",
                &s.reason,
            ));
        }
        let effect_id = (req.choice == "approve").then(|| uuid::Uuid::new_v4().to_string());
        let receipt = json!({
            "decision_id": req.decision_id, "review_id": r.id, "envelope_hash": r.envelope_hash,
            "choice": req.choice, "effect_id": effect_id, "recorded_at": now, "disposition": "recorded",
            "meaning": "decision recorded; the action has NOT been executed",
            "executed": false, "work_success": false, "work_closed": false,
        });
        tx.execute(
            "INSERT INTO decision VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                req.decision_id,
                r.id,
                req.actor,
                request_hash,
                r.envelope_hash,
                req.expected_work_version,
                req.choice,
                now,
                receipt.to_string(),
                epoch
            ],
        )?;
        if let Some(eid) = &effect_id {
            let action = serde_json::to_string(&env.action)?;
            tx.execute("UPDATE review SET state='authorized' WHERE id=?1", [&r.id])?;
            tx.execute(
                "INSERT INTO effect(id,work_id,review_id,decision_id,repo_id,action_kind,action_hash,action_json,state,state_reason,intent_epoch,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'intent','approved; not yet dispatched',?9,?10)",
                params![eid, r.work, r.id, req.decision_id, r.repo_id, ACTION_KIND, fingerprint(&action), action, epoch, now],
            )?;
            set_due(
                &tx,
                &r.work,
                "applying",
                "coordinator",
                "dispatch_approved_effect",
                now + RESPONSE_MS,
            )?;
        } else {
            tx.execute(
                "UPDATE review SET state='rejected',state_reason='owner rejected' WHERE id=?1",
                [&r.id],
            )?;
            set_due(
                &tx,
                &r.work,
                "blocked",
                "local-owner",
                "review_rejected_new_review_or_close",
                now + RESPONSE_MS,
            )?;
        }
        event(
            &tx,
            &r.work,
            "decision.recorded",
            &req.decision_id,
            json!({"review_id": r.id, "choice": req.choice, "effect_id": effect_id}),
        )?;
        barrier("before_decision_commit")?;
        g.commit(tx)?;
        barrier("after_decision_commit")?;
        Ok(receipt)
    }

    /// Retire open reviews whose authority or bound content is no longer current.
    /// Writes only on genuine transitions, so repeated status never postpones deadlines.
    pub fn sweep_reviews(&mut self) -> Result<(), Error> {
        let open: Vec<ReviewRow> = {
            let mut q = self.conn.prepare(&format!(
                "SELECT {REVIEW_COLUMNS} FROM review WHERE state IN ('pending','authorized') ORDER BY created_at,id"
            ))?;
            q.query_map([], review_row)?.collect::<Result<_, _>>()?
        };
        for r in open {
            if let Err(s) = self.assess(&r) {
                self.retire(&r.id, &s)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hmac(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(constant_eq("ab", "ab") && !constant_eq("ab", "ac") && !constant_eq("a", "ab"));
    }
}

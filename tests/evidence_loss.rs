//! Lost, corrupt, truncated or torn evidence is an explicit durable gap or an
//! inconclusive result, never completeness, a pass or a supported success. Every loss here is
//! SIMULATED on a disposable fixture: output overflow through the fault-injection seam
//! `ASHER_TEST_EXTRA_OUTPUT`, capture loss by editing the disposable `captured.log`, and a torn
//! supervisor fact by appending a truncated payload row to a disposable ledger.
#![cfg(feature = "fault-injection")]
use asher_bridge::{
    Owner,
    protocol::{PermitScope, SubmitWork},
    store::now_ms,
};
use rusqlite::Connection;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fx {
    _t: TempDir,
    root: PathBuf,
}
impl Fx {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("state");
        let mut o = Owner::initialize(&root).unwrap();
        o.add_permit(
            "p",
            &PermitScope {
                fixture_id: "fixture:symbol".into(),
                action: "prepare_fixture".into(),
                max_attempts: 1,
                max_runtime_ms: 60000,
                network: false,
            },
            now_ms().unwrap() + 600_000,
        )
        .unwrap();
        o.submit(&SubmitWork {
            protocol_version: 1,
            request_key: "w".into(),
            fixture_id: "fixture:symbol".into(),
            success_criteria: "Prepare synthetic symbol correction; capture remains incomplete"
                .into(),
            source_evidence: vec!["fixture:malformed-symbol".into()],
            permit_id: "p".into(),
        })
        .unwrap();
        Self { _t: t, root }
    }
    fn cmd(&self, action: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        c.arg(action).arg(&self.root);
        c
    }
    fn run(&self, action: &str, args: &[&str]) -> Output {
        self.cmd(action).args(args).output().unwrap()
    }
    fn ok(&self, action: &str, args: &[&str]) -> String {
        let o = self.run(action, args);
        assert!(
            o.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into()
    }
    fn refused(&self, action: &str, args: &[&str]) -> String {
        let o = self.run(action, args);
        assert!(!o.status.success(), "{action} unexpectedly succeeded");
        String::from_utf8_lossy(&o.stderr).into()
    }
    fn prepared(&self, overflow: bool) {
        self.ok("symbol-intent", &["w", "a"]);
        self.ok("reproduce", &["a"]);
        let mut c = self.cmd("run-fake");
        c.args(["w", "a", "prepare_symbol", "1000"]);
        if overflow {
            c.env("ASHER_TEST_EXTRA_OUTPUT", "1");
        }
        let o = c.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.join("ledger.db")).unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn text(&self, sql: &str) -> String {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn gaps(&self, kind: &str) -> i64 {
        self.count(&format!(
            "SELECT count(*) FROM evidence_gap WHERE attempt_id='a' AND kind='{kind}'"
        ))
    }
    fn capture(&self) -> PathBuf {
        PathBuf::from(self.text("SELECT fixture_path FROM attempt WHERE id='a'"))
            .join("captured.log")
    }
    fn no_pass(&self) {
        assert_eq!(
            self.count("SELECT count(*) FROM evidence WHERE authority='trusted_validator' AND kind='candidate_validation'"),
            0,
            "a gap must never produce a validator pass"
        );
        assert_eq!(
            self.count("SELECT count(*) FROM work WHERE mode='closed'"),
            0
        );
    }
}

#[test]
fn clock_and_budget_output_truncation_is_a_recorded_gap() {
    let f = Fx::new();
    f.prepared(true);
    // M1 classification is kept: cleanup of the supported scope was confirmed.
    assert_eq!(f.text("SELECT state FROM attempt WHERE id='a'"), "cleaned");
    assert!(fs::metadata(f.capture()).unwrap().len() <= 16384);
    // The truncation is a durable gap, recorded once by the supervisor that observed it.
    assert_eq!(
        f.gaps("output_truncated"),
        1,
        "truncation must be a recorded gap"
    );
    let detail = f.text("SELECT detail_json FROM evidence_gap WHERE kind='output_truncated'");
    assert!(detail.contains("16384"), "{detail}");
    // The truncated attempt can never be sealed, validated or closed as a success.
    let why = f.refused("seal", &["a"]);
    assert!(why.contains("output_truncated"), "{why}");
    f.ok("reconcile", &[]);
    assert_eq!(f.gaps("output_truncated"), 1, "recorded once");
    // Visible, read-only, in diagnose.
    let d: serde_json::Value = serde_json::from_str(&f.ok("diagnose", &[])).unwrap();
    assert_eq!(d["ledger"]["evidence_gaps"]["output_truncated"], 1, "{d}");
    f.no_pass();
}

#[test]
fn missing_or_torn_capture_is_inconclusive() {
    for (case, kind) in [("missing", "capture_missing"), ("torn", "capture_torn")] {
        let f = Fx::new();
        f.prepared(false);
        let cap = f.capture();
        let len = fs::metadata(&cap).unwrap().len();
        assert!(len > 0);
        match case {
            // SIMULATED evidence loss on the disposable fixture.
            "missing" => fs::remove_file(&cap).unwrap(),
            _ => {
                let fh = fs::OpenOptions::new().write(true).open(&cap).unwrap();
                fh.set_len(len - 1).unwrap();
            }
        }
        // Inconclusive before any restart accounting ...
        let why = f.refused("seal", &["a"]);
        assert!(why.contains(kind), "{case}: {why}");
        // ... and an explicit durable gap after a restart reconciles, recorded once.
        f.ok("reconcile", &[]);
        f.ok("reconcile", &[]);
        assert_eq!(f.gaps(kind), 1, "{case}");
        let why = f.refused("seal", &["a"]);
        assert!(why.contains(kind), "{case}: {why}");
        f.no_pass();
    }
}

#[test]
fn corrupt_supervisor_fact_is_a_gap_not_a_crash() {
    // A torn stop request (truncated JSON) after the last durable fact, and a torn cleanup
    // record as the latest fact. Neither may abort reconcile or count as cleanup.
    for kind in ["stop_requested", "cleanup_completed"] {
        let f = Fx::new();
        f.prepared(false);
        f.db()
            .execute(
                "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a',?1,?2,'{\"reason\":\"par')",
                rusqlite::params![kind, now_ms().unwrap()],
            )
            .unwrap();
        f.ok("reconcile", &[]);
        assert_eq!(
            f.text("SELECT state FROM attempt WHERE id='a'"),
            "unknown",
            "{kind}"
        );
        let reason = f.text("SELECT reason FROM attempt WHERE id='a'");
        assert!(reason.contains("unreadable"), "{kind}: {reason}");
        f.ok("reconcile", &[]);
        assert_eq!(f.gaps("fact_corrupt"), 1, "{kind}");
        assert!(!f.refused("seal", &["a"]).is_empty());
        f.no_pass();
    }
}

// ------------------------------------------------------------------ captured-output digest
/// Why the attempt's capture gap was recorded.
fn torn_why(f: &Fx) -> String {
    f.text("SELECT detail_json FROM evidence_gap WHERE attempt_id='a' AND kind='capture_torn'")
}

/// SIMULATED same-length corruption after cleanup (a strict reproducer):
/// every byte of the disposable capture is replaced and the size kept. The supervisor's
/// digest of the bytes it wrote no longer matches, so nothing may be sealed or validated.
#[test]
fn same_length_corrupt_capture_is_a_gap_and_never_a_pass() {
    let f = Fx::new();
    f.prepared(false);
    let cap = f.capture();
    let before = fs::read(&cap).unwrap();
    assert!(!before.is_empty());
    fs::write(&cap, vec![b'X'; before.len()]).unwrap();
    assert_eq!(fs::metadata(&cap).unwrap().len(), before.len() as u64);
    // Inconclusive at once ...
    let why = f.refused("seal", &["a"]);
    assert!(why.contains("capture_torn"), "{why}");
    // ... and one explicit durable gap after reconcile.
    f.ok("reconcile", &[]);
    f.ok("reconcile", &[]);
    assert_eq!(f.gaps("capture_torn"), 1);
    assert!(torn_why(&f).contains("content differs from the recorded digest"));
    assert!(!f.refused("seal", &["a"]).is_empty());
    f.no_pass();
    // Putting bytes back does not help a single flipped byte either.
    let mut one = before.clone();
    one[0] ^= 1;
    fs::write(&cap, &one).unwrap();
    assert!(!f.refused("seal", &["a"]).is_empty());
    f.no_pass();
}

/// The authentic capture is the bytes the supervisor hashed while writing them: the durable
/// cleanup record names their SHA-256, and an unchanged capture seals and validates.
#[test]
fn authentic_capture_matches_the_supervisor_digest_and_still_passes() {
    use sha2::{Digest, Sha256};
    let f = Fx::new();
    f.prepared(false);
    let fact: serde_json::Value = serde_json::from_str(&f.text(
        "SELECT payload_json FROM supervisor_fact WHERE attempt_id='a' AND kind='cleanup_completed'",
    ))
    .unwrap();
    let bytes = fs::read(f.capture()).unwrap();
    assert_eq!(fact["bytes_captured"], bytes.len() as u64);
    assert_eq!(
        fact["capture_sha256"].as_str().unwrap(),
        format!("{:x}", Sha256::digest(&bytes))
    );
    f.ok("reconcile", &[]);
    assert_eq!(f.count("SELECT count(*) FROM evidence_gap"), 0);
    let art: serde_json::Value = serde_json::from_str(&f.ok("seal", &["a"])).unwrap();
    f.ok("validate", &[art["id"].as_str().unwrap()]);
    assert_eq!(
        f.count("SELECT count(*) FROM evidence WHERE authority='trusted_validator' AND kind='candidate_validation'"),
        1
    );
}

/// A cleanup record without a usable digest (as written by an earlier build, or with a
/// malformed one) cannot vouch for the capture's content, even when the size agrees. SIMULATED
/// by appending a later record to the disposable ledger.
#[test]
fn cleanup_record_without_a_trustworthy_digest_is_a_gap() {
    for digest in ["", r#","capture_sha256":"abc""#, r#","capture_sha256":7"#] {
        let f = Fx::new();
        f.prepared(false);
        let len = fs::metadata(f.capture()).unwrap().len();
        // Facts are immutable, so the case is modelled by a later cleanup record of the
        // kind an older supervisor wrote: the size, and no usable digest.
        f.db()
            .execute(
                "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','cleanup_completed',?1,?2)",
                rusqlite::params![
                    now_ms().unwrap(),
                    format!(r#"{{"bytes_observed":{len},"bytes_captured":{len}{digest}}}"#)
                ],
            )
            .unwrap();
        let why = f.refused("seal", &["a"]);
        assert!(why.contains("capture_torn"), "{digest}: {why}");
        f.ok("reconcile", &[]);
        assert_eq!(f.gaps("capture_torn"), 1, "{digest}");
        f.no_pass();
    }
}

/// The digest is established only by the supervisor. A second cleanup record naming the
/// digest of the replaced bytes (hashed after the fact) does not make them authentic: two
/// records that disagree about the capture are a gap.
#[test]
fn a_later_record_hashing_the_corrupted_bytes_is_not_accepted() {
    use sha2::{Digest, Sha256};
    let f = Fx::new();
    f.prepared(false);
    let cap = f.capture();
    let len = fs::metadata(&cap).unwrap().len() as usize;
    let forged = vec![b'X'; len];
    fs::write(&cap, &forged).unwrap();
    f.db()
        .execute(
            "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES('a','cleanup_completed',?1,?2)",
            rusqlite::params![
                now_ms().unwrap(),
                format!(
                    r#"{{"bytes_observed":{len},"bytes_captured":{len},"capture_sha256":"{:x}"}}"#,
                    Sha256::digest(&forged)
                )
            ],
        )
        .unwrap();
    let why = f.refused("seal", &["a"]);
    assert!(why.contains("capture_torn"), "{why}");
    f.ok("reconcile", &[]);
    assert_eq!(f.gaps("capture_torn"), 1);
    assert!(torn_why(&f).contains("disagree"), "{}", torn_why(&f));
    f.no_pass();
    // No command on this path ever wrote a digest: the supervisor's record is unchanged.
    assert_eq!(
        f.count(
            "SELECT count(*) FROM supervisor_fact WHERE attempt_id='a' AND kind='cleanup_completed'"
        ),
        2
    );
}

/// A capture replaced by a symlink to same-sized bytes, or grown past the output limit, is
/// not read as the capture. SIMULATED on the disposable fixture.
#[test]
fn replaced_or_oversized_capture_is_a_gap() {
    let f = Fx::new();
    f.prepared(false);
    let cap = f.capture();
    let bytes = fs::read(&cap).unwrap();
    let side = cap.with_file_name("elsewhere.log");
    fs::write(&side, &bytes).unwrap();
    fs::remove_file(&cap).unwrap();
    std::os::unix::fs::symlink(&side, &cap).unwrap();
    let why = f.refused("seal", &["a"]);
    assert!(why.contains("capture_torn"), "{why}");
    fs::remove_file(&cap).unwrap();
    fs::write(&cap, vec![b'X'; 16385]).unwrap();
    let why = f.refused("seal", &["a"]);
    assert!(why.contains("capture_torn"), "{why}");
    f.no_pass();
}

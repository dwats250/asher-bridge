use asher_bridge::{
    Error, Owner, Reader,
    evidence::MAX_CLAIM_BYTES,
    protocol::{MAX_REQUEST, PermitScope, SubmitWork},
    recovery,
    storage::{Boundary, Durability, Readback, StorageFault},
    store::now_ms,
};
use serde_json::{Value, json};
use std::{
    env, fmt,
    io::{self, Read},
    path::Path,
};

/// Exit codes. 1: refused or failed (no storage classification). 3/4/5: a classified storage
/// fault at a transaction boundary whose outcome is not committed / unknown / committed.
/// 6: ordinary startup refused (corrupt, incompatible, unavailable or blocked ledger).
const EXIT_NOT_COMMITTED: i32 = 3;
const EXIT_UNKNOWN: i32 = 4;
const EXIT_COMMITTED: i32 = 5;
const EXIT_STARTUP_REFUSED: i32 = 6;

/// An error raised while opening the ledger for ordinary mutating startup.
#[derive(Debug)]
struct Startup(Error);
impl fmt::Display for Startup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Startup {}
fn owner(root: &Path) -> Result<Owner, Startup> {
    Owner::open(root).map_err(Startup)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if let Err(e) = run(&args) {
        eprintln!("{e}");
        let (code, detail) = classify(&*e, args.get(2).map(Path::new));
        if let Some(d) = detail {
            eprintln!("{d}");
        }
        std::process::exit(code);
    }
}

fn storage_exit(f: &StorageFault, root: Option<&Path>) -> (i32, Option<Value>) {
    // Only an outcome that is not known to have failed needs carrying across an outage.
    let record = root
        .filter(|_| f.durability != Durability::NotCommitted)
        .map(|r| recovery::record_incident(r, f));
    let code = match f.durability {
        Durability::NotCommitted => EXIT_NOT_COMMITTED,
        Durability::Unknown => EXIT_UNKNOWN,
        Durability::Committed => EXIT_COMMITTED,
    };
    (
        code,
        Some(json!({"storage_fault": f, "next": f.next_step(),
        "readback": format!("orchestrator readback STATE {} {}{}", f.boundary.as_str(), f.stable_id,
                            if f.step.is_empty() { String::new() } else { format!(" {}", f.step) }),
        "emergency_record": match record {
            Some(Ok(id)) => json!({"incident": id, "durable_receipt": false}),
            Some(Err(e)) => json!({"not_recorded": e}),
            None => Value::Null,
        }})),
    )
}

/// Whether a SQLite error means the database cannot be reached or written right now (as
/// opposed to content or a precondition of an opened, verified ledger).
fn unavailable(s: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode::*;
    matches!(s, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code,
        DatabaseBusy | DatabaseLocked | DiskFull | SystemIoFailure | CannotOpen | ReadOnly
            | PermissionDenied | OutOfMemory | FileLockingProtocolFailed | NoLargeFileSupport))
}

/// Ledger class of a refused or failed ordinary startup.
fn ledger_class(e: &Error) -> &'static str {
    match e {
        Error::Corrupt => "corrupt",
        Error::Schema(_) => "incompatible",
        Error::StartupBlocked(_) => "degraded_startup_blocked",
        Error::Restored(_) => "restored_inspect_only",
        Error::Unreadable(_) => "unreadable",
        // The ledger was opened and verified before these: an availability failure stays
        // `unavailable`; anything else is a condition of the accessible ledger.
        Error::StartupFailed(inner, _) | Error::StartupPrecondition(inner, _) => {
            condition_class(inner)
        }
        Error::Sql(s) => match asher_bridge::storage::kind_of(s).map(|k| k.0) {
            Some(asher_bridge::storage::FaultKind::Corrupt) => "unreadable",
            Some(_) if s.to_string().contains("no such table") => "incompatible",
            Some(_) => "unavailable",
            // Before verification, a missing or mistyped schema version is not an Asher ledger
            // of a supported shape.
            None => "incompatible",
        },
        Error::Io(_) => "unavailable",
        Error::Json(_) => "degraded_malformed_content",
        _ => "refused",
    }
}

/// Class of a failure on an opened, verified ledger: an availability failure stays
/// `unavailable`; anything else is a condition of the accessible ledger.
fn condition_class(inner: &Error) -> &'static str {
    match inner {
        Error::CommitUncertain(_, source) => condition_class(source),
        Error::Sql(s) if unavailable(s) => "unavailable",
        Error::Io(_) => "unavailable",
        Error::Json(_) => "degraded_malformed_content",
        Error::Sql(
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::IntegralValueOutOfRange(..),
        ) => "degraded_malformed_content",
        _ => "degraded_startup_failed",
    }
}

fn classify(e: &(dyn std::error::Error + 'static), root: Option<&Path>) -> (i32, Option<Value>) {
    if let Some(Startup(inner)) = e.downcast_ref::<Startup>() {
        use asher_bridge::{recovery::Recorded, store::Written};
        let (began, cause) = match inner {
            Error::StartupFailed(c, _) => (json!(true), &**c),
            // A failed startup COMMIT may have landed: whether startup began is unknown.
            Error::StartupPrecondition(c, Written::Unknown(_)) => (json!("unknown"), &**c),
            Error::StartupPrecondition(c, _) => (json!(false), &**c),
            other => (json!(false), other),
        };
        return match cause {
            // Contention with a live coordinator is not a ledger condition.
            Error::Owned => (1, None),
            Error::Storage(f) => storage_exit(f, root),
            _ => (
                EXIT_STARTUP_REFUSED,
                Some(json!({"startup_refused": {
                    "ledger": ledger_class(inner), "class": recovery::error_class(inner),
                    "detail": match cause {
                        Error::StartupBlocked(d) | Error::Restored(d) => serde_json::from_str(d).unwrap_or(json!(d)),
                        _ => json!(asher_bridge::store::cause(cause)),
                    },
                    "startup_began": began,
                    "preserved": match inner {
                        Error::StartupFailed(_, Recorded::Yes) => "startup had begun: the epoch advanced, the pass is recorded as a failed recovery run, and reconciliation committed before the failure is kept; nothing consequential was performed".to_string(),
                        Error::StartupFailed(_, Recorded::No) => "startup had begun: the epoch advanced and reconciliation committed before the failure is kept; the failure itself could not be recorded, so the run stays 'started' and the next pass marks it interrupted; nothing consequential was performed".to_string(),
                        Error::StartupFailed(_, Recorded::Unknown) => "startup had begun: the epoch advanced and reconciliation committed before the failure is kept; recording the failure hit a storage fault, so it may or may not be recorded (if not, the run stays 'started' and the next pass marks it interrupted); nothing consequential was performed".to_string(),
                        Error::StartupPrecondition(_, Written::Unknown(m)) => format!("startup may have begun: a startup COMMIT failed and its outcome is unknown, so what it would have written (an additive migration, or the new epoch, owner session and 'started' recovery run) may be in the ledger{}; `diagnose`, or the next pass, shows what landed; nothing consequential was performed", m.map(|v| format!(" (the additive migration from schema {v} had committed)")).unwrap_or_default()),
                        Error::StartupPrecondition(_, Written::Migrated(v)) => format!("startup could not begin: the additive migration from schema {v} committed; no epoch change, recovery run or reconciliation was committed; nothing consequential was performed"),
                        Error::StartupPrecondition(_, Written::MigrationUnknown(v)) => format!("startup could not begin: no epoch change, recovery run or reconciliation was committed; whether the additive migration from schema {v} committed is unknown (the schema version could not be read back after the failure, which establishes nothing); `diagnose`, or the next pass, shows the schema version; nothing consequential was performed"),
                        Error::StartupPrecondition(_, Written::Nothing) => "startup could not begin: nothing was written (no migration, epoch change, recovery run or reconciliation)".to_string(),
                        _ => "refused before any write: no checkpoint, migration, epoch change or reconciliation".to_string(),
                    },
                    "inspect": "orchestrator diagnose STATE (read-only, bounded)"}})),
            ),
        };
    }
    if let Some(Error::Storage(f)) = e.downcast_ref::<Error>() {
        return storage_exit(f, root);
    }
    (1, None)
}

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 3 {
        return Err(
            "usage: orchestrator <init|permit|revoke|get|list|submit|check|run-fake|reconcile|attempts|symbol-intent|reproduce|seal|validate|task3-status|ingest-claim|evidence-status|review-create|task4-status|decision-template|decide|dispatch|reconcile-effects|revalidate|owner-key-status|owner-key-reset|sweep|inbox|command-template|command|closure-summary|diagnose|readback|recover|backup|restore> STATE_DIR [ARGS]"
                .into(),
        );
    }
    let root = Path::new(&args[2]);
    match args[1].as_str() {
        "diagnose" => {
            // Bounded and read-only; it reports even when the ledger cannot be opened.
            println!("{}", asher_bridge::diagnose::diagnose(root));
        }
        "readback" => {
            let b = args
                .get(3)
                .and_then(|b| Boundary::parse(b))
                .ok_or("missing boundary T1|T4|T5|T6|T7|S1|S2")?;
            let id = args.get(4).ok_or("missing stable ID")?;
            // Optional: T7 `executor_result|result_record`, or an S2 fact kind.
            let step = args.get(5).map(String::as_str).unwrap_or("");
            let out = match Reader::open(root).and_then(|r| {
                asher_bridge::storage::readback_step(r.conn_for_readback(), b, step, id)
            }) {
                Ok(Readback::Present(d)) => {
                    json!({"boundary": b.as_str(), "stable_id_kind": b.stable_id_kind(), "stable_id": id,
                           "observed": "present", "detail": d})
                }
                Ok(Readback::Absent(d)) => {
                    json!({"boundary": b.as_str(), "stable_id_kind": b.stable_id_kind(), "stable_id": id,
                           "observed": "absent", "detail": d, "establishes_absence": false,
                           "meaning": "not visible in this read-only view, which does not establish that the transaction did not commit: a commit whose WAL sync failed can stay invisible while any other connection holds the ledger and become visible later. `orchestrator recover STATE` settles an outcome recorded in the emergency log, reading back under the coordinator lock after its own commit; otherwise retry only with the same identity"})
                }
                Ok(Readback::Inconclusive(d)) => {
                    // Only an attempt ID given for T4 is inconclusive; name it as what it is.
                    json!({"boundary": b.as_str(), "stable_id_kind": "attempt_id", "stable_id": id,
                           "observed": "inconclusive", "detail": d, "establishes_absence": false})
                }
                Err(e) => {
                    json!({"boundary": b.as_str(), "stable_id_kind": b.stable_id_kind(), "stable_id": id,
                           "observed": "unavailable", "error": e.to_string(), "establishes_absence": false,
                           "meaning": "a failed or unavailable read back does not establish that the transaction did not commit"})
                }
            };
            println!("{out}");
        }
        "recover" => {
            // One explicitly requested bounded pass; not subject to the startup breaker.
            let o = Owner::open_for_recovery(root).map_err(Startup)?;
            // The pass is committed; report it even if the status read then fails.
            let status = o
                .recovery_status()
                .unwrap_or_else(|e| json!({"unavailable": e.to_string()}));
            println!(
                "{}",
                json!({"recovery_run": o.recovery_summary(), "status": status})
            );
        }
        "backup" => {
            let dest = args.get(3).ok_or("missing new backup directory")?;
            println!("{}", asher_bridge::restore::backup(root, Path::new(dest))?);
        }
        "restore" => {
            // STATE_DIR is the backup directory here; TARGET must be a new, separate path.
            let target = args.get(3).ok_or("missing new target state directory")?;
            println!(
                "{}",
                asher_bridge::restore::restore(root, Path::new(target))?
            );
        }
        "run-fake" => {
            let key = args.get(3).ok_or("missing request key")?;
            let id = args.get(4).ok_or("missing immutable attempt ID")?;
            let mode = args.get(5).ok_or("missing fake mode")?;
            let mode = serde_json::from_value(serde_json::Value::String(mode.clone()))?;
            let runtime_ms = args.get(6).ok_or("missing runtime milliseconds")?.parse()?;
            let mut o = owner(root)?;
            o.intent(
                key,
                id,
                &asher_bridge::execution::Spec {
                    mode,
                    runtime_ms,
                    output_limit: 16384,
                },
            )?;
            println!("{}", serde_json::to_string(&o.execute(id)?)?);
        }
        "symbol-intent" => {
            let key = args.get(3).ok_or("missing request key")?;
            let id = args.get(4).ok_or("missing immutable attempt ID")?;
            let a = owner(root)?.symbol_intent(key, id)?;
            println!("{}", serde_json::to_string(&a)?);
        }
        "reproduce" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            println!("{}", owner(root)?.reproduce(id)?);
        }
        "seal" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            println!("{}", owner(root)?.seal(id)?);
        }
        "validate" => {
            let id = args.get(3).ok_or("missing artifact ID")?;
            println!("{}", owner(root)?.validate(id)?);
        }
        "task3-status" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            println!("{}", owner(root)?.task3_status(id)?);
        }
        "evidence-status" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            let rows = owner(root)?.evidence_status(id)?;
            println!("{}", serde_json::to_string(&rows)?);
        }
        "ingest-claim" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            // Read the bounded body BEFORE taking ownership; oversize is audited, not truncated silently.
            let mut b = Vec::new();
            io::stdin()
                .take((MAX_CLAIM_BYTES + 1) as u64)
                .read_to_end(&mut b)?;
            let ack = owner(root)?.ingest_claim(id, &b)?;
            // Acknowledgment is printed only after the commit returned.
            println!("{}", serde_json::to_string(&ack)?);
        }
        "reconcile" => {
            let mut o = owner(root)?;
            for a in o.reconcile()? {
                println!("{}", serde_json::to_string(&a)?);
            }
        }
        "attempts" => {
            let c = asher_bridge::execution::open_ledger(
                root,
                asher_bridge::execution::Access::Inspect,
            )?;
            for a in asher_bridge::execution::attempts(&c)? {
                println!("{}", serde_json::to_string(&a)?);
            }
        }
        "init" => {
            let o = Owner::initialize(root)?;
            println!(
                "{}",
                serde_json::json!({"initialized":true,"epoch":o.epoch,"execution_available":"fake_only","notification_delivery":"explicit_sweep_only"})
            );
        }
        "permit" => {
            let id = args.get(3).ok_or("missing permit ID")?;
            let fixture = args.get(4).ok_or("missing fixture:ID")?;
            let mut o = owner(root)?;
            o.add_permit(
                id,
                &PermitScope {
                    fixture_id: fixture.clone(),
                    action: "prepare_fixture".into(),
                    max_attempts: 1,
                    max_runtime_ms: 60000,
                    network: false,
                },
                now_ms()? + 3600000,
            )?;
            println!(
                "{}",
                serde_json::json!({"permit":id,"execution_available":"fake_only","notification_delivery":"explicit_sweep_only"})
            );
        }
        "revoke" => {
            // No coordinator lock: revocation can race (and is serialized against) a claim.
            let r =
                asher_bridge::effect::revoke_permit(root, args.get(3).ok_or("missing permit ID")?)?;
            println!("{}", serde_json::json!({"revoked": true, "detail": r}));
        }
        "review-create" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            println!("{}", owner(root)?.create_review(id)?);
        }
        "task4-status" => {
            let id = args.get(3).ok_or("missing attempt ID")?;
            println!("{}", owner(root)?.task4_status(id)?);
        }
        "decision-template" => {
            let review = args.get(3).ok_or("missing review ID")?;
            let decision = args.get(4).ok_or("missing decision ID")?;
            let choice = args.get(5).ok_or("missing choice approve|reject")?;
            let d = owner(root)?.decision_template(review, decision, choice)?;
            println!("{}", serde_json::to_string(&d)?);
        }
        "decide" => {
            let mut b = Vec::new();
            io::stdin()
                .take((asher_bridge::review::MAX_DECISION_BYTES + 1) as u64)
                .read_to_end(&mut b)?;
            println!("{}", owner(root)?.decide(&b)?);
        }
        "dispatch" => {
            let id = args.get(3).ok_or("missing effect ID")?;
            println!("{}", owner(root)?.dispatch(id)?);
        }
        "reconcile-effects" => {
            // Opening already reconciles; this is an explicit owner reprobe as well.
            let mut o = owner(root)?;
            let reprobed = o.reconcile_effects()?;
            println!(
                "{}",
                serde_json::json!({"reprobed": reprobed, "effects": o.effect_summaries()?})
            );
        }
        "sweep" => {
            let backend = match (args.get(3).map(String::as_str), args.get(4)) {
                (Some("--backend"), Some(b)) => asher_bridge::attention::Backend::parse(b)?,
                (None, _) => asher_bridge::attention::Backend::parse("desktop")?,
                _ => return Err("usage: sweep STATE_DIR [--backend desktop]".into()),
            };
            println!("{}", owner(root)?.sweep(backend)?);
        }
        "inbox" => {
            println!("{}", Reader::open(root)?.inbox()?);
        }
        "command-template" => {
            let action = args.get(3).ok_or("missing action")?;
            let target = args
                .get(4)
                .ok_or("missing target (attention:ID or work:ID)")?;
            let id = args.get(5).ok_or("missing command ID")?;
            let ver: i64 = args.get(6).ok_or("missing expected version")?.parse()?;
            let payload: serde_json::Value = match args.get(7) {
                Some(p) => serde_json::from_str(p)?,
                None => serde_json::json!({}),
            };
            let t = owner(root)?.command_template(action, target, id, ver, payload)?;
            println!("{t}");
        }
        "command" => {
            let mut b = Vec::new();
            io::stdin()
                .take((asher_bridge::command::MAX_COMMAND_BYTES + 1) as u64)
                .read_to_end(&mut b)?;
            println!("{}", owner(root)?.command(&b)?);
        }
        "closure-summary" => {
            let key = args.get(3).ok_or("missing request key")?;
            let outcome = args.get(4).ok_or("missing outcome")?;
            println!("{}", owner(root)?.closure_summary(key, outcome)?);
        }
        "owner-key-status" => {
            println!("{}", owner(root)?.owner_key_status()?);
        }
        "owner-key-reset" => {
            let flag = args.get(3).map(String::as_str) == Some("--replace-invalid");
            println!("{}", owner(root)?.owner_key_reset(flag)?);
        }
        "revalidate" => {
            let id = args.get(3).ok_or("missing artifact ID")?;
            println!("{}", owner(root)?.revalidate(id)?);
        }
        "submit" => {
            let mut b = Vec::new();
            io::stdin()
                .take((MAX_REQUEST + 1) as u64)
                .read_to_end(&mut b)?;
            if b.len() > MAX_REQUEST {
                return Err("request too large".into());
            }
            let req: SubmitWork = serde_json::from_slice(&b)?;
            let receipt = owner(root)?.submit(&req)?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
        "get" => {
            let r = Reader::open(root)?;
            let w = r.get(args.get(3).ok_or("missing request key")?)?;
            println!("{}", serde_json::to_string(&w)?);
        }
        "list" => {
            let r = Reader::open(root)?;
            let now = now_ms()?;
            for w in r.list()? {
                println!(
                    "{}",
                    serde_json::json!({"overdue":w.overdue(now),"work":w,"execution_available":"fake_only","notification_delivery":"explicit_sweep_only"})
                );
            }
        }
        "check" => {
            Reader::open(root)?.assert_accounted()?;
            println!(
                "accounting invariants hold; fixture-only review, one fixture ref effect, durable attention and owner-authorized closure; no scheduler, so delivery and overdue recording happen only during an explicit sweep"
            );
        }
        _ => return Err("unknown command".into()),
    }
    Ok(())
}

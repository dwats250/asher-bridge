//! Verified ledger backup and restoration into a separate, explicitly selected target.
//!
//! `backup STATE DEST` copies a verified ledger with SQLite's online backup API into a new
//! private directory as `snapshot.db` (deliberately not `ledger.db`, so a backup directory is
//! never a usable state root) plus a manifest (file digest, logical digest, schema, counts).
//! The copy carries a snapshot marker in its own data: any ledger ever derived from it, by
//! `restore` or by copying the file, is inspect-only. The source is opened read-only.
//!
//! `restore BACKUP TARGET` verifies the backup against its manifest and builds the restored
//! ledger in a staging directory beside TARGET; TARGET appears (atomic rename) only once the
//! copy is verified, the restore recorded and a restore-verification recovery run finished.
//! TARGET must not exist and must not lie inside the backup, the backup's source root, or any
//! ledger root or backup. A restored ledger is inspect-only: ordinary startup, launches and
//! effect dispatch are refused, because external actions may have happened after the backup
//! and its rows cannot show them. The verification run opens a case for every obligation the
//! backup cannot vouch for. Restoration authorizes no replay, resume or consequential repair.
//! Artifact snapshots, fixture repositories and the owner credential are not copied.
use crate::{Error, store};
use rusqlite::{Connection, OpenFlags, ffi};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub const MANIFEST: &str = "backup-manifest.json";
/// The backup copy's file name; never `ledger.db`.
pub const SNAPSHOT: &str = "snapshot.db";
const MANIFEST_MAX: u64 = 64 * 1024;
const FORMAT: &str = "asher-ledger-backup-v1";

fn absolute(p: &Path) -> Result<PathBuf, Error> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}
/// Canonical form of a path whose last component may not exist yet.
fn resolved(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    match (
        p.parent().and_then(|x| x.canonicalize().ok()),
        p.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => p.to_path_buf(),
    }
}
/// A new directory path: absent, with a real parent, no symlinked ancestor, and not inside a
/// ledger root or a backup.
fn new_target(p: &Path) -> Result<PathBuf, Error> {
    let p = absolute(p)?;
    if fs::symlink_metadata(&p).is_ok() {
        return Err(Error::Rejected(format!(
            "target {} already exists; restore and backup only create a new, explicitly selected directory",
            p.display()
        )));
    }
    let parent = p
        .parent()
        .ok_or(Error::Invalid("target has no parent directory"))?;
    for a in parent.ancestors() {
        if fs::symlink_metadata(a)?.file_type().is_symlink() {
            return Err(Error::Invalid("symlink in target path"));
        }
        // A WAL or journal without its main file still marks a (damaged) ledger root.
        for marker in [
            "ledger.db",
            "ledger.db-wal",
            "ledger.db-journal",
            MANIFEST,
            SNAPSHOT,
        ] {
            if fs::symlink_metadata(a.join(marker)).is_ok() {
                return Err(Error::Rejected(format!(
                    "target {} lies inside {} (it holds {marker}); a target must be separate from every ledger and backup",
                    p.display(),
                    a.display()
                )));
            }
        }
    }
    if !fs::metadata(parent)?.is_dir() {
        return Err(Error::Invalid("target parent is not a directory"));
    }
    Ok(resolved(&p))
}
/// Neither path may contain the other.
fn separate(a: &Path, b: &Path, what: &str) -> Result<(), Error> {
    let (a, b) = (resolved(a), resolved(b));
    if a.starts_with(&b) || b.starts_with(&a) {
        return Err(Error::Rejected(format!(
            "target must be separate from {what}: neither may contain the other"
        )));
    }
    Ok(())
}
fn private_dir(p: &Path) -> Result<(), Error> {
    fs::DirBuilder::new().mode(0o700).create(p)?;
    Ok(())
}
fn sync_dir(p: &Path) -> Result<(), Error> {
    File::open(p)?.sync_all()?;
    Ok(())
}
fn sha256_file(p: &Path) -> Result<String, Error> {
    let mut b = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)?
        .read_to_end(&mut b)?;
    Ok(crate::artifact::sha256_hex(&b))
}
fn create_private(p: &Path) -> Result<(), Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    Ok(())
}
fn rename_noreplace(from: &Path, to: &Path) -> Result<(), Error> {
    let f = CString::new(from.as_os_str().as_bytes()).map_err(|_| Error::Invalid("path"))?;
    let t = CString::new(to.as_os_str().as_bytes()).map_err(|_| Error::Invalid("path"))?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// SQLite online backup of `src` (main) into `dst` (main), in one step.
pub(crate) fn copy(src: &Connection, dst: &Connection) -> Result<(), Error> {
    unsafe {
        let b = ffi::sqlite3_backup_init(
            dst.handle(),
            c"main".as_ptr(),
            src.handle(),
            c"main".as_ptr(),
        );
        if b.is_null() {
            return Err(Error::Rejected("backup could not start".into()));
        }
        let step = ffi::sqlite3_backup_step(b, -1);
        let done = ffi::sqlite3_backup_finish(b);
        if step != ffi::SQLITE_DONE || done != ffi::SQLITE_OK {
            return Err(Error::Rejected(format!(
                "backup copy failed (step {step}, finish {done}); nothing is claimed"
            )));
        }
    }
    Ok(())
}

/// Order-independent digest of the schema (tables, indexes, triggers) and every table row.
pub fn logical_digest(conn: &Connection) -> Result<(String, Value), Error> {
    let objects: Vec<(String, String, Option<String>)> = {
        let mut s = conn.prepare(
            "SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
        )?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut acc = String::new();
    let mut counts = serde_json::Map::new();
    for (kind, name, sql) in &objects {
        acc.push_str(&format!(
            "{kind} {name}\n{}\n",
            sql.as_deref().unwrap_or("")
        ));
        if kind != "table" {
            continue;
        }
        let mut s = conn.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))?;
        let n = s.column_count();
        let mut rows = s.query([])?;
        let mut c = 0i64;
        while let Some(r) = rows.next()? {
            c += 1;
            for i in 0..n {
                let v: rusqlite::types::Value = r.get(i)?;
                acc.push_str(&format!("{v:?}\u{1f}"));
            }
            acc.push('\n');
        }
        counts.insert(name.clone(), json!(c));
    }
    Ok((
        crate::artifact::sha256_hex(acc.as_bytes()),
        Value::Object(counts),
    ))
}

/// Verified read-only view of a single-file (non-WAL) snapshot; no lock, no sidecar file. The
/// full integrity check (index content against table rows) must pass, not only the quick one.
fn verify_immutable(path: &Path) -> Result<Connection, Error> {
    crate::storage::prepare();
    let c = store::open_immutable(path)?;
    store::check(&c)?;
    full_integrity(&c, "the backup snapshot")?;
    Ok(c)
}
/// Refuse unless `PRAGMA integrity_check` reports `ok`; the refusal names what it found.
fn full_integrity(c: &Connection, what: &str) -> Result<(), Error> {
    let rows = store::integrity(c)?;
    if rows != ["ok"] {
        return Err(Error::Rejected(format!(
            "{what} fails PRAGMA integrity_check ({}); refused",
            rows.join("; ")
        )));
    }
    Ok(())
}

/// Verified backup of a ledger into a new directory. Never writes the source's content.
pub fn backup(state: &Path, dest: &Path) -> Result<Value, Error> {
    let dest = new_target(dest)?;
    let state_abs = resolved(&absolute(state)?);
    separate(&state_abs, &dest, "the source ledger")?;
    let reader = store::Reader::open(state)?; // validates the root, full integrity and schema
    let src = reader.conn();
    // A backup is only as good as its source: refuse one whose indexes disagree with its rows.
    full_integrity(src, "the source ledger")?;
    let epoch: Option<i64> = src
        .query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))
        .ok();
    let source_schema: i64 = src.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
    private_dir(&dest)?;
    let db = dest.join(SNAPSHOT);
    // The copy is built under a staged, non-ledger name and becomes `snapshot.db` only once it
    // carries its marker and is verified: an interrupted or failed backup never leaves an
    // unmarked copy under the name a backup is known by.
    let staged = dest.join(format!(
        ".snapshot-{}.partial",
        uuid::Uuid::new_v4().simple()
    ));
    let snapshot_id = uuid::Uuid::new_v4().to_string();
    let created_at = store::now_ms()?;
    let built = (|| -> Result<(i64, String, Value), Error> {
        create_private(&staged)?;
        {
            let mut dst = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            copy(src, &dst)?;
            crate::store::barrier("after_backup_copy")?;
            // The copy (never the source) carries the current schema and its snapshot marker,
            // so every ledger ever derived from it is inspect-only.
            store::migrate(&mut dst)?;
            let tx = dst.transaction()?;
            tx.execute(
                "INSERT INTO ledger_snapshot VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    snapshot_id,
                    state_abs.to_string_lossy(),
                    source_schema,
                    created_at
                ],
            )?;
            crate::storage::commit_point(tx, crate::storage::Point::Backup)?;
            // The copied header carries the source's WAL mode; a backup is one self-contained
            // file.
            let mode: String = dst.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
            if mode != "delete" {
                return Err(Error::Rejected(
                    "backup could not be made a single file".into(),
                ));
            }
        }
        let mut side = staged.as_os_str().to_owned();
        for s in ["-wal", "-journal", "-shm"] {
            side.push(s);
            if fs::symlink_metadata(Path::new(&side)).is_ok() {
                return Err(Error::Rejected(format!(
                    "the backup copy left a {s} file; not verified"
                )));
            }
            side = staged.as_os_str().to_owned();
        }
        File::open(&staged)?.sync_all()?;
        let check = verify_immutable(&staged)?;
        let schema: i64 = check.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
        let marked: i64 =
            check.query_row("SELECT count(*) FROM ledger_snapshot", [], |r| r.get(0))?;
        if marked == 0 {
            return Err(Error::Rejected(
                "the backup copy carries no snapshot marker".into(),
            ));
        }
        let (digest, counts) = logical_digest(&check)?;
        drop(check);
        rename_noreplace(&staged, &db)?;
        sync_dir(&dest)?;
        Ok((schema, digest, counts))
    })();
    let (schema, digest, counts) = match built {
        Ok(x) => x,
        Err(e) => {
            let mut side = staged.as_os_str().to_owned();
            let _ = fs::remove_file(&staged);
            for s in ["-wal", "-journal", "-shm"] {
                side.push(s);
                let _ = fs::remove_file(Path::new(&side));
                side = staged.as_os_str().to_owned();
            }
            // Only removes the directory this call created, and only if it is empty.
            let _ = fs::remove_dir(&dest);
            return Err(Error::Rejected(format!(
                "backup failed; no verified snapshot exists (the partial copy was removed): {e}"
            )));
        }
    };
    drop(reader);
    let manifest = json!({
        "format": FORMAT,
        "created_at": created_at,
        "snapshot_id": snapshot_id,
        "source_root": state_abs,
        "source_schema": source_schema,
        "schema_version": schema,
        "sqlite_version": rusqlite::version(),
        "ledger_file": SNAPSHOT,
        "ledger_sha256": sha256_file(&db)?,
        "ledger_bytes": fs::metadata(&db)?.len(),
        "logical_digest": digest,
        "row_counts": counts,
        "coordinator_epoch": epoch,
        "not_included": ["artifact snapshots and fixture repositories (restored rows name root-relative or original paths)",
                         "owner credential", "emergency log"],
        "limits": "a local backup does not survive host or disk loss; anything after this snapshot is unknown to a restore of it; the manifest is not authenticated against a same-user writer",
    });
    let body = serde_json::to_vec_pretty(&manifest)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest.join(MANIFEST))?;
    f.write_all(&body)?;
    f.sync_all()?;
    sync_dir(&dest)?;
    if let Some(p) = dest.parent() {
        sync_dir(p)?;
    }
    Ok(
        json!({"backup": dest, "manifest": manifest, "source_content_written": false,
              "note": "the source was read through a read-only connection; when it had no WAL, SQLite may have created its empty WAL and shared-memory files"}),
    )
}

/// Restore a verified backup into a new, separate target, inspect-only.
pub fn restore(backup_dir: &Path, target: &Path) -> Result<Value, Error> {
    let target = new_target(target)?;
    let backup_dir = resolved(&absolute(backup_dir)?);
    separate(&backup_dir, &target, "the backup")?;
    let mm = fs::symlink_metadata(backup_dir.join(MANIFEST))?;
    if !mm.is_file() || mm.len() > MANIFEST_MAX {
        return Err(Error::Invalid("backup manifest missing or oversize"));
    }
    let mut body = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(backup_dir.join(MANIFEST))?
        .read_to_end(&mut body)?;
    let manifest: Value = serde_json::from_slice(&body)?;
    if manifest["format"] != FORMAT || manifest["ledger_file"] != SNAPSHOT {
        return Err(Error::Invalid("not an Asher ledger backup manifest"));
    }
    let source_root = manifest["source_root"].as_str().unwrap_or("").to_string();
    if !source_root.is_empty() {
        separate(
            Path::new(&source_root),
            &target,
            "the backup's source ledger",
        )?;
    }
    let src_db = backup_dir.join(SNAPSHOT);
    let sm = fs::symlink_metadata(&src_db)?;
    if !sm.is_file() || sm.nlink() != 1 {
        return Err(Error::Invalid("backup snapshot is not a regular file"));
    }
    for side in ["-wal", "-journal"] {
        if fs::symlink_metadata(backup_dir.join(format!("{SNAPSHOT}{side}"))).is_ok() {
            return Err(Error::Rejected(
                "the backup has a journal beside it: it was opened for writing after it was made and no longer matches its manifest".into(),
            ));
        }
    }
    let file_sha = sha256_file(&src_db)?;
    if Some(file_sha.as_str()) != manifest["ledger_sha256"].as_str() {
        return Err(Error::Rejected(
            "backup snapshot digest differs from its manifest; refusing to restore".into(),
        ));
    }
    let src = verify_immutable(&src_db)?;
    let (digest, _) = logical_digest(&src)?;
    if Some(digest.as_str()) != manifest["logical_digest"].as_str() {
        return Err(Error::Rejected(
            "backup content differs from its manifest; refusing to restore".into(),
        ));
    }
    let schema: i64 = src.query_row("SELECT version FROM schema_meta", [], |r| r.get(0))?;
    let marked: i64 = if schema >= 7 {
        src.query_row("SELECT count(*) FROM ledger_snapshot", [], |r| r.get(0))?
    } else {
        0
    };
    if marked == 0 {
        return Err(Error::Rejected(
            "not a backup snapshot (no snapshot marker); refusing to restore".into(),
        ));
    }

    // Build beside the target; the selected target appears only once complete and verified.
    let name = target
        .file_name()
        .ok_or(Error::Invalid("target has no name"))?
        .to_string_lossy()
        .to_string();
    let staging = target.with_file_name(format!(
        ".{name}.restoring-{}",
        uuid::Uuid::new_v4().simple()
    ));
    private_dir(&staging)?;
    let built = (|| -> Result<Value, Error> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(staging.join("coordinator.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| Error::Owned)?;
        let db = staging.join("ledger.db");
        create_private(&db)?;
        {
            let dst = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            copy(&src, &dst)?;
        }
        File::open(&db)?.sync_all()?;
        let mut conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        store::check(&conn)?;
        let integrity = store::integrity(&conn)?;
        if integrity != ["ok"] {
            return Err(Error::Rejected(format!(
                "the restored copy fails PRAGMA integrity_check ({}); refused",
                integrity.join("; ")
            )));
        }
        let (copy_digest, counts) = logical_digest(&conn)?;
        if copy_digest != digest {
            return Err(Error::Rejected(
                "restored copy differs from the verified backup".into(),
            ));
        }
        store::configure(&conn)?;
        store::migrate(&mut conn)?;
        let epoch: i64 = conn
            .query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))
            .unwrap_or(0);
        let now = store::now_ms()?;
        // Read-only look at the artifact snapshots the restored rows reference. Nothing is
        // invalidated here, and a restored ledger never runs the artifact rules itself.
        let artifacts: Vec<(String, String)> = {
            let mut s =
                conn.prepare("SELECT id,content_path FROM artifact ORDER BY sealed_at,id")?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let locate = |p: &str| {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                Path::new(&source_root).join(p)
            }
        };
        let missing: Vec<&String> = artifacts
            .iter()
            .filter(|(_, p)| fs::symlink_metadata(locate(p)).is_err())
            .map(|(id, _)| id)
            .collect();
        let verification = json!({
            "manifest_matches": true, "snapshot_sha256": file_sha, "logical_digest": digest,
            "copy_logical_digest": copy_digest,
            "integrity": integrity.join("; "), "integrity_check": "full (PRAGMA integrity_check) on the backup snapshot and on the restored copy",
            "foreign_keys": "ok (PRAGMA foreign_key_check)",
            "row_counts": counts, "backup_schema": schema, "restored_schema": store::CURRENT_SCHEMA,
            "artifacts": {"referenced": artifacts.len(), "missing_at_source_root": missing,
                          "resolved_against": source_root,
                          "note": "snapshots are not part of the backup; the restored ledger never applies artifact rules"},
        });
        conn.execute(
            "INSERT INTO ledger_restore VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                String::from_utf8_lossy(&body),
                crate::artifact::sha256_hex(&body),
                file_sha,
                source_root,
                manifest["created_at"].as_i64().unwrap_or(0),
                schema,
                now,
                verification.to_string(),
            ],
        )?;
        crate::store::barrier("during_restore")?;
        let run = crate::recovery::begin(&mut conn, epoch, crate::recovery::Trigger::Restore)?;
        let summary = match crate::recovery::finish(&mut conn, &staging, &run) {
            Ok(s) => s,
            Err(e) => {
                crate::recovery::fail(&mut conn, &run, &e);
                return Err(e);
            }
        };
        let status = crate::recovery::status(&conn)?;
        drop(conn);
        File::open(&db)?.sync_all()?;
        sync_dir(&staging)?;
        rename_noreplace(&staging, &target)?;
        if let Some(p) = target.parent() {
            sync_dir(p)?;
        }
        drop(lock);
        Ok(json!({
            "restored": target, "from_backup": backup_dir, "verification": verification,
            "restore_run": summary, "open_cases": status["cases"], "open_case_count": status["open_cases"],
            "fence": {"ordinary_startup": "refused", "launch": "refused", "dispatch": "refused"},
            "inspect": "diagnose, readback, list, get, inbox and check are read-only; recover records an accounting pass only",
            "replay_authorized": false, "resume_authorized": false, "consequential_actions": "none",
            "meaning": "the restored ledger reflects the backup only; obligations whose external progress after the backup cannot be shown stay open as recovery cases",
        }))
    })();
    built.map_err(|e| {
        Error::Rejected(format!(
            "restore did not complete; the selected target was not created and the partial copy was left at {} (inspect-only, never a state to resume): {e}",
            staging.display()
        ))
    })
}

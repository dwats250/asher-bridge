//! Single-session supervisor: inherited local-role channel, facts only, no domain workers.
use crate::{
    Error,
    execution::{FakeMode, Launch, Spec, check_permit, fixture_path, open_ledger},
    process_scope::{self, Identity},
    store::{barrier, now_ms},
};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
/// State root of this supervisor process, for best-effort emergency records.
static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
/// S2: one lifecycle fact, its own autocommit. A failure is classified (`storage`); callers
/// keep their existing behaviour (cleanup never waits for SQLite; unrecorded facts stay
/// volatile and the attempt is later projected conservatively). An uncertain outcome also
/// leaves a best-effort emergency record, because the caller does not report it further.
fn fact(c: &Connection, id: &str, kind: &str, payload: serde_json::Value) -> Result<(), Error> {
    // An unreadable wall clock must not lose a lifecycle fact: the stamp falls back to the
    // durable floor. Stamps never authorize.
    let at = crate::store::stamp_ms(c);
    let r = crate::storage::autocommit(crate::storage::Boundary::S2, id, kind, || {
        c.execute(
            "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES(?1,?2,?3,?4)",
            params![id, kind, at, payload.to_string()],
        )
    });
    if let (Err(Error::Storage(f)), Some(root)) = (&r, ROOT.get())
        && f.durability != crate::storage::Durability::NotCommitted
    {
        match crate::recovery::record_incident(root, f) {
            Ok(i) => eprintln!("supervisor: {f}; emergency record {i} (not a durable receipt)"),
            Err(why) => eprintln!("supervisor: {f}; emergency record not written: {why}"),
        }
    }
    r?;
    Ok(())
}
/// S2: one explicit evidence gap, its own autocommit, like a lifecycle fact.
fn gap(c: &Connection, id: &str, kind: &str, detail: serde_json::Value) -> Result<(), Error> {
    let at = crate::store::stamp_ms(c);
    crate::storage::autocommit(crate::storage::Boundary::S2, id, kind, || {
        c.execute(
            "INSERT INTO evidence_gap(attempt_id,kind,detail_json,at) VALUES(?1,?2,?3,?4)",
            params![id, kind, detail.to_string(), at],
        )
    })?;
    Ok(())
}
fn peer(stream: &UnixStream) -> Result<i32, Error> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    unsafe {
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if cred.uid != libc::geteuid() {
            return Err(Error::Invalid("wrong local role uid"));
        }
    }
    Ok(cred.pid)
}
fn authorized(c: &Connection, m: &Launch, pid: i32, root: &Path) -> Result<(), Error> {
    let owner_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("coordinator.lock"))?;
    match owner_lock.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&owner_lock)?;
            return Err(Error::Invalid("coordinator ownership released"));
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
        Err(e) => return Err(e.into()),
    }
    let epoch: i64 = c.query_row("SELECT epoch FROM coordinator_epoch", [], |r| r.get(0))?;
    if epoch != m.epoch {
        return Err(Error::Invalid("stale coordinator epoch"));
    }
    let (expected, start, boot): (i32, String, String) = c.query_row(
        "SELECT pid,start,boot FROM owner_session WHERE epoch=?1",
        [m.epoch],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let i = Identity::read(pid)?;
    if pid != expected || i.start != start || i.boot != boot || !i.live() {
        return Err(Error::Invalid("coordinator identity absent"));
    }
    Ok(())
}
fn disconnected(s: &mut UnixStream) -> Result<bool, Error> {
    let mut b = [0; 1];
    match s.read(&mut b) {
        Ok(0) => Ok(true),
        Ok(_) => Err(Error::Invalid("unexpected command after typed launch")),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e.into()),
    }
}
#[cfg(feature = "fault-injection")]
fn fail(point: &str) -> Result<(), Error> {
    if std::env::var("ASHER_TEST_FAIL").ok().as_deref() == Some(point) {
        return Err(Error::Invalid("injected partial startup failure"));
    }
    Ok(())
}
#[cfg(not(feature = "fault-injection"))]
fn fail(_: &str) -> Result<(), Error> {
    Ok(())
}
struct Scope {
    child: Child,
    identity: Identity,
    known: Vec<Identity>,
    finished: bool,
}
impl Drop for Scope {
    fn drop(&mut self) {
        if !self.finished {
            let _ = process_scope::stop(&mut self.child, &self.identity, &self.known);
        }
    }
}
pub fn run(root: &Path) -> Result<(), Error> {
    let _ = ROOT.set(root.to_path_buf());
    // fd 0 must be the inherited Unix socket. stdin text / arbitrary callers are refused.
    let mut channel = unsafe { UnixStream::from_raw_fd(0) };
    let pid = peer(&channel)?;
    channel.set_read_timeout(Some(Duration::from_secs(2)))?;
    channel.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut body = Vec::new();
    let request_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = request_deadline
            .checked_duration_since(Instant::now())
            .ok_or(Error::Invalid("launch request deadline"))?;
        channel.set_read_timeout(Some(remaining))?;
        let mut byte = [0];
        if channel.read(&mut byte)? == 0 {
            return Err(Error::Invalid("missing launch"));
        }
        if byte[0] == b'\n' {
            break;
        }
        body.push(byte[0]);
        if body.len() > 4096 {
            return Err(Error::Invalid("oversize launch"));
        }
    }
    let m: Launch = serde_json::from_slice(&body)?;
    if m.version != 1
        || m.role != "coordinator"
        || !crate::protocol::valid_id(&m.attempt_id)
        || m.command_id != format!("launch:{}", m.attempt_id)
    {
        return Err(Error::Invalid("invalid local launch command"));
    }
    let mut conn = open_ledger(root, crate::execution::Access::Act("supervisor launch"))?;
    // Receipt replay is historical, but caller identity/role/epoch must still be current.
    authorized(&conn, &m, pid, root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("supervisor.lock"))?;
    lock.try_lock_exclusive().map_err(|_| Error::Owned)?;
    let hash: String = conn.query_row(
        "SELECT spec_hash FROM attempt WHERE id=?1",
        [&m.attempt_id],
        |r| r.get(0),
    )?;
    if hash != m.fingerprint {
        return Err(Error::Conflict);
    }
    let old: Option<String> = conn
        .query_row(
            "SELECT fingerprint FROM supervisor_command WHERE request_id=?1",
            [&m.command_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        if old != m.fingerprint {
            return Err(Error::Conflict);
        }
        let latest:Option<(String,String)>=conn.query_row("SELECT kind,payload_json FROM supervisor_fact WHERE attempt_id=?1 ORDER BY seq DESC LIMIT 1",[&m.attempt_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        writeln!(
            channel,
            "{}",
            json!({"existing":m.attempt_id,"durable":true,"latest_fact":latest,"work_success":false})
        )?;
        return Ok(());
    }
    let state: String = conn.query_row(
        "SELECT state FROM attempt WHERE id=?1",
        [&m.attempt_id],
        |r| r.get(0),
    )?;
    if state != "intent" {
        return Err(Error::Invalid(
            "only a new intent may launch; reconcile unknown activity",
        ));
    }
    check_permit(&conn, &m.attempt_id, m.epoch)?;
    let spec: Spec = serde_json::from_str(&conn.query_row::<String, _, _>(
        "SELECT spec_json FROM attempt WHERE id=?1",
        [&m.attempt_id],
        |r| r.get(0),
    )?)?;
    spec.validate()?;
    let fixture = fixture_path(&conn, &m.attempt_id)?;
    let myself = Identity::current()?;
    // S1: the receipt commits before any child exists; the child is created only after
    // COMMIT returned OK, so an unknown commit outcome never creates a process.
    let mut g = crate::storage::Guard::new(crate::storage::Boundary::S1, &m.attempt_id);
    let accepted = (|| -> Result<(), Error> {
        barrier("before_supervisor_accept")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorized(&tx, &m, pid, root)?;
        check_permit(&tx, &m.attempt_id, m.epoch)?;
        crate::execution::launch_fence(&tx, &m.attempt_id)?;
        tx.execute(
            "INSERT INTO supervisor_command VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                m.command_id,
                m.attempt_id,
                m.fingerprint,
                m.epoch,
                myself.pid,
                myself.start,
                myself.boot,
                now_ms()?
            ],
        )?;
        tx.execute(
            "INSERT INTO supervisor_fact(attempt_id,kind,at,payload_json) VALUES(?1,?2,?3,?4)",
            params![
                m.attempt_id,
                "launch_accepted",
                now_ms()?,
                json!({"scope":"cooperative_group"}).to_string()
            ],
        )?;
        g.commit(tx)
    })();
    g.finish(accepted)?;
    writeln!(
        channel,
        "{}",
        json!({"accepted":m.attempt_id,"durable":true})
    )?;
    barrier("after_supervisor_accept")?;
    channel.set_nonblocking(true)?;
    authorized(&conn, &m, pid, root)?;
    if disconnected(&mut channel)? {
        return Err(Error::Invalid("owner absent before spawn"));
    }
    let mut cmd = Command::new(std::env::current_exe()?.with_file_name("fake-agent"));
    cmd.arg("wrapper")
        .arg(serde_json::to_string(&spec)?)
        .arg(myself.pid.to_string())
        .current_dir(&fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    process_scope::scrub(&mut cmd, &fixture);
    let mut child = cmd.spawn()?;
    // Child handle is owned before any fallible setup. Failure to obtain identity still kills/reaps it.
    let identity = match Identity::read(child.id() as i32) {
        Ok(i) => i,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let mut scope = Scope {
        child,
        identity,
        known: Vec::new(),
        finished: false,
    };
    conn.busy_timeout(Duration::from_millis(50))?;
    let mut escaped = false;
    let mut exit_observed = false;
    let mut released = false;
    let mut output_bytes = 0usize;
    // Bytes actually written to captured.log; with output_bytes it accounts for truncation.
    let mut captured_bytes = 0usize;
    // SHA-256 of exactly the bytes handed to captured.log, taken here at the producer from the
    // in-memory stream and never by reading the file back. It is recorded in cleanup_completed
    // so later validation can tell the captured output from anything that replaced it.
    let mut capture_digest = Sha256::new();
    let mut truncated = false;
    let mut pipe_eof = false;
    let result = (|| -> Result<(), Error> {
        #[cfg(feature = "fault-injection")]
        if let Some(dir) = std::env::var_os("ASHER_TEST_BARRIER_DIR") {
            std::fs::write(
                Path::new(&dir).join("wrapper_identity.json"),
                serde_json::to_vec(&scope.identity)?,
            )?;
        }
        barrier("after_wrapper_create")?;
        fail("reader_setup")?;
        let mut output = scope
            .child
            .stdout
            .take()
            .ok_or(Error::Invalid("missing output pipe"))?;
        process_scope::nonblocking(&output)?;
        fail("writer_setup")?;
        let mut input = scope
            .child
            .stdin
            .take()
            .ok_or(Error::Invalid("missing control pipe"))?;
        fail("worker_setup")?;
        let mut capture = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.join("captured.log"))?;
        // Startup, runtime and drain budgets are boot-local CLOCK_BOOTTIME: suspended time is
        // charged to them. All readings come from this one process within one boot.
        let elapsed_since = |t: u64| -> Result<Duration, Error> {
            Ok(Duration::from_millis(
                crate::host::boot_ms()?.saturating_sub(t),
            ))
        };
        let startup = crate::host::boot_ms()?;
        let mut exited_at = None;
        let mut began = None;
        let mut buffer = Vec::new();
        let mut ready = false;
        loop {
            let mut bytes = [0; 4096];
            match output.read(&mut bytes) {
                Ok(0) => pipe_eof = true,
                Ok(n) => {
                    let remaining = (spec.output_limit as usize).saturating_sub(output_bytes);
                    capture.write_all(&bytes[..n.min(remaining)])?;
                    capture_digest.update(&bytes[..n.min(remaining)]);
                    captured_bytes += n.min(remaining);
                    output_bytes += n;
                    if output_bytes > spec.output_limit as usize {
                        truncated = true;
                        return Err(Error::Invalid("output limit exceeded; capture truncated"));
                    }
                    buffer.extend_from_slice(&bytes[..n]);
                    while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<_> = buffer.drain(..=end).collect();
                        let v: serde_json::Value = serde_json::from_slice(&line)?;
                        match v["kind"].as_str() {
                            Some("ready") => {
                                let i: Identity = serde_json::from_value(v["identity"].clone())?;
                                if ready || i != scope.identity {
                                    return Err(Error::Invalid("wrapper identity mismatch"));
                                }
                                fact(
                                    &conn,
                                    &m.attempt_id,
                                    "starting",
                                    json!({"identity":i,"output_ready":true}),
                                )?;
                                barrier("before_release")?;
                                authorized(&conn, &m, pid, root)?;
                                check_permit(&conn, &m.attempt_id, m.epoch)?;
                                if disconnected(&mut channel)? {
                                    return Err(Error::Invalid("owner absent before release"));
                                }
                                input.write_all(b"R")?;
                                released = true;
                                began = Some(crate::host::boot_ms()?);
                                ready = true;
                                barrier("after_release")?;
                            }
                            Some("started") => {
                                if !released {
                                    return Err(Error::Invalid("workload started before release"));
                                }
                                fact(
                                    &conn,
                                    &m.attempt_id,
                                    "started",
                                    json!({"identity":scope.identity}),
                                )?;
                                writeln!(
                                    channel,
                                    "{}",
                                    json!({"started":m.attempt_id,"durable":true})
                                )?;
                            }
                            Some("descendant") => {
                                let i: Identity = serde_json::from_value(v["identity"].clone())?;
                                // Fixed fake protocol; verify live identity rather than trusting a PID in text.
                                if Identity::read(i.pid)? != i {
                                    return Err(Error::Invalid("descendant identity mismatch"));
                                }
                                escaped |= i.group != scope.identity.group;
                                scope.known.push(i.clone()); // retain cleanup handle even if persistence fails
                                fact(
                                    &conn,
                                    &m.attempt_id,
                                    "descendant_observed",
                                    json!({"identity":i,"escaped":escaped}),
                                )?;
                            }
                            Some("output") => (),
                            _ => return Err(Error::Invalid("invalid fake output")),
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e.into()),
            }
            if let Some(status) = scope.child.try_wait()? {
                if !exit_observed {
                    fact(
                        &conn,
                        &m.attempt_id,
                        "parent_exit_observed",
                        json!({"identity":scope.identity,"status":status.to_string()}),
                    )?;
                    exit_observed = true;
                    exited_at = Some(crate::host::boot_ms()?);
                    barrier("after_parent_exit")?;
                }
                if pipe_eof {
                    break;
                }
                // Do not bypass deadlines/control checks while a descendant holds stdout.
            }
            if disconnected(&mut channel)? {
                return Err(Error::Invalid("coordinator channel closed"));
            }
            authorized(&conn, &m, pid, root)?;
            if let Some(t) = exited_at
                && elapsed_since(t)? > Duration::from_millis(200)
            {
                return Err(Error::Invalid("post-exit output drain deadline"));
            }
            check_permit(&conn, &m.attempt_id, m.epoch)?;
            if (!ready && elapsed_since(startup)? > Duration::from_secs(2))
                || match began {
                    Some(t) => elapsed_since(t)? > Duration::from_millis(spec.runtime_ms),
                    None => false,
                }
            {
                return Err(Error::Invalid("startup/runtime deadline reached"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Drain finite remaining parent output; known descendants are stopped below.
        let mut b = [0; 4096];
        match output.read(&mut b) {
            Ok(0) => pipe_eof = true,
            // Late output is captured and counted like any other, never silently dropped.
            Ok(n) => {
                let remaining = (spec.output_limit as usize).saturating_sub(output_bytes);
                capture.write_all(&b[..n.min(remaining)])?;
                capture_digest.update(&b[..n.min(remaining)]);
                captured_bytes += n.min(remaining);
                output_bytes += n;
                truncated |= output_bytes > spec.output_limit as usize;
            }
            Err(_) => (),
        }
        drop(input);
        drop(output);
        Ok(())
    })();
    let reason = result
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "parent exited; cleanup required".into());
    // Output beyond the limit was observed but not captured: an explicit durable gap, so the
    // capture can never be read as complete. Recorded before the stop request; a failure to
    // record it leaves the run non-durable (never a clean result).
    let gap_durable = !truncated
        || gap(
            &conn,
            &m.attempt_id,
            "output_truncated",
            json!({"output_limit":spec.output_limit,"bytes_observed":output_bytes,
                   "bytes_captured":captured_bytes}),
        )
        .is_ok();
    // Any persistence failure here must not skip in-memory cleanup.
    let mut durable = fact(
        &conn,
        &m.attempt_id,
        "stop_requested",
        json!({"reason":reason,"released":released}),
    )
    .is_ok();
    durable &= gap_durable;
    let (empty, forced) = process_scope::stop(&mut scope.child, &scope.identity, &scope.known);
    scope.finished = true;
    if forced {
        durable &= fact(
            &conn,
            &m.attempt_id,
            "force_stop_sent",
            json!({"scope":"known identities"}),
        )
        .is_ok();
    }
    if !exit_observed {
        if let Ok(Some(status)) = scope.child.try_wait() {
            durable &= fact(
                &conn,
                &m.attempt_id,
                "parent_exit_observed",
                json!({"identity":scope.identity,"status":status.to_string()}),
            )
            .is_ok();
            exit_observed = true;
        }
    }
    // Escape-mode is deliberately always incomplete, even if the known escape was stopped.
    escaped |= spec.mode == FakeMode::Escape;
    if empty {
        durable &= fact(&conn,&m.attempt_id,"stop_confirmed",json!({"scope":"cooperative_group_and_known_identities","census_empty":true,"whole_descendants":false,"parent_exit_observed":exit_observed,"pipe_eof_observed":pipe_eof})).is_ok();
    }
    if empty && !escaped && durable && exit_observed {
        durable &= fact(&conn,&m.attempt_id,"cleanup_completed",json!({"scope":"fixed_cooperative_fake","output_closed":true,"bytes_observed":output_bytes,"bytes_captured":captured_bytes,"capture_sha256":format!("{:x}", capture_digest.clone().finalize()),"output_truncated":truncated,"work_success":false})).is_ok();
    } else {
        durable &= fact(&conn,&m.attempt_id,"unknown",json!({"reason":reason,"escaped":escaped,"empty_supported_scope":empty,"notification_delivery":false})).is_ok();
    }
    if !durable {
        eprintln!(
            "VOLATILE lifecycle observations for {}: SQLite unavailable; intent remains unresolved",
            m.attempt_id
        );
    }
    let _ = writeln!(
        channel,
        "{}",
        json!({"finished":true,"durable":durable,"work_success":false})
    );
    Ok(())
}

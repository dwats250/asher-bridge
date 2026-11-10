//! Test-only SQLite VFS that injects a single storage failure at a named transaction boundary.
//!
//! Compiled only with the `fault-injection` feature and registered only when
//! `ASHER_TEST_STORAGE_FAULT=<boundary>:<op>:<error>[:<nth>]` is set (for example `T1:wal_sync:ioerr`;
//! `nth` selects the Nth armed commit of that boundary in the process, default 1). Besides the
//! documented boundaries, `EPOCH`, `MIGRATE` and `BACKUP` name the internal commits of
//! `storage::Point`.
//! It wraps the platform's default VFS and forwards every call. While the named boundary's
//! commit is armed (see `storage::Guard`), the first matching WAL operation fails once with a
//! real SQLite error code, so SQLite's own commit path, not a mock of ours, produces the fault.
//!
//! * `wal_write:full` / `wal_write:ioerr`: the WAL write returns `SQLITE_FULL` /
//!   `SQLITE_IOERR_WRITE` without writing. Nothing of the transaction reaches the WAL.
//! * `wal_sync:ioerr`: the frames (including the commit frame) are written to the WAL file, but
//!   the fsync reports `SQLITE_IOERR_FSYNC`. SQLite rolls back in memory and reports failure, yet
//!   the frames stay in the file: whether the transaction later appears depends on whether the
//!   process closes normally or crashes. This is the genuinely uncertain commit.
use rusqlite::ffi;
use std::{
    ffi::{c_char, c_int, c_void},
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    WalWrite,
    WalSync,
}
struct Rule {
    boundary: u8,
    op: Op,
    code: c_int,
    /// Fail at the Nth armed commit of the boundary in this process (default 1).
    nth: u32,
}
static RULE: OnceLock<Option<Rule>> = OnceLock::new();
static INSTALLED: OnceLock<bool> = OnceLock::new();
/// Boundary index whose commit is in progress on this process, or 0.
static ARMED: AtomicU8 = AtomicU8::new(0);
static FIRED: AtomicBool = AtomicBool::new(false);
/// Armed commits of the rule's boundary seen so far.
static SEEN: AtomicU32 = AtomicU32::new(0);

fn rule() -> Option<&'static Rule> {
    RULE.get_or_init(|| {
        let spec = std::env::var("ASHER_TEST_STORAGE_FAULT").ok()?;
        // A malformed rule must fail loudly: a seam that silently injects nothing would let a
        // fault test pass without exercising the fault.
        Some(parse(&spec).unwrap_or_else(|| {
            panic!("invalid ASHER_TEST_STORAGE_FAULT {spec:?}: <T1|T4|T5|T6|T7|S1|S2|EPOCH|MIGRATE|BACKUP>:<wal_write|wal_sync>:<full|ioerr>[:nth]")
        }))
    })
    .as_ref()
}
fn parse(spec: &str) -> Option<Rule> {
    let mut it = spec.split(':');
    let name = it.next()?;
    let boundary = match crate::storage::Boundary::parse(name) {
        Some(b) => b.index(),
        None => crate::storage::Point::parse(name)?.index(),
    };
    let op = match it.next()? {
        "wal_write" => Op::WalWrite,
        "wal_sync" => Op::WalSync,
        _ => return None,
    };
    let code = match (op, it.next()?) {
        (Op::WalWrite, "full") => ffi::SQLITE_FULL,
        (Op::WalWrite, "ioerr") => ffi::SQLITE_IOERR_WRITE,
        (Op::WalSync, "ioerr") => ffi::SQLITE_IOERR_FSYNC,
        _ => return None,
    };
    let nth = match it.next() {
        None => 1,
        Some(n) => n.parse().ok().filter(|n| *n > 0)?,
    };
    if it.next().is_some() {
        return None;
    }
    Some(Rule {
        boundary,
        op,
        code,
        nth,
    })
}

/// Register the wrapper as the default VFS before the first connection, if a rule is set.
pub(crate) fn install() {
    INSTALLED.get_or_init(|| rule().is_some() && unsafe { register() });
}
pub(crate) fn arm(boundary: u8) {
    if rule().is_some_and(|r| r.boundary == boundary) {
        SEEN.fetch_add(1, Ordering::SeqCst);
    }
    ARMED.store(boundary, Ordering::SeqCst);
}
pub(crate) fn disarm() {
    ARMED.store(0, Ordering::SeqCst);
}
fn inject(kind: c_int, op: Op) -> Option<c_int> {
    let r = rule()?;
    if r.op != op
        || kind & ffi::SQLITE_OPEN_WAL == 0
        || ARMED.load(Ordering::SeqCst) != r.boundary
        || SEEN.load(Ordering::SeqCst) != r.nth
    {
        return None;
    }
    if FIRED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(r.code)
}

#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    real: *mut ffi::sqlite3_file,
    kind: c_int,
}
const HEADER: usize = (std::mem::size_of::<File>() + 15) & !15;

unsafe fn parent(vfs: *mut ffi::sqlite3_vfs) -> *mut ffi::sqlite3_vfs {
    unsafe { (*vfs).pAppData as *mut ffi::sqlite3_vfs }
}
unsafe fn real(f: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    unsafe { (*(f as *mut File)).real }
}
unsafe fn methods(f: *mut ffi::sqlite3_file) -> &'static ffi::sqlite3_io_methods {
    unsafe { &*(*real(f)).pMethods }
}

unsafe fn register() -> bool {
    unsafe {
        let p = ffi::sqlite3_vfs_find(ptr::null());
        if p.is_null() {
            return false;
        }
        let v = Box::leak(Box::new(ffi::sqlite3_vfs {
            iVersion: 2,
            szOsFile: (HEADER + (*p).szOsFile as usize) as c_int,
            mxPathname: (*p).mxPathname,
            pNext: ptr::null_mut(),
            zName: c"asher-fault".as_ptr(),
            pAppData: p as *mut c_void,
            xOpen: Some(x_open),
            xDelete: Some(x_delete),
            xAccess: Some(x_access),
            xFullPathname: Some(x_full_pathname),
            // The platform implementations ignore their VFS argument; forward them unchanged.
            xDlOpen: (*p).xDlOpen,
            xDlError: (*p).xDlError,
            xDlSym: (*p).xDlSym,
            xDlClose: (*p).xDlClose,
            xRandomness: Some(x_randomness),
            xSleep: Some(x_sleep),
            xCurrentTime: Some(x_current_time),
            xGetLastError: Some(x_get_last_error),
            xCurrentTimeInt64: Some(x_current_time_int64),
            xSetSystemCall: None,
            xGetSystemCall: None,
            xNextSystemCall: None,
        }));
        ffi::sqlite3_vfs_register(v, 1) == ffi::SQLITE_OK
    }
}

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let p = parent(vfs);
        let f = file as *mut File;
        let r = (file as *mut u8).add(HEADER) as *mut ffi::sqlite3_file;
        (*f).base.pMethods = ptr::null();
        (*f).real = r;
        (*f).kind = flags;
        (*r).pMethods = ptr::null();
        let rc = ((*p).xOpen.expect("parent xOpen"))(p, name, r, flags, out);
        if !(*r).pMethods.is_null() {
            (*f).base.pMethods = &IO;
        }
        rc
    }
}
unsafe extern "C" fn x_delete(vfs: *mut ffi::sqlite3_vfs, z: *const c_char, s: c_int) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xDelete.expect("parent xDelete"))(p, z, s)
    }
}
unsafe extern "C" fn x_access(
    vfs: *mut ffi::sqlite3_vfs,
    z: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xAccess.expect("parent xAccess"))(p, z, flags, out)
    }
}
unsafe extern "C" fn x_full_pathname(
    vfs: *mut ffi::sqlite3_vfs,
    z: *const c_char,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xFullPathname.expect("parent xFullPathname"))(p, z, n, out)
    }
}
unsafe extern "C" fn x_randomness(vfs: *mut ffi::sqlite3_vfs, n: c_int, out: *mut c_char) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xRandomness.expect("parent xRandomness"))(p, n, out)
    }
}
unsafe extern "C" fn x_sleep(vfs: *mut ffi::sqlite3_vfs, us: c_int) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xSleep.expect("parent xSleep"))(p, us)
    }
}
unsafe extern "C" fn x_current_time(vfs: *mut ffi::sqlite3_vfs, t: *mut f64) -> c_int {
    unsafe {
        let p = parent(vfs);
        ((*p).xCurrentTime.expect("parent xCurrentTime"))(p, t)
    }
}
unsafe extern "C" fn x_get_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let p = parent(vfs);
        match (*p).xGetLastError {
            Some(f) => f(p, n, out),
            None => 0,
        }
    }
}
unsafe extern "C" fn x_current_time_int64(
    vfs: *mut ffi::sqlite3_vfs,
    t: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let p = parent(vfs);
        match (*p).xCurrentTimeInt64 {
            Some(f) => f(p, t),
            None => {
                let mut d = 0f64;
                let rc = x_current_time(vfs, &mut d);
                *t = (d * 86_400_000.0) as ffi::sqlite3_int64;
                rc
            }
        }
    }
}

static IO: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(f_close),
    xRead: Some(f_read),
    xWrite: Some(f_write),
    xTruncate: Some(f_truncate),
    xSync: Some(f_sync),
    xFileSize: Some(f_file_size),
    xLock: Some(f_lock),
    xUnlock: Some(f_unlock),
    xCheckReservedLock: Some(f_check_reserved_lock),
    xFileControl: Some(f_file_control),
    xSectorSize: Some(f_sector_size),
    xDeviceCharacteristics: Some(f_device_characteristics),
    xShmMap: Some(f_shm_map),
    xShmLock: Some(f_shm_lock),
    xShmBarrier: Some(f_shm_barrier),
    xShmUnmap: Some(f_shm_unmap),
    xFetch: Some(f_fetch),
    xUnfetch: Some(f_unfetch),
};

unsafe extern "C" fn f_close(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let rc = (methods(f).xClose.expect("xClose"))(real(f));
        (*f).pMethods = ptr::null();
        rc
    }
}
unsafe extern "C" fn f_read(
    f: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    n: c_int,
    off: ffi::sqlite3_int64,
) -> c_int {
    unsafe { (methods(f).xRead.expect("xRead"))(real(f), buf, n, off) }
}
unsafe extern "C" fn f_write(
    f: *mut ffi::sqlite3_file,
    buf: *const c_void,
    n: c_int,
    off: ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        if let Some(code) = inject((*(f as *mut File)).kind, Op::WalWrite) {
            return code;
        }
        (methods(f).xWrite.expect("xWrite"))(real(f), buf, n, off)
    }
}
unsafe extern "C" fn f_truncate(f: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    unsafe { (methods(f).xTruncate.expect("xTruncate"))(real(f), size) }
}
unsafe extern "C" fn f_sync(f: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    unsafe {
        if let Some(code) = inject((*(f as *mut File)).kind, Op::WalSync) {
            return code;
        }
        (methods(f).xSync.expect("xSync"))(real(f), flags)
    }
}
unsafe extern "C" fn f_file_size(f: *mut ffi::sqlite3_file, out: *mut ffi::sqlite3_int64) -> c_int {
    unsafe { (methods(f).xFileSize.expect("xFileSize"))(real(f), out) }
}
unsafe extern "C" fn f_lock(f: *mut ffi::sqlite3_file, l: c_int) -> c_int {
    unsafe { (methods(f).xLock.expect("xLock"))(real(f), l) }
}
unsafe extern "C" fn f_unlock(f: *mut ffi::sqlite3_file, l: c_int) -> c_int {
    unsafe { (methods(f).xUnlock.expect("xUnlock"))(real(f), l) }
}
unsafe extern "C" fn f_check_reserved_lock(f: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe { (methods(f).xCheckReservedLock.expect("xCheckReservedLock"))(real(f), out) }
}
unsafe extern "C" fn f_file_control(
    f: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    unsafe { (methods(f).xFileControl.expect("xFileControl"))(real(f), op, arg) }
}
unsafe extern "C" fn f_sector_size(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe { (methods(f).xSectorSize.expect("xSectorSize"))(real(f)) }
}
unsafe extern "C" fn f_device_characteristics(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        (methods(f)
            .xDeviceCharacteristics
            .expect("xDeviceCharacteristics"))(real(f))
    }
}
unsafe extern "C" fn f_shm_map(
    f: *mut ffi::sqlite3_file,
    pg: c_int,
    sz: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        match methods(f).xShmMap {
            Some(m) => m(real(f), pg, sz, extend, out),
            None => ffi::SQLITE_IOERR,
        }
    }
}
unsafe extern "C" fn f_shm_lock(
    f: *mut ffi::sqlite3_file,
    off: c_int,
    n: c_int,
    fl: c_int,
) -> c_int {
    unsafe {
        match methods(f).xShmLock {
            Some(m) => m(real(f), off, n, fl),
            None => ffi::SQLITE_IOERR,
        }
    }
}
unsafe extern "C" fn f_shm_barrier(f: *mut ffi::sqlite3_file) {
    unsafe {
        if let Some(m) = methods(f).xShmBarrier {
            m(real(f))
        }
    }
}
unsafe extern "C" fn f_shm_unmap(f: *mut ffi::sqlite3_file, del: c_int) -> c_int {
    unsafe {
        match methods(f).xShmUnmap {
            Some(m) => m(real(f), del),
            None => ffi::SQLITE_OK,
        }
    }
}
unsafe extern "C" fn f_fetch(
    f: *mut ffi::sqlite3_file,
    off: ffi::sqlite3_int64,
    n: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        match methods(f).xFetch {
            Some(m) if methods(f).iVersion >= 3 => m(real(f), off, n, out),
            _ => {
                *out = ptr::null_mut();
                ffi::SQLITE_OK
            }
        }
    }
}
unsafe extern "C" fn f_unfetch(
    f: *mut ffi::sqlite3_file,
    off: ffi::sqlite3_int64,
    p: *mut c_void,
) -> c_int {
    unsafe {
        match methods(f).xUnfetch {
            Some(m) if methods(f).iVersion >= 3 => m(real(f), off, p),
            _ => ffi::SQLITE_OK,
        }
    }
}

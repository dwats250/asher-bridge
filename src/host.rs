//! Host facts that do not survive a reboot. This module is the only reader of the kernel boot
//! identity and of per-process kernel stat used for process identity.
//!
//! Test seams exist only under the `fault-injection` feature, so a test can SIMULATE a reboot
//! or PID reuse; ordinary builds contain no environment lookup here:
//! - `ASHER_TEST_BOOT_ID_FILE`: the trimmed contents replace the kernel boot id. Re-read on
//!   every call, so rewriting the file "reboots" the next reader.
//! - `ASHER_TEST_PROC_STAT_DIR`: a file `<dir>/<pid>` replaces `/proc/<pid>/stat` for that PID.
//! - `ASHER_TEST_BOOTTIME_OFFSET_FILE`: milliseconds added to CLOCK_BOOTTIME, so a test can
//!   SIMULATE a suspend (with the wall offset seam in `store`) or boot-local time passing.
use crate::Error;
use std::{fs, io};

const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// The identity of the running boot. Every comparison against a recorded boot goes through
/// this function.
pub fn boot_id() -> Result<String, Error> {
    #[cfg(feature = "fault-injection")]
    if let Some(p) = std::env::var_os("ASHER_TEST_BOOT_ID_FILE") {
        return Ok(fs::read_to_string(p)?.trim().to_string());
    }
    Ok(fs::read_to_string(BOOT_ID)?.trim().to_string())
}

/// `/proc/<pid>/stat` for process identity and state.
pub(crate) fn proc_stat(pid: i32) -> io::Result<String> {
    #[cfg(feature = "fault-injection")]
    if let Some(d) = std::env::var_os("ASHER_TEST_PROC_STAT_DIR") {
        let p = std::path::Path::new(&d).join(pid.to_string());
        if p.exists() {
            return fs::read_to_string(p);
        }
    }
    fs::read_to_string(format!("/proc/{pid}/stat"))
}

/// Boot-local elapsed time in milliseconds from the suspend-aware monotonic clock
/// (CLOCK_BOOTTIME): it keeps counting while the host is suspended and restarts at reboot.
/// A value is meaningful only against another value read under the same `boot_id()`; it is
/// never compared across boots or stored as a persistent clock.
pub fn boot_ms() -> Result<u64, Error> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes only the timespec it is given.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let ms = u64::try_from(ts.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .and_then(|s| s.checked_add(u64::try_from(ts.tv_nsec).ok()? / 1_000_000))
        .ok_or(Error::Invalid("boot clock out of range"))?;
    #[cfg(feature = "fault-injection")]
    if let Some(p) = std::env::var_os("ASHER_TEST_BOOTTIME_OFFSET_FILE")
        && let Ok(v) = fs::read_to_string(p)
    {
        let add: u64 = v
            .trim()
            .parse()
            .map_err(|_| Error::Invalid("bad test boot clock offset"))?;
        return ms
            .checked_add(add)
            .ok_or(Error::Invalid("boot clock out of range"));
    }
    Ok(ms)
}

/// Environment keys a spawned helper (supervisor) must inherit so it reads the same simulated
/// host (boot, process stat, wall and boot-local clocks) as its parent. Empty in ordinary builds.
/// `ASHER_TEST_EXTRA_OUTPUT` makes the fake symbol preparation exceed its output limit.
#[cfg(feature = "fault-injection")]
pub(crate) const TEST_SEAMS: &[&str] = &[
    "ASHER_TEST_BOOT_ID_FILE",
    "ASHER_TEST_PROC_STAT_DIR",
    "ASHER_TEST_BOOTTIME_OFFSET_FILE",
    "ASHER_TEST_CLOCK_OFFSET_FILE",
    "ASHER_TEST_EXTRA_OUTPUT",
];

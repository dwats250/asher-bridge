//! Linux cooperative identities. Group census is deliberately not isolation.
use crate::Error;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::fd::AsRawFd,
    process::{Child, Command},
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub pid: i32,
    pub start: String,
    pub boot: String,
    pub group: i32,
}
impl Identity {
    pub fn current() -> Result<Self, Error> {
        Self::read(std::process::id() as i32)
    }
    pub fn read(pid: i32) -> Result<Self, Error> {
        let stat = crate::host::proc_stat(pid)?;
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or(Error::Invalid("bad process stat"))?
            .1
            .split_whitespace()
            .collect();
        Ok(Self {
            pid,
            start: fields
                .get(19)
                .ok_or(Error::Invalid("missing process start"))?
                .to_string(),
            boot: crate::host::boot_id()?,
            group: fields
                .get(2)
                .ok_or(Error::Invalid("missing process group"))?
                .parse()
                .map_err(|_| Error::Invalid("bad group"))?,
        })
    }
    /// Whether this identity was recorded under an earlier boot. Decided from the boot
    /// identity alone, before any `/proc` read: no process survives a reboot, and a PID of an
    /// earlier boot names nothing in this one. `None` when the current boot is unreadable.
    pub fn from_earlier_boot(&self) -> Option<bool> {
        crate::host::boot_id().ok().map(|b| b != self.boot)
    }
    pub fn live(&self) -> bool {
        match self.from_earlier_boot() {
            // Stale: never probed, never live, never a PID-based inference.
            Some(true) => return false,
            Some(false) => (),
            None => return true, // uncertainty is never proof of absence
        }
        match Self::read(self.pid) {
            Ok(i) => self.same_process(&i) && !zombie(self.pid),
            Err(Error::Io(e)) if process_absent(&e) => false,
            Err(_) => true, // uncertainty is never proof of absence
        }
    }
    fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start == other.start && self.boot == other.boot
    }
    fn anchors_group(&self, group: i32) -> bool {
        self.from_earlier_boot() == Some(false)
            && Self::read(self.pid).is_ok_and(|current| {
                self.same_process(&current) && current.group == group && !zombie(current.pid)
            })
    }
    pub fn signal(&self, sig: i32) {
        // Never signal an identity from an earlier boot, or when the boot is unknown.
        if self.from_earlier_boot() != Some(false) {
            return;
        }
        // Open a stable kernel handle first, then validate identity. A recycled PID
        // cannot redirect the later signal to another process.
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_open, self.pid, 0) as i32;
            if fd < 0 {
                return;
            }
            if Self::read(self.pid).is_ok_and(|i| self.same_process(&i)) {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            libc::close(fd);
        }
    }
}
fn process_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}
fn zombie(pid: i32) -> bool {
    crate::host::proc_stat(pid)
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, t)| t.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}
pub fn group_members(group: i32) -> Result<Vec<Identity>, Error> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        match Identity::read(pid) {
            Ok(i) if i.group == group && !zombie(pid) => members.push(i),
            Ok(_) => (),
            Err(Error::Io(e)) if process_absent(&e) => (),
            Err(e) => return Err(e),
        }
    }
    Ok(members)
}
pub fn nonblocking(fd: &impl AsRawFd) -> Result<(), Error> {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}
pub fn scrub(command: &mut Command, home: &std::path::Path) {
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    // A simulated host (boot identity, process stat) must be the same for every process of an
    // attempt, or a helper would record identities of a different "boot".
    #[cfg(feature = "fault-injection")]
    for key in crate::host::TEST_SEAMS {
        if let Some(v) = std::env::var_os(key) {
            command.env(key, v);
        }
    }
}
/// Stop only identities discovered while this owned group is alive. Each signal is
/// rechecked against boot/start identity; no broad name kill or reused-PID kill.
pub fn stop(child: &mut Child, leader: &Identity, known: &[Identity]) -> (bool, bool) {
    let mut tracked = known.to_vec();
    tracked.push(leader.clone());
    let mut complete = true;
    // Group number alone is not durable identity. Enroll members only while a
    // known original member establishes continuity of this attempt's group.
    let anchored = tracked.iter().any(|i| i.anchors_group(leader.group));
    match group_members(leader.group) {
        Ok(v) if anchored => tracked.extend(v),
        Ok(v) if v.is_empty() => (),
        _ => complete = false,
    }
    for p in &tracked {
        p.signal(libc::SIGTERM);
    }
    let until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < until && tracked.iter().any(Identity::live) {
        let _ = child.try_wait();
        std::thread::sleep(Duration::from_millis(5));
    }
    let forced = tracked.iter().any(Identity::live);
    for p in &tracked {
        p.signal(libc::SIGKILL);
    }
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until && tracked.iter().any(Identity::live) {
        let _ = child.try_wait();
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.try_wait();
    let empty = group_members(leader.group).is_ok_and(|v| v.is_empty());
    (
        complete && empty && tracked.iter().all(|i| !i.live()),
        forced,
    )
}
/// Inherited-descriptor protocol only: deny creation/use of new network sockets.
/// Not filesystem isolation; supported only for the fixed fake on Linux x86_64.
pub fn deny_network() -> io::Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        const LD: u16 = 0x20;
        const JEQ: u16 = 0x15;
        const RET: u16 = 0x06;
        let mut code = vec![
            libc::sock_filter {
                code: LD,
                jt: 0,
                jf: 0,
                k: 4,
            },
            libc::sock_filter {
                code: JEQ,
                jt: 1,
                jf: 0,
                k: 0xc000003e,
            },
            libc::sock_filter {
                code: RET,
                jt: 0,
                jf: 0,
                k: 0x80000000,
            },
            libc::sock_filter {
                code: LD,
                jt: 0,
                jf: 0,
                k: 0,
            },
        ];
        for nr in [libc::SYS_socket, libc::SYS_socketpair, libc::SYS_connect] {
            code.push(libc::sock_filter {
                code: JEQ,
                jt: 0,
                jf: 1,
                k: nr as u32,
            });
            code.push(libc::sock_filter {
                code: RET,
                jt: 0,
                jf: 0,
                k: 0x00050000 | libc::EPERM as u32,
            });
        }
        code.push(libc::sock_filter {
            code: RET,
            jt: 0,
            jf: 0,
            k: 0x7fff0000,
        });
        let program = libc::sock_fprog {
            len: code.len() as u16,
            filter: code.as_mut_ptr(),
        };
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_SECCOMP, 2, &program) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Err(io::Error::other(
            "fake network restriction requires Linux x86_64",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{Identity, process_absent};
    #[test]
    fn vanished_proc_entry_is_absent_but_access_failure_is_unknown() {
        assert!(process_absent(&std::io::Error::from_raw_os_error(
            libc::ENOENT
        )));
        assert!(process_absent(&std::io::Error::from_raw_os_error(
            libc::ESRCH
        )));
        assert!(!process_absent(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!process_absent(&std::io::Error::from_raw_os_error(
            libc::EIO
        )));
    }
    #[test]
    fn group_anchor_requires_current_membership_not_stored_group() {
        let mut identity = Identity::current().unwrap();
        let actual = identity.group;
        identity.group = actual + 100000;
        assert!(identity.live(), "group change is not process death");
        assert!(
            !identity.anchors_group(identity.group),
            "stale group cannot establish continuity"
        );
        assert!(identity.anchors_group(actual));
    }
}

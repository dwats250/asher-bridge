//! Process identity under PID reuse, within one boot and across a (SIMULATED) reboot.
//! The fault-injection seams `ASHER_TEST_BOOT_ID_FILE` and `ASHER_TEST_PROC_STAT_DIR` present a
//! different boot identity or a different kernel stat for a named PID; nothing is rebooted and
//! no PID is actually recycled. These tests set process environment, so they live in their own
//! test binary and serialize on one lock.
#![cfg(feature = "fault-injection")]
use asher_bridge::process_scope::Identity;
use std::{
    fs,
    path::Path,
    process::{Child, Command},
    sync::Mutex,
};

static ENV: Mutex<()> = Mutex::new(());

struct Seams {
    _dir: tempfile::TempDir,
}
impl Seams {
    fn new(boot: &str) -> (Self, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("boot_id"), format!("{boot}\n")).unwrap();
        fs::create_dir(d.path().join("stat")).unwrap();
        unsafe {
            std::env::set_var("ASHER_TEST_BOOT_ID_FILE", d.path().join("boot_id"));
            std::env::set_var("ASHER_TEST_PROC_STAT_DIR", d.path().join("stat"));
        }
        let stat = d.path().join("stat");
        (Self { _dir: d }, stat)
    }
}
impl Drop for Seams {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("ASHER_TEST_BOOT_ID_FILE");
            std::env::remove_var("ASHER_TEST_PROC_STAT_DIR");
        }
    }
}
fn sleeper() -> Child {
    Command::new("/bin/sleep").arg("30").spawn().unwrap()
}
fn alive(c: &mut Child) -> bool {
    c.try_wait().unwrap().is_none()
}
/// The kernel stat of `pid` with field 22 (start time) replaced.
fn stat_with_start(stat_dir: &Path, pid: u32, start: &str) {
    let real = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (head, tail) = real.rsplit_once(')').unwrap();
    let mut fields: Vec<&str> = tail.split_whitespace().collect();
    fields[19] = start;
    fs::write(
        stat_dir.join(pid.to_string()),
        format!("{head}) {}", fields.join(" ")),
    )
    .unwrap();
}

#[test]
fn boot_seam_supplies_every_identity_boot() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (_s, _) = Seams::new("sim-boot-7");
    assert_eq!(Identity::current().unwrap().boot, "sim-boot-7");
}

#[test]
fn pid_reuse_within_boot_is_not_the_recorded_process() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = sleeper();
    let (_s, stat) = Seams::new("sim-boot-1");
    let recorded = Identity::read(child.id() as i32).unwrap();
    assert!(recorded.live());
    // The recorded process ended and its PID now names a later process of the same boot.
    let later: u64 = recorded.start.parse::<u64>().unwrap() + 1000;
    stat_with_start(&stat, child.id(), &later.to_string());
    assert!(
        !recorded.live(),
        "a reused PID read as the recorded process"
    );
    recorded.signal(libc::SIGKILL);
    assert!(alive(&mut child), "signal reached a reused PID");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn pid_reuse_across_boots_is_not_the_recorded_process() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = sleeper();
    let (s, _stat) = Seams::new("sim-boot-1");
    let recorded = Identity::read(child.id() as i32).unwrap();
    drop(s);
    // Reboot: the same PID with the same start ticks now belongs to a process of a new boot.
    let (_s, _stat) = Seams::new("sim-boot-2");
    assert!(!recorded.live(), "an earlier-boot identity read as live");
    recorded.signal(libc::SIGKILL);
    assert!(alive(&mut child), "signal reached an earlier-boot identity");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn earlier_boot_identity_is_never_signalled_or_probed() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = sleeper();
    let (_s, stat) = Seams::new("sim-boot-2");
    let mut recorded = Identity::read(child.id() as i32).unwrap();
    recorded.boot = "sim-boot-1".into();
    // Any /proc probe of this PID would now fail as unreadable, which liveness treats as
    // "may be live". An earlier-boot identity is classified before any probe: not live.
    fs::write(stat.join(child.id().to_string()), "unreadable").unwrap();
    assert!(
        !recorded.live(),
        "earlier-boot identity was probed or read as live"
    );
    recorded.signal(libc::SIGKILL);
    assert!(alive(&mut child));
    let _ = child.kill();
    let _ = child.wait();
}

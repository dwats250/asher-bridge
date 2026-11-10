//! Fixed offline workload. No shell, plugins, arbitrary paths or credential inputs.
use asher_bridge::{
    execution::{FakeMode, Spec},
    process_scope::{self, Identity},
};
use std::{
    io::{BufRead, Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn emit(v: serde_json::Value) -> std::io::Result<()> {
    println!("{v}");
    std::io::stdout().flush()
}
fn main() {
    if let Err(e) = run() {
        eprintln!("fake: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = std::env::args().collect();
    process_scope::deny_network()?;
    // Verify the installed restriction before announcing readiness.
    unsafe {
        let probe = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if probe >= 0 {
            libc::close(probe);
            return Err("network restriction unavailable".into());
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return Err("unexpected network probe result".into());
        }
    }
    if a.get(1).is_some_and(|s| s == "descendant") {
        if a.get(2).is_some_and(|s| s == "escape") {
            unsafe {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
        }
        emit(serde_json::json!({"kind":"descendant","identity":Identity::current()?}))?;
        // Self-limits survive supervisor and leader loss; they are cooperation, not containment.
        let end = Instant::now() + Duration::from_secs(4);
        while Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        return Ok(());
    }
    if a.len() != 4 || a[1] != "wrapper" {
        return Err("only typed wrapper entry is supported".into());
    }
    let spec: Spec = serde_json::from_str(&a[2])?;
    spec.validate()?;
    let parent: i32 = a[3].parse()?;
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::getppid() != parent {
            return Err("supervisor absent before handshake".into());
        }
    }
    process_scope::nonblocking(&std::io::stdin())?;
    emit(serde_json::json!({"kind":"ready","identity":Identity::current()?}))?;
    let end = Instant::now() + Duration::from_secs(3);
    let mut release = [0];
    loop {
        match std::io::stdin().read(&mut release) {
            Ok(1) if release[0] == b'R' => break,
            Ok(_) => return Err("release channel closed or invalid".into()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e.into()),
        }
        if Instant::now() > end {
            return Err("blocked-wrapper deadline".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if spec.mode == FakeMode::Ignore {
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    emit(serde_json::json!({"kind":"started"}))?;
    match spec.mode {
        FakeMode::Complete => {
            std::fs::write("candidate.txt", b"synthetic candidate; not validated\n")?;
        }
        FakeMode::PrepareSymbol => {
            // Deterministic preparation: changes only the disposable candidate's symbol.
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open("candidate/symbols.csv")?;
            f.write_all(b"symbol,scope\nFIXTURE_OK,core\n")?;
            f.sync_all()?;
            // Fault injection only: a preparation that then exceeds its output limit, so a
            // test can check that the truncation is a recorded gap and never a pass.
            #[cfg(feature = "fault-injection")]
            if std::env::var_os("ASHER_TEST_EXTRA_OUTPUT").is_some() {
                for _ in 0..64 {
                    emit(serde_json::json!({"kind":"output","data":"x".repeat(512)}))?;
                }
            }
        }
        FakeMode::Descendant | FakeMode::Escape => {
            let mut c = Command::new(std::env::current_exe()?);
            c.arg("descendant")
                .arg(if spec.mode == FakeMode::Escape {
                    "escape"
                } else {
                    "group"
                })
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            process_scope::scrub(&mut c, &std::env::current_dir()?);
            let mut child = c.spawn()?;
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().ok_or("missing descendant pipe")?)
                .read_line(&mut line)?;
            let v: serde_json::Value = serde_json::from_str(&line)?;
            emit(v)?;
            // Intentionally leave the bounded child for supervisor cleanup testing.
        }
        FakeMode::PipeHolder => {
            let mut c = Command::new(std::env::current_exe()?);
            c.arg("descendant")
                .arg("group")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::null());
            process_scope::scrub(&mut c, &std::env::current_dir()?);
            let child = c.spawn()?;
            emit(
                serde_json::json!({"kind":"descendant","identity":Identity::read(child.id() as i32)?}),
            )?;
        }
        FakeMode::Flood => {
            for _ in 0..10000 {
                emit(serde_json::json!({"kind":"output","data":"x".repeat(512)}))?;
            }
        }
        FakeMode::Hold | FakeMode::Ignore => {
            let end = Instant::now() + Duration::from_millis(spec.runtime_ms + 2000);
            while Instant::now() < end {
                match std::io::stdin().read(&mut release) {
                    Ok(0) if spec.mode == FakeMode::Ignore => (),
                    Ok(0) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(e.into()),
                    _ => return Err("unexpected control byte".into()),
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    Ok(())
}

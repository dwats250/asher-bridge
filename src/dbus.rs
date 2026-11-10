//! Local desktop notification backend: `org.freedesktop.Notifications.Notify` over the
//! session-bus Unix socket, in-process (no subprocess, no dependency).
//!
//! What its receipt proves: the notification daemon replied to `Notify` with a notification ID,
//! i.e. it accepted the request. That is NOT proof that anyone saw it, that it stayed on screen,
//! or that a human read it. A failure before the request was fully written is `Failed`; a
//! failure after the request was written but before a reply is `Unknown` (it may have shown).
use crate::attention::{Message, SendResult};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Remaining time before `deadline`, capped at the per-operation timeout; `None` once spent.
fn remaining(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then(|| left.min(IO_TIMEOUT))
}
/// Arm the socket for the next blocking operation, or report that the deadline is spent.
fn arm(s: &UnixStream, deadline: Instant) -> bool {
    match remaining(deadline) {
        Some(t) => s.set_read_timeout(Some(t)).is_ok() && s.set_write_timeout(Some(t)).is_ok(),
        None => false,
    }
}
/// Every read and write re-arms against the absolute deadline, so a bus trickling bytes one
/// at a time cannot stretch a single message read (or write) past it.
struct Bounded<'a> {
    s: &'a mut UnixStream,
    deadline: Instant,
}
impl Read for Bounded<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !arm(self.s, self.deadline) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.s.read(buf)
    }
}
impl Write for Bounded<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !arm(self.s, self.deadline) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.s.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.s.flush()
    }
}
/// Bounded connect: a non-blocking `connect(2)` on a Unix socket either completes at once or
/// fails (EAGAIN when the listener's backlog is full); it never waits. The descriptor is then
/// switched back to blocking mode with per-operation timeouts.
fn connect_bounded(path: &std::path::Path) -> std::io::Result<UnixStream> {
    use std::os::fd::FromRawFd;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: plain libc socket calls on a descriptor we own; sockaddr_un is zero-initialized
    // and the path length is checked against sun_path (leaving room for the terminator).
    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        if bytes.len() >= addr.sun_path.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session bus path too long",
            ));
        }
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, b) in addr.sun_path.iter_mut().zip(bytes) {
            *d = *b as libc::c_char;
        }
        let fd = libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let s = UnixStream::from_raw_fd(fd);
        let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        s.set_nonblocking(false)?;
        Ok(s)
    }
}

fn align(b: &mut Vec<u8>, n: usize) {
    while b.len() % n != 0 {
        b.push(0);
    }
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    align(b, 4);
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}
fn put_sig(b: &mut Vec<u8>, s: &str) {
    b.push(s.len() as u8);
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}
/// One header field `(byte code, variant)`; `kind` is the variant's single-type signature.
fn field(f: &mut Vec<u8>, code: u8, kind: char, value: &str) {
    align(f, 8);
    f.push(code);
    put_sig(f, &kind.to_string());
    match kind {
        'g' => put_sig(f, value),
        _ => put_str(f, value),
    }
}
/// Serialize a method call (little-endian, protocol version 1).
fn method_call(
    serial: u32,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    sig: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields = Vec::new();
    field(&mut fields, 1, 'o', path);
    field(&mut fields, 2, 's', iface);
    field(&mut fields, 3, 's', member);
    field(&mut fields, 6, 's', dest);
    if !sig.is_empty() {
        field(&mut fields, 8, 'g', sig);
    }
    let mut m = vec![b'l', 1, 0, 1];
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(&serial.to_le_bytes());
    m.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    m.extend_from_slice(&fields);
    align(&mut m, 8);
    m.extend_from_slice(body);
    m
}
fn notify_body(summary: &str, body: &str, urgency: u8, timeout_ms: i32) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, "Asher Bridge");
    put_u32(&mut b, 0); // replaces_id: never replace another notification
    put_str(&mut b, ""); // icon
    put_str(&mut b, summary);
    put_str(&mut b, body);
    put_u32(&mut b, 0); // actions: empty array of strings
    // hints a{sv} with one entry: "urgency" => byte
    let mut entries = Vec::new();
    put_str(&mut entries, "urgency");
    put_sig(&mut entries, "y");
    entries.push(urgency);
    put_u32(&mut b, entries.len() as u32);
    align(&mut b, 8);
    // entries were built from offset 0 (8-aligned), the same alignment as here
    b.extend_from_slice(&entries);
    put_u32(&mut b, timeout_ms as u32);
    b
}

/// A parsed incoming message: type, reply serial and error name (the only fields we need).
#[derive(Debug, Default)]
struct Incoming {
    kind: u8,
    reply_serial: Option<u32>,
    error: Option<String>,
    body: Vec<u8>,
}
fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}
fn read_message(s: &mut impl Read) -> std::io::Result<Incoming> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let mut head = [0u8; 16];
    s.read_exact(&mut head)?;
    if head[0] != b'l' || head[3] != 1 {
        return Err(bad("unsupported D-Bus message"));
    }
    let body_len = rd_u32(&head, 4).ok_or_else(|| bad("short header"))? as usize;
    let fields_len = rd_u32(&head, 12).ok_or_else(|| bad("short header"))? as usize;
    if fields_len > 65536 || body_len > 1 << 20 {
        return Err(bad("oversize D-Bus message"));
    }
    let mut fields = vec![0u8; fields_len];
    s.read_exact(&mut fields)?;
    let pad = (8 - (16 + fields_len) % 8) % 8;
    let mut skip = vec![0u8; pad];
    s.read_exact(&mut skip)?;
    let mut body = vec![0u8; body_len];
    s.read_exact(&mut body)?;
    let mut m = Incoming {
        kind: head[1],
        body,
        ..Default::default()
    };
    // Fields are 8-aligned relative to the message start; 16 is 8-aligned, so offsets into
    // `fields` align the same way.
    let mut i = 0;
    while i < fields.len() {
        i = (i + 7) & !7;
        if i + 3 > fields.len() {
            break;
        }
        let code = fields[i];
        let sl = fields[i + 1] as usize;
        let sig = fields.get(i + 2..i + 2 + sl).unwrap_or(&[]).to_vec();
        i += 2 + sl + 1;
        match sig.as_slice() {
            b"u" => {
                i = (i + 3) & !3;
                let v = rd_u32(&fields, i).ok_or_else(|| bad("short field"))?;
                if code == 5 {
                    m.reply_serial = Some(v);
                }
                i += 4;
            }
            b"s" | b"o" => {
                i = (i + 3) & !3;
                let l = rd_u32(&fields, i).ok_or_else(|| bad("short field"))? as usize;
                let text = fields.get(i + 4..i + 4 + l).unwrap_or(&[]);
                if code == 4 {
                    m.error = Some(String::from_utf8_lossy(text).into_owned());
                }
                i += 4 + l + 1;
            }
            b"g" => {
                let l = *fields.get(i).ok_or_else(|| bad("short field"))? as usize;
                i += 1 + l + 1;
            }
            _ => break,
        }
    }
    Ok(m)
}

fn session_bus_path() -> Result<PathBuf, String> {
    if let Ok(a) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        // Only a filesystem Unix socket is supported; abstract sockets and other transports are not.
        for part in a.split(';') {
            if let Some(rest) = part.strip_prefix("unix:") {
                for kv in rest.split(',') {
                    if let Some(p) = kv.strip_prefix("path=") {
                        return Ok(PathBuf::from(p));
                    }
                }
            }
        }
        return Err("DBUS_SESSION_BUS_ADDRESS has no supported unix:path= address".into());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let p = PathBuf::from(format!("/run/user/{uid}/bus"));
    if p.exists() {
        Ok(p)
    } else {
        Err("no desktop session bus found".into())
    }
}

/// Send one notification through the session bus at `path`, finishing by `deadline` whatever
/// the bus does (slow, silent or flooding unrelated messages). Expiry before the Notify request
/// is fully written is `Failed`; expiry after it is `Unknown` (it may have been shown).
pub(crate) fn notify_at(path: &std::path::Path, m: &Message, deadline: Instant) -> SendResult {
    const SPENT: &str = "notification time budget spent";
    let mut s = match connect_bounded(path) {
        Ok(s) => s,
        Err(e) => return SendResult::Failed(format!("desktop session unavailable: {e}")),
    };
    if !arm(&s, deadline) {
        return SendResult::Failed(format!("{SPENT} before the bus handshake"));
    }
    let mut s = Bounded {
        s: &mut s,
        deadline,
    };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
    let auth = format!("\0AUTH EXTERNAL {hex}\r\n");
    if let Err(e) = s.write_all(auth.as_bytes()) {
        return SendResult::Failed(format!("desktop bus handshake failed: {e}"));
    }
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if remaining(deadline).is_none() {
            return SendResult::Failed(format!("{SPENT} during authentication"));
        }
        match s.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            _ => return SendResult::Failed("desktop bus authentication did not complete".into()),
        }
        if line.len() > 512 {
            return SendResult::Failed("desktop bus authentication reply too long".into());
        }
    }
    if !line.starts_with(b"OK ") {
        return SendResult::Failed("desktop bus refused authentication".into());
    }
    if remaining(deadline).is_none() {
        return SendResult::Failed(format!("{SPENT} during authentication"));
    }
    if let Err(e) = s.write_all(b"BEGIN\r\n") {
        return SendResult::Failed(format!("desktop bus handshake failed: {e}"));
    }
    let hello = method_call(
        1,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "Hello",
        "",
        &[],
    );
    if let Err(e) = s.write_all(&hello) {
        return SendResult::Failed(format!("desktop bus hello failed: {e}"));
    }
    loop {
        if remaining(deadline).is_none() {
            return SendResult::Failed(format!("{SPENT} waiting for the bus hello reply"));
        }
        match read_message(&mut s) {
            Ok(r) if r.reply_serial == Some(1) => {
                if r.kind == 3 {
                    return SendResult::Failed(format!(
                        "desktop bus hello error: {}",
                        r.error.unwrap_or_default()
                    ));
                }
                break;
            }
            Ok(_) => continue, // signals such as NameAcquired
            Err(e) => return SendResult::Failed(format!("desktop bus hello reply failed: {e}")),
        }
    }
    let urgency = if m.severity == "urgent" { 2 } else { 1 };
    let body = notify_body(&m.summary, &m.body, urgency, 0);
    let call = method_call(
        2,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        "susssasa{sv}i",
        &body,
    );
    if remaining(deadline).is_none() {
        return SendResult::Failed(format!("{SPENT} before the notify request was sent"));
    }
    if let Err(e) = s.write_all(&call) {
        // The request may have been partly written: not provably undelivered.
        return SendResult::Unknown(format!("notify request write failed: {e}"));
    }
    loop {
        if remaining(deadline).is_none() {
            return SendResult::Unknown(format!(
                "{SPENT} with no reply to notify (it may or may not have been shown)"
            ));
        }
        match read_message(&mut s) {
            Ok(r) if r.reply_serial == Some(2) => {
                if r.kind == 3 {
                    return SendResult::Failed(format!(
                        "desktop notification daemon error: {}",
                        r.error.unwrap_or_default()
                    ));
                }
                return match rd_u32(&r.body, 0) {
                    Some(id) => SendResult::Accepted(json!({
                        "backend": "desktop", "notification_id": id,
                        "meaning": "the notification daemon accepted the request; this does not show a human saw it",
                    })),
                    None => SendResult::Unknown("notify reply had no notification id".into()),
                };
            }
            Ok(_) => continue,
            Err(e) => {
                return SendResult::Unknown(format!(
                    "no reply to notify (it may or may not have been shown): {e}"
                ));
            }
        }
    }
}
pub(crate) fn notify(m: &Message, deadline: Instant) -> SendResult {
    match session_bus_path() {
        Ok(p) => notify_at(&p, m, deadline),
        Err(e) => SendResult::Failed(format!("desktop session unavailable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fake session bus that checks the wire format the client sends: AUTH EXTERNAL, Hello,
    /// then Notify with the exact signature and body layout, answering with a notification ID.
    fn fake_bus(
        dir: &std::path::Path,
        reply_to_notify: bool,
        error: bool,
    ) -> (PathBuf, std::thread::JoinHandle<Vec<u8>>) {
        let path = dir.join("bus");
        let l = UnixListener::bind(&path).unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 1];
            while !got.ends_with(b"\r\n") {
                s.read_exact(&mut b).unwrap();
                got.push(b[0]);
            }
            assert!(got.starts_with(b"\0AUTH EXTERNAL "));
            s.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")
                .unwrap();
            let mut begin = [0u8; 7];
            s.read_exact(&mut begin).unwrap();
            assert_eq!(&begin, b"BEGIN\r\n");
            let hello = read_message(&mut s).unwrap();
            assert_eq!(hello.kind, 1);
            // Reply to Hello with a method_return carrying the unique name.
            let mut body = Vec::new();
            put_str(&mut body, ":1.7");
            s.write_all(&reply(1, 100, &body, false)).unwrap();
            // A signal before the Notify reply must be ignored by the client.
            let call = read_message(&mut s).unwrap();
            assert_eq!(call.kind, 1);
            let raw_body = call.body.clone();
            if reply_to_notify {
                let mut rb = Vec::new();
                put_u32(&mut rb, 42);
                s.write_all(&reply(2, 101, &rb, error)).unwrap();
            }
            raw_body
        });
        (path, h)
    }
    fn reply(reply_serial: u32, serial: u32, body: &[u8], error: bool) -> Vec<u8> {
        let mut fields = Vec::new();
        if error {
            field(
                &mut fields,
                4,
                's',
                "org.freedesktop.DBus.Error.ServiceUnknown",
            );
        }
        align(&mut fields, 8);
        fields.push(5);
        put_sig(&mut fields, "u");
        put_u32(&mut fields, reply_serial);
        if !body.is_empty() {
            field(&mut fields, 8, 'g', if error { "s" } else { "u" });
        }
        let mut m = vec![b'l', if error { 3 } else { 2 }, 0, 1];
        m.extend_from_slice(&(body.len() as u32).to_le_bytes());
        m.extend_from_slice(&serial.to_le_bytes());
        m.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        m.extend_from_slice(&fields);
        align(&mut m, 8);
        m.extend_from_slice(body);
        m
    }
    fn msg() -> Message {
        Message {
            attention_id: "att-1".into(),
            work_id: "w-1".into(),
            kind: "responsibility".into(),
            severity: "urgent".into(),
            ordinal: 1,
            summary: "Asher Bridge: test".into(),
            body: "body text\nattention att-1".into(),
        }
    }
    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    /// A bus that completes AUTH, then behaves as `after_auth` says.
    fn hostile_bus(
        dir: &std::path::Path,
        after_auth: &'static str,
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        let path = dir.join("bus");
        let l = UnixListener::bind(&path).unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let end = Instant::now() + Duration::from_secs(8);
            if after_auth == "silent_auth" {
                // Never answers AUTH at all; hold the connection open.
                while Instant::now() < end && s.write_all(b"").is_ok() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                return;
            }
            let mut got = Vec::new();
            let mut b = [0u8; 1];
            while !got.ends_with(b"\r\n") {
                if s.read_exact(&mut b).is_err() {
                    return;
                }
                got.push(b[0]);
            }
            let _ = s.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n");
            let mut begin = [0u8; 7];
            let _ = s.read_exact(&mut begin);
            let Ok(_hello) = read_message(&mut s) else {
                return;
            };
            let _ = s.write_all(&reply(1, 100, &[], false));
            let Ok(_call) = read_message(&mut s) else {
                return;
            };
            // Flood unrelated signals (or trickle one message byte by byte), never replying.
            let mut serial = 200u32;
            while Instant::now() < end {
                let mut sig = vec![b'l', 4, 0, 1, 0, 0, 0, 0];
                sig.extend_from_slice(&serial.to_le_bytes());
                sig.extend_from_slice(&[0, 0, 0, 0]);
                serial += 1;
                let ok = if after_auth == "trickle" {
                    sig.iter().all(|byte| {
                        std::thread::sleep(Duration::from_millis(150));
                        s.write_all(&[*byte]).is_ok()
                    })
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    s.write_all(&sig).is_ok()
                };
                if !ok {
                    return;
                }
            }
        });
        (path, h)
    }
    #[test]
    fn hostile_bus_cannot_outlast_the_notification_deadline() {
        for (mode, expect_unknown) in [("flood", true), ("trickle", true), ("silent_auth", false)] {
            let d = tempfile::tempdir().unwrap();
            let (path, _h) = hostile_bus(d.path(), mode);
            let start = Instant::now();
            let r = notify_at(&path, &msg(), start + Duration::from_millis(1200));
            let took = start.elapsed();
            assert!(took < Duration::from_millis(2500), "{mode}: took {took:?}");
            match r {
                SendResult::Unknown(_) => assert!(expect_unknown, "{mode}"),
                SendResult::Failed(_) => assert!(!expect_unknown, "{mode}"),
                SendResult::Accepted(_) => panic!("{mode}: no reply was sent"),
            }
        }
    }
    #[test]
    fn notify_body_has_the_documented_layout() {
        let b = notify_body("S", "B", 2, 0);
        // app_name "Asher Bridge" (12) + nul, replaces_id 0, icon "", summary, body.
        assert_eq!(&b[0..4], &12u32.to_le_bytes());
        assert_eq!(&b[4..16], b"Asher Bridge");
        assert_eq!(b[16], 0);
        assert_eq!(
            &b[20..24],
            &0u32.to_le_bytes(),
            "replaces_id is 0 and 4-aligned"
        );
        // The hints array length precedes 8-aligned dict entries; timeout is the last i32.
        assert_eq!(&b[b.len() - 4..], &0u32.to_le_bytes());
        assert_eq!(b.len() % 4, 0);
    }
    #[test]
    fn client_speaks_auth_hello_notify_and_reads_the_notification_id() {
        let d = tempfile::tempdir().unwrap();
        let (path, h) = fake_bus(d.path(), true, false);
        match notify_at(&path, &msg(), soon()) {
            SendResult::Accepted(v) => assert_eq!(v["notification_id"], 42),
            _ => panic!("expected acceptance"),
        }
        let body = h.join().unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Asher Bridge: test"));
        assert!(String::from_utf8_lossy(&body).contains("urgency"));
    }
    #[test]
    fn missing_reply_is_unknown_and_daemon_error_is_failed() {
        let d = tempfile::tempdir().unwrap();
        let (path, h) = fake_bus(d.path(), false, false);
        assert!(matches!(
            notify_at(&path, &msg(), soon()),
            SendResult::Unknown(_)
        ));
        h.join().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let (path2, h2) = fake_bus(d2.path(), true, true);
        assert!(matches!(
            notify_at(&path2, &msg(), soon()),
            SendResult::Failed(_)
        ));
        h2.join().unwrap();
        assert!(matches!(
            notify_at(
                std::path::Path::new("/nonexistent/asher-bus"),
                &msg(),
                soon()
            ),
            SendResult::Failed(_)
        ));
    }
}

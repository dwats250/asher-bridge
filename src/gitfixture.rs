//! Disposable fixture Git repository, written and updated IN-PROCESS. No `git` binary,
//! no repository scripts, hooks or inherited configuration are ever executed.
//!
//! Objects are written as Git loose objects (SHA-1 id, zlib with stored blocks). The ref
//! update uses Git's own lock protocol: create `<ref>.lock` with O_EXCL, compare the
//! current value with the exact expected old object ID, write the new ID, fsync and
//! rename over the ref. A real `git update-ref` racing this respects the same lock.
//! After the rename, one reflog line naming the effect is appended. The reflog is a
//! shared audit trail under exclusive fixture ownership; a same-user writer can rewrite
//! it, so it supports attribution only for this disposable fixture, never isolation.
use crate::Error;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub const REF_NAME: &str = "refs/heads/fixture";
const REF_REL: &str = "refs/heads/fixture";
const LOCK_REL: &str = "refs/heads/fixture.lock";
const LOG_REL: &str = "logs/refs/heads/fixture";
const ID_FILE: &str = "asher-fixture-id";
const MAX_LOG: u64 = 1 << 20;
pub const IDENT: &str = "Asher Fixture <fixture@invalid>";

// ---------- SHA-1 (Git object identity only; not a security primitive here) ----------
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
pub fn is_oid(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Deterministic zlib stream using only stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks: Vec<&[u8]> = data.chunks(65535).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i == last));
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for x in data {
        a = (a + u32::from(*x)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// A Git object: its id and the exact loose-object file bytes this module writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub id: String,
    file: Vec<u8>,
}
fn object(kind: &str, body: &[u8]) -> Object {
    let mut raw = format!("{kind} {}\0", body.len()).into_bytes();
    raw.extend_from_slice(body);
    Object {
        id: hex(&sha1(&raw)),
        file: zlib_stored(&raw),
    }
}
fn oid_bytes(id: &str) -> Vec<u8> {
    (0..20)
        .map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}
/// Tree of the fixed flat layout. Every file is a regular 100644 blob, sorted by name.
pub fn tree(files: &std::collections::BTreeMap<String, Vec<u8>>) -> (Object, Vec<Object>) {
    let mut body = Vec::new();
    let mut blobs = Vec::new();
    for (name, data) in files {
        let b = object("blob", data);
        body.extend_from_slice(format!("100644 {name}\0").as_bytes());
        body.extend_from_slice(&oid_bytes(&b.id));
        blobs.push(b);
    }
    (object("tree", &body), blobs)
}
pub fn commit(tree: &str, parent: Option<&str>, secs: i64, message: &str) -> Object {
    let mut s = format!("tree {tree}\n");
    if let Some(p) = parent {
        s.push_str(&format!("parent {p}\n"));
    }
    s.push_str(&format!(
        "author {IDENT} {secs} +0000\ncommitter {IDENT} {secs} +0000\n\n{message}\n"
    ));
    object("commit", s.as_bytes())
}

/// Recorded identity of the disposable repository. The nonce file is inside the repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub nonce: String,
}
fn excl(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}
fn sync_dir(p: &Path) -> io::Result<()> {
    File::open(p)?.sync_all()
}

/// Create a fresh bare repository below the state root. Never reuses an existing path.
/// Layout: HEAD, minimal config, empty hooks/, objects/, refs/heads/, an empty reflog
/// (so later writers append), and a nonce identity file. No remote and no credentials.
pub fn create(root: &Path, id: &str) -> Result<Repo, Error> {
    if !crate::protocol::valid_id(id) {
        return Err(Error::Invalid("invalid repository ID"));
    }
    let parent = root.join("repos");
    match fs::DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => sync_dir(root)?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(&parent)?;
    if !m.is_dir() || m.mode() & 0o077 != 0 {
        return Err(Error::Invalid(
            "repository parent must be a private directory",
        ));
    }
    let p = parent.join(id);
    fs::DirBuilder::new().mode(0o700).create(&p)?;
    for d in [
        "objects",
        "refs",
        "refs/heads",
        "refs/tags",
        "hooks",
        "logs",
        "logs/refs",
        "logs/refs/heads",
    ] {
        fs::DirBuilder::new().mode(0o700).create(p.join(d))?;
    }
    excl(&p.join("HEAD"), b"ref: refs/heads/fixture\n", 0o600)?;
    excl(
        &p.join("config"),
        b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n",
        0o600,
    )?;
    let nonce = uuid::Uuid::new_v4().to_string();
    excl(&p.join(ID_FILE), nonce.as_bytes(), 0o400)?;
    excl(&p.join(LOG_REL), b"", 0o600)?;
    for d in [
        "logs/refs/heads",
        "logs/refs",
        "logs",
        "refs/heads",
        "refs",
        "hooks",
        "objects",
    ] {
        sync_dir(&p.join(d))?;
    }
    sync_dir(&p)?;
    sync_dir(&parent)?;
    let m = fs::symlink_metadata(&p)?;
    Ok(Repo {
        path: p,
        dev: m.dev(),
        ino: m.ino(),
        nonce,
    })
}

/// Verify the recorded repository identity and that no unsupported state was added.
pub fn verify(repo: &Repo) -> Result<(), String> {
    let m = fs::symlink_metadata(&repo.path).map_err(|e| format!("repository unavailable: {e}"))?;
    if !m.is_dir() || m.dev() != repo.dev || m.ino() != repo.ino {
        return Err("repository identity changed (replaced or moved)".into());
    }
    let canonical = repo
        .path
        .canonicalize()
        .map_err(|e| format!("repository path unreadable: {e}"))?;
    if canonical != repo.path {
        return Err("repository path traverses a symlink".into());
    }
    let nonce = read_small(&repo.path.join(ID_FILE), 128)
        .map_err(|e| format!("repository nonce unreadable: {e}"))?;
    if nonce != repo.nonce.as_bytes() {
        return Err("repository nonce differs from recorded identity".into());
    }
    for unsupported in [
        "packed-refs",
        "commondir",
        "modules",
        "info/grafts",
        "shallow",
    ] {
        if fs::symlink_metadata(repo.path.join(unsupported)).is_ok() {
            return Err(format!("unsupported repository state: {unsupported}"));
        }
    }
    let hooks: Vec<_> = fs::read_dir(repo.path.join("hooks"))
        .map_err(|e| format!("hooks directory unreadable: {e}"))?
        .collect();
    if !hooks.is_empty() {
        return Err("repository hooks directory is not empty".into());
    }
    Ok(())
}
fn read_small(p: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut b = Vec::new();
    f.take(max + 1).read_to_end(&mut b)?;
    if b.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(b)
}

/// Write a loose object if absent; an existing object file must equal these exact bytes.
pub fn write_object(repo: &Repo, o: &Object) -> Result<(), Error> {
    let dir = repo.path.join("objects").join(&o.id[..2]);
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => sync_dir(&repo.path.join("objects"))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let target = dir.join(&o.id[2..]);
    match read_small(&target, 1 << 20) {
        Ok(b) if b == o.file => return Ok(()),
        Ok(_) => {
            return Err(Error::Rejected(format!(
                "object {} has unexpected bytes",
                o.id
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let tmp = dir.join(format!("tmp_obj_{}", uuid::Uuid::new_v4()));
    excl(&tmp, &o.file, 0o400)?;
    // link() never replaces an existing object; a concurrent identical write is harmless.
    let linked = fs::hard_link(&tmp, &target);
    fs::remove_file(&tmp)?;
    match linked {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if read_small(&target, 1 << 20)? != o.file {
                return Err(Error::Rejected(format!(
                    "object {} has unexpected bytes",
                    o.id
                )));
            }
        }
        Err(e) => return Err(e.into()),
    }
    sync_dir(&dir)?;
    Ok(())
}
/// True only if the object exists with exactly the bytes this module would write.
pub fn object_present(repo: &Repo, o: &Object) -> bool {
    read_small(
        &repo.path.join("objects").join(&o.id[..2]).join(&o.id[2..]),
        1 << 20,
    )
    .is_ok_and(|b| b == o.file)
}

/// Current loose ref value. Missing, symbolic, packed or malformed refs are errors.
pub fn read_ref(repo: &Repo) -> Result<String, String> {
    let b =
        read_small(&repo.path.join(REF_REL), 128).map_err(|e| format!("ref unreadable: {e}"))?;
    let s = std::str::from_utf8(&b).map_err(|_| "ref is not UTF-8".to_string())?;
    let v = s.strip_suffix('\n').unwrap_or(s);
    if !is_oid(v) {
        return Err("ref does not hold a plain object ID".into());
    }
    Ok(v.into())
}
/// Kernel identity of the loose ref file. Every rename-based ref update (this module's
/// and real Git's) installs a new inode, so a changed identity reveals a rewrite even
/// when the value and the audit log were restored.
pub fn ref_identity(repo: &Repo) -> Result<String, String> {
    let m = fs::symlink_metadata(repo.path.join(REF_REL))
        .map_err(|e| format!("ref unreadable: {e}"))?;
    Ok(format!(
        "{}:{}:{}.{}",
        m.dev(),
        m.ino(),
        m.ctime(),
        m.ctime_nsec()
    ))
}
/// Contents of a leftover ref lock, if any (None when absent).
pub fn lock_contents(repo: &Repo) -> Result<Option<String>, String> {
    match read_small(&repo.path.join(LOCK_REL), 128) {
        Ok(b) => Ok(Some(String::from_utf8_lossy(&b).trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("ref lock unreadable: {e}")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogEntry {
    pub old: String,
    pub new: String,
    pub message: String,
}
/// The reflog as raw bytes plus parsed entries. Malformed lines are an error.
pub fn read_log(repo: &Repo) -> Result<(Vec<u8>, Vec<LogEntry>), String> {
    let b = read_small(&repo.path.join(LOG_REL), MAX_LOG)
        .map_err(|e| format!("reflog unreadable: {e}"))?;
    let text = std::str::from_utf8(&b).map_err(|_| "reflog is not UTF-8".to_string())?;
    let mut out = Vec::new();
    for line in text.split_terminator('\n') {
        let (head, message) = line.split_once('\t').unwrap_or((line, ""));
        let mut parts = head.splitn(3, ' ');
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            return Err("malformed reflog line".into());
        };
        if !is_oid(old) || !is_oid(new) {
            return Err("malformed reflog object IDs".into());
        }
        out.push(LogEntry {
            old: old.into(),
            new: new.into(),
            message: message.into(),
        });
    }
    if !b.is_empty() && !b.ends_with(b"\n") {
        return Err("reflog ends with a partial line".into());
    }
    Ok((b, out))
}
pub fn effect_message(effect_id: &str) -> String {
    format!("asher-effect {effect_id}")
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "cas", rename_all = "snake_case")]
pub enum Cas {
    /// The rename happened: this operation changed the ref from old to new.
    Applied { reflog_appended: bool },
    /// The ref held another value; this operation changed nothing.
    Mismatch { current: String },
    /// Another writer holds the ref lock; this operation changed nothing.
    Locked,
    /// The final authorization recheck, made while holding the ref lock, refused; nothing changed.
    Aborted { reason: String },
}
/// Native compare-and-swap using Git's ref lock protocol. Exactly one rename at most.
///
/// `recheck` runs while the ref lock is held and the old value has been compared, i.e.
/// immediately before the update. It is NOT atomic with the rename: authority can still
/// change between the recheck and the rename, and no mechanism here can prevent that.
pub fn compare_and_swap(
    repo: &Repo,
    old: &str,
    new: &str,
    effect_id: &str,
    recheck: &mut dyn FnMut() -> Result<(), String>,
) -> Result<Cas, Error> {
    if !is_oid(old) || !is_oid(new) {
        return Err(Error::Invalid("invalid object IDs"));
    }
    let lock = repo.path.join(LOCK_REL);
    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(Cas::Locked),
        Err(e) => return Err(e.into()),
    };
    let current = match read_ref(repo) {
        Ok(c) => c,
        Err(e) => {
            drop(f);
            fs::remove_file(&lock)?;
            return Err(Error::Rejected(e));
        }
    };
    if current != old {
        drop(f);
        fs::remove_file(&lock)?;
        return Ok(Cas::Mismatch { current });
    }
    if let Err(reason) = recheck() {
        drop(f);
        fs::remove_file(&lock)?;
        return Ok(Cas::Aborted { reason });
    }
    crate::store::barrier("after_final_recheck")?;
    f.write_all(format!("{new}\n").as_bytes())?;
    f.sync_all()?;
    drop(f);
    let heads = repo.path.join("refs/heads");
    let (from, to) = (cstr(&lock)?, cstr(&repo.path.join(REF_REL))?);
    crate::store::journal(&format!("cas_begin {effect_id}"));
    // SAFETY: valid NUL-terminated absolute paths.
    if unsafe { libc::rename(from.as_ptr(), to.as_ptr()) } != 0 {
        let e = io::Error::last_os_error();
        let _ = fs::remove_file(&lock);
        return Err(e.into());
    }
    let d = File::open(&heads)?;
    // SAFETY: valid descriptor.
    unsafe { libc::fsync(d.as_raw_fd()) };
    // Window: the ref is updated but the audit line is not yet written. The two writes are
    // not atomic in either order; see docs/OPERATIONS.md (write-order assessment).
    crate::store::barrier("after_ref_rename")?;
    let secs = crate::store::now_ms().map(|t| t / 1000).unwrap_or(0);
    let line = format!(
        "{old} {new} {IDENT} {secs} +0000\t{}\n",
        effect_message(effect_id)
    );
    let appended = OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(repo.path.join(LOG_REL))
        .and_then(|mut l| {
            l.write_all(line.as_bytes())?;
            l.sync_all()
        })
        .is_ok();
    Ok(Cas::Applied {
        reflog_appended: appended,
    })
}
fn cstr(p: &Path) -> Result<CString, Error> {
    CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| Error::Invalid("NUL in path"))
}
/// Set the initial ref value on a freshly created repository (no prior value).
pub fn initialize_ref(repo: &Repo, base: &str) -> Result<(), Error> {
    if !is_oid(base) {
        return Err(Error::Invalid("invalid base object ID"));
    }
    excl(
        &repo.path.join(REF_REL),
        format!("{base}\n").as_bytes(),
        0o600,
    )?;
    sync_dir(&repo.path.join("refs/heads"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha1_and_object_ids_match_git() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // `echo hi | git hash-object --stdin`
        assert_eq!(
            object("blob", b"hi\n").id,
            "45b983be36b73c0788dc9cbcb76cbb80fc7bb057"
        );
        // Known empty tree id.
        assert_eq!(
            object("tree", b"").id,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        let big = vec![7u8; 70000];
        let z = zlib_stored(&big);
        assert_eq!(z.len(), 2 + 2 * 5 + 70000 + 4);
    }
}

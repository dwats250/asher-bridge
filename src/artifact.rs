//! Frozen snapshots of the fixed synthetic symbol layout.
//!
//! Hashes, modes and inode/ctime identities DETECT change; they are not isolation
//! against a same-user writer. Directory and file access never follows symlinks.
use crate::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, Metadata},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

pub const LAYOUT: &str = "symbol-fixture-v1";
/// Complete supported layout, in canonical (sorted) order. Anything else is rejected.
pub const FILES: [&str; 2] = ["capture.json", "symbols.csv"];
pub const MAX_FILE_BYTES: u64 = 65536;
/// Directory listing bound; listings beyond this are rejected without further reads.
pub const MAX_LISTING: usize = 64;
pub const FILE_MODE: u32 = 0o600;
pub const DIR_MODE: u32 = 0o700;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub mode: u32,
    pub size: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub layout: String,
    pub entries: Vec<Entry>,
}
/// Kernel identity of a node, used to detect replacement, relinking, chmod or rewrite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeId {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub dir: NodeId,
    pub files: Vec<NodeId>,
}
/// A verified scan: manifest, identities, and the exact bytes that were hashed.
#[derive(Debug, Clone)]
pub struct Scan {
    pub manifest: Manifest,
    pub identity: Identity,
    pub bytes: BTreeMap<String, Vec<u8>>,
}

fn rejected(s: impl Into<String>) -> Error {
    Error::Rejected(s.into())
}
pub fn sha256_hex(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
impl Manifest {
    pub fn canonical(&self) -> Result<String, Error> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }
    pub fn hash(&self) -> Result<String, Error> {
        Ok(sha256_hex(self.canonical()?.as_bytes()))
    }
    /// Structural rules: complete fixed layout, sorted, bounded, no other paths.
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != 1 || self.layout != LAYOUT || self.entries.len() != FILES.len() {
            return Err(rejected("unsupported manifest layout"));
        }
        for (e, name) in self.entries.iter().zip(FILES) {
            if e.path != name
                || e.mode != FILE_MODE
                || e.size > MAX_FILE_BYTES
                || !is_hex64(&e.sha256)
            {
                return Err(rejected("manifest entry outside supported layout"));
            }
        }
        Ok(())
    }
    /// Accept only the exact canonical encoding, so one manifest has one identity.
    pub fn parse(json: &str) -> Result<Self, Error> {
        let m: Manifest =
            serde_json::from_str(json).map_err(|_| rejected("manifest is not valid JSON"))?;
        if m.canonical()? != json {
            return Err(rejected("manifest encoding is not canonical"));
        }
        Ok(m)
    }
    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path)
    }
}

fn node(path: &str, m: &Metadata) -> NodeId {
    NodeId {
        path: path.into(),
        dev: m.dev(),
        ino: m.ino(),
        mode: m.mode(),
        nlink: m.nlink(),
        size: m.size(),
        mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
        ctime_ns: m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128,
    }
}
fn cname(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))
}
fn owned(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: fd was just returned by the kernel and is exclusively owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
fn openat(dir: &OwnedFd, name: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = cname(name)?;
    // SAFETY: valid NUL-terminated name and open directory descriptor.
    owned(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode as libc::c_uint,
        )
    })
}
/// Opens an absolute or relative directory path whose final component is not a symlink.
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: valid NUL-terminated path.
    owned(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}
pub fn open_subdir(dir: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    openat(dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}
fn metadata(fd: &OwnedFd) -> io::Result<Metadata> {
    File::from(fd.try_clone()?).metadata()
}
/// Type of an entry without following it (S_IFMT bits).
pub fn entry_type(dir: &OwnedFd, name: &str) -> io::Result<(u32, u64)> {
    let c = cname(name)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor/name, writable stat buffer.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((st.st_mode & libc::S_IFMT, st.st_nlink))
}
/// Bounded, sorted listing of a directory descriptor (never by re-resolving a path name).
pub fn list(dir: &OwnedFd) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            return Err(rejected("non-UTF-8 entry name"));
        };
        names.push(name.to_string());
        if names.len() > MAX_LISTING {
            return Err(rejected("directory listing exceeds bound"));
        }
    }
    names.sort();
    Ok(names)
}
fn describe(names: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .take(8)
        .map(|n| n.chars().take(64).collect())
        .collect();
    format!("{} entries {:?}", names.len(), shown)
}
fn owner_mode(m: &Metadata, mode: u32) -> bool {
    // SAFETY: geteuid has no preconditions.
    m.mode() & 0o7777 == mode && m.uid() == unsafe { libc::geteuid() }
}

/// Complete scan of one fixed-layout directory. Extra, missing, linked, special or
/// oversized entries are rejected; nothing is silently excluded.
pub fn scan_dir(dir: &OwnedFd) -> Result<Scan, Error> {
    let before = metadata(dir)?;
    if !before.is_dir() || !owner_mode(&before, DIR_MODE) {
        return Err(rejected("directory type, owner or mode unsupported"));
    }
    let names = list(dir)?;
    if names != FILES {
        return Err(rejected(format!(
            "unsupported layout: {} (expected exactly {:?})",
            describe(&names),
            FILES
        )));
    }
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut bytes = BTreeMap::new();
    for name in FILES {
        let (kind, _) = entry_type(dir, name)?;
        if kind != libc::S_IFREG {
            return Err(rejected(format!("{name}: not a regular file")));
        }
        let fd = openat(dir, name, libc::O_RDONLY | libc::O_NONBLOCK, 0)
            .map_err(|e| rejected(format!("{name}: open refused: {e}")))?;
        let m1 = metadata(&fd)?;
        if !m1.is_file() || m1.nlink() != 1 || !owner_mode(&m1, FILE_MODE) {
            return Err(rejected(format!(
                "{name}: must be a single-link private regular file"
            )));
        }
        if m1.size() > MAX_FILE_BYTES {
            return Err(rejected(format!("{name}: exceeds {MAX_FILE_BYTES} bytes")));
        }
        let mut buf = Vec::new();
        File::from(fd.try_clone()?)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut buf)?;
        let m2 = metadata(&fd)?;
        if buf.len() as u64 != m1.size() || node(name, &m1) != node(name, &m2) {
            return Err(rejected(format!("{name}: changed while reading")));
        }
        entries.push(Entry {
            path: name.into(),
            mode: m1.mode() & 0o7777,
            size: m1.size(),
            sha256: sha256_hex(&buf),
        });
        files.push(node(name, &m1));
        bytes.insert(name.to_string(), buf);
    }
    let after = metadata(dir)?;
    if node(".", &before) != node(".", &after) || list(dir)? != FILES {
        return Err(rejected("directory changed during scan"));
    }
    let manifest = Manifest {
        version: 1,
        layout: LAYOUT.into(),
        entries,
    };
    manifest.validate()?;
    Ok(Scan {
        manifest,
        identity: Identity {
            dir: node(".", &before),
            files,
        },
        bytes,
    })
}

fn sync(fd: &OwnedFd) -> io::Result<()> {
    File::from(fd.try_clone()?).sync_all()
}
fn mkdirat(dir: &OwnedFd, name: &str) -> io::Result<()> {
    let c = cname(name)?;
    // SAFETY: valid descriptor and name.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), DIR_MODE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn artifacts_dir(root: &Path) -> Result<OwnedFd, Error> {
    let root_fd = open_dir(root)?;
    let fd = match open_subdir(&root_fd, "artifacts") {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            mkdirat(&root_fd, "artifacts")?;
            sync(&root_fd)?;
            open_subdir(&root_fd, "artifacts")?
        }
        Err(e) => return Err(e.into()),
    };
    let m = metadata(&fd)?;
    if !m.is_dir() || !owner_mode(&m, DIR_MODE) {
        return Err(Error::Invalid(
            "artifact store must be a private 0700 directory",
        ));
    }
    Ok(fd)
}
pub fn content_path(id: &str) -> String {
    format!("artifacts/{id}")
}

/// Stage exact verified bytes, sync, rename into place and sync the parent BEFORE any
/// database reference commits. A crash leaves a harmless unreferenced directory.
pub fn freeze(root: &Path, id: &str, source: &Scan) -> Result<(PathBuf, Identity), Error> {
    if !crate::protocol::valid_id(id) {
        return Err(Error::Invalid("invalid artifact ID"));
    }
    let store = artifacts_dir(root)?;
    let stage_name = format!(".stage-{id}");
    mkdirat(&store, &stage_name)?;
    let stage = open_subdir(&store, &stage_name)?;
    File::from(stage.try_clone()?).set_permissions(fs::Permissions::from_mode(DIR_MODE))?;
    for (name, data) in &source.bytes {
        let fd = openat(
            &stage,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            FILE_MODE,
        )?;
        let mut f = File::from(fd);
        f.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    sync(&stage)?;
    drop(stage);
    let (from, to) = (cname(&stage_name)?, cname(id)?);
    // SAFETY: valid descriptors and names. NOREPLACE: never overwrite an existing artifact.
    if unsafe {
        libc::renameat2(
            store.as_raw_fd(),
            from.as_ptr(),
            store.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    sync(&store)?;
    let frozen = scan_dir(&open_subdir(&store, id)?)?;
    if frozen.manifest != source.manifest || frozen.bytes != source.bytes {
        return Err(rejected("snapshot readback differs from verified source"));
    }
    Ok((root.join(content_path(id)), frozen.identity))
}

/// Re-read a sealed snapshot and require exact manifest AND recorded kernel identity.
pub fn verify(
    root: &Path,
    id: &str,
    manifest_json: &str,
    manifest_hash: &str,
    identity_json: &str,
) -> Result<Scan, String> {
    let manifest = Manifest::parse(manifest_json).map_err(|e| e.to_string())?;
    if manifest.hash().map_err(|e| e.to_string())? != manifest_hash {
        return Err("stored manifest hash mismatch".into());
    }
    let identity: Identity = serde_json::from_str(identity_json)
        .map_err(|_| "stored identity unreadable".to_string())?;
    let open = || -> Result<OwnedFd, Error> {
        let store = open_subdir(&open_dir(root)?, "artifacts")?;
        Ok(open_subdir(&store, id)?)
    };
    let dir = open().map_err(|e| format!("snapshot unavailable: {e}"))?;
    let scan = scan_dir(&dir).map_err(|e| format!("snapshot rejected: {e}"))?;
    if scan.manifest != manifest {
        return Err("snapshot content differs from sealed manifest".into());
    }
    if scan.identity != identity {
        return Err("snapshot identity changed (replaced, relinked, rewritten or chmod)".into());
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            version: 1,
            layout: LAYOUT.into(),
            entries: FILES
                .iter()
                .map(|p| Entry {
                    path: (*p).into(),
                    mode: FILE_MODE,
                    size: 1,
                    sha256: "a".repeat(64),
                })
                .collect(),
        }
    }
    #[test]
    fn manifest_encoding_is_canonical_and_complete() {
        let m = manifest();
        let json = m.canonical().unwrap();
        assert_eq!(Manifest::parse(&json).unwrap(), m);
        assert_eq!(m.hash().unwrap(), sha256_hex(json.as_bytes()));
        // Same content with different key order or whitespace is not the same identity.
        let reordered = json.replacen("{\"version\":1,\"layout\"", "{\"layout\"", 1);
        assert!(Manifest::parse(&json.replace(',', ", ")).is_err());
        assert!(Manifest::parse(&reordered).is_err());
        let mut missing = m.clone();
        missing.entries.pop();
        assert!(missing.canonical().is_err());
        let mut extra = m.clone();
        extra.entries.push(extra.entries[0].clone());
        assert!(extra.validate().is_err());
    }
    #[test]
    fn manifest_rejects_traversal_and_unsupported_entries() {
        for bad in [
            "../symbols.csv",
            "/etc/passwd",
            "sub/symbols.csv",
            ".gitmodules",
            "symbols.csv/",
            "",
        ] {
            let mut m = manifest();
            m.entries[1].path = bad.into();
            assert!(m.validate().is_err(), "{bad}");
        }
        let mut m = manifest();
        m.entries[0].mode = 0o644;
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.entries[0].size = MAX_FILE_BYTES + 1;
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.entries[0].sha256 = "Z".repeat(64);
        assert!(m.validate().is_err());
    }
}

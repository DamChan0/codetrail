//! Editor file model (AC5): load limits, atomic save, external-change detection.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub const MAX_EDIT_BYTES: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: Option<SystemTime>,
    pub len: u64,
    pub hash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOnly {
    Binary,
    TooLarge(u64),
    /// Resolves (symlink or `..`) outside the repository: never read or written.
    Outside,
}
impl ReadOnly {
    pub fn reason(&self) -> String {
        match self {
            ReadOnly::Outside => "This path resolves outside the repository: read-only, contents not shown.".into(),
            ReadOnly::Binary => "Binary or non-UTF-8 file: read-only, contents not shown.".into(),
            ReadOnly::TooLarge(n) => format!("{} is over the 1 MB edit limit: read-only, showing the first 1 MB.", crate::timefmt::human_bytes(*n)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub text: String,
    pub stamp: FileStamp,
    pub crlf: bool,
    pub read_only: Option<ReadOnly>,
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// True when `path` (after resolving symlinks and `..`) stays inside `root`.
/// A not-yet-existing file is judged by its parent directory.
pub fn contained(root: &Path, path: &Path) -> bool {
    let Ok(root) = root.canonicalize() else { return false };
    let resolved = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => match (path.parent().and_then(|p| p.canonicalize().ok()), path.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => return false,
        },
    };
    resolved.starts_with(&root)
}

/// `load` after the containment check; an outside target is not opened at all.
pub fn load_in(root: &Path, path: &Path) -> io::Result<Loaded> {
    if !contained(root, path) {
        let stamp = FileStamp { mtime: None, len: 0, hash: 0 };
        return Ok(Loaded { text: String::new(), stamp, crlf: false, read_only: Some(ReadOnly::Outside) });
    }
    load(path)
}

/// `save` after the containment check (re-checked at write time: the link may have changed since open).
pub fn save_in(root: &Path, path: &Path, text: &str, crlf: bool, loaded: &FileStamp, force: bool) -> io::Result<SaveOutcome> {
    if !contained(root, path) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "path resolves outside the repository"));
    }
    save(path, text, crlf, loaded, force)
}

pub fn load(path: &Path) -> io::Result<Loaded> {
    use io::Read;
    let meta = std::fs::metadata(path)?;
    let len = meta.len();
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(MAX_EDIT_BYTES).read_to_end(&mut buf)?;
    let stamp = FileStamp { mtime: meta.modified().ok(), len, hash: hash_bytes(&buf) };
    let binary = buf.iter().take(8000).any(|&b| b == 0);
    if binary {
        return Ok(Loaded { text: String::new(), stamp, crlf: false, read_only: Some(ReadOnly::Binary) });
    }
    let too_large = len > MAX_EDIT_BYTES;
    let text = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) if too_large && e.utf8_error().error_len().is_none() => {
            // cut inside a multi-byte char at the 1 MB boundary: keep the valid prefix
            let up = e.utf8_error().valid_up_to();
            String::from_utf8_lossy(&e.into_bytes()[..up]).into_owned()
        }
        Err(_) => return Ok(Loaded { text: String::new(), stamp, crlf: false, read_only: Some(ReadOnly::Binary) }),
    };
    let crlf = text.contains("\r\n");
    let text = if crlf { text.replace("\r\n", "\n") } else { text };
    Ok(Loaded { text, stamp, crlf, read_only: too_large.then_some(ReadOnly::TooLarge(len)) })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disk {
    Unchanged,
    Changed(FileStamp),
    Gone,
}

/// Cheap stat first; content hash only when mtime/len moved (a `touch` is not a change).
pub fn check_disk(path: &Path, loaded: &FileStamp) -> Disk {
    let Ok(meta) = std::fs::metadata(path) else { return Disk::Gone };
    let mt = meta.modified().ok();
    if mt == loaded.mtime && meta.len() == loaded.len {
        return Disk::Unchanged;
    }
    match std::fs::read(path) {
        Ok(b) => {
            let h = hash_bytes(&b[..b.len().min(MAX_EDIT_BYTES as usize)]);
            if h == loaded.hash && meta.len() == loaded.len {
                Disk::Unchanged
            } else {
                Disk::Changed(FileStamp { mtime: mt, len: meta.len(), hash: h })
            }
        }
        Err(_) => Disk::Gone,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(FileStamp),
    /// File changed (or vanished) on disk since it was loaded; nothing was written.
    Conflict(Disk),
}

/// Atomic save (temp + rename). Unless `force`, refuses when the disk copy changed since load.
pub fn save(path: &Path, text: &str, crlf: bool, loaded: &FileStamp, force: bool) -> io::Result<SaveOutcome> {
    if !force {
        match check_disk(path, loaded) {
            Disk::Unchanged => {}
            d => return Ok(SaveOutcome::Conflict(d)),
        }
    }
    let bytes: Vec<u8> = if crlf { text.replace('\n', "\r\n").into_bytes() } else { text.as_bytes().to_vec() };
    crate::fsutil::atomic_write(path, &bytes)?;
    let stamp = FileStamp { mtime: mtime(path), len: bytes.len() as u64, hash: hash_bytes(&bytes) };
    Ok(SaveOutcome::Saved(stamp))
}

/// Byte offset of the start of 1-based `line` (clamped to the text end).
pub fn line_start_offset(text: &str, line: u32) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut n = 1;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            n += 1;
            if n == line {
                return i + 1;
            }
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        (d, p)
    }

    #[test]
    fn symlink_and_dotdot_outside_root_are_not_opened() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let secret = d.path().join("secret.txt");
        std::fs::write(&secret, "TOPSECRET").unwrap();
        std::fs::write(root.join("ok.txt"), "fine").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
        let l = load_in(&root, &root.join("link.txt")).unwrap();
        assert_eq!(l.read_only, Some(ReadOnly::Outside));
        assert!(l.text.is_empty());
        let l = load_in(&root, &root.join("../secret.txt")).unwrap();
        assert_eq!(l.read_only, Some(ReadOnly::Outside));
        assert!(load_in(&root, &root.join("ok.txt")).unwrap().read_only.is_none());
        let st = FileStamp { mtime: None, len: 0, hash: 0 };
        assert!(save_in(&root, &root.join("link.txt"), "x", false, &st, true).is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "TOPSECRET");
    }

    #[test]
    fn load_normal_text() {
        let (_d, p) = tmp();
        std::fs::write(&p, "hello\n한글\n").unwrap();
        let l = load(&p).unwrap();
        assert_eq!(l.text, "hello\n한글\n");
        assert!(l.read_only.is_none() && !l.crlf);
    }

    #[test]
    fn binary_and_non_utf8_are_read_only() {
        let (_d, p) = tmp();
        std::fs::write(&p, b"abc\0def").unwrap();
        assert_eq!(load(&p).unwrap().read_only, Some(ReadOnly::Binary));
        std::fs::write(&p, [0xff, 0xfe, 0x41]).unwrap();
        assert_eq!(load(&p).unwrap().read_only, Some(ReadOnly::Binary));
    }

    #[test]
    fn over_one_mb_is_read_only_and_truncated() {
        let (_d, p) = tmp();
        let big = "x".repeat(MAX_EDIT_BYTES as usize + 10);
        std::fs::write(&p, &big).unwrap();
        let l = load(&p).unwrap();
        assert_eq!(l.read_only, Some(ReadOnly::TooLarge(MAX_EDIT_BYTES + 10)));
        assert_eq!(l.text.len() as u64, MAX_EDIT_BYTES);
    }

    #[test]
    fn exactly_one_mb_is_editable() {
        let (_d, p) = tmp();
        std::fs::write(&p, "y".repeat(MAX_EDIT_BYTES as usize)).unwrap();
        assert!(load(&p).unwrap().read_only.is_none());
    }

    #[test]
    fn crlf_roundtrips() {
        let (_d, p) = tmp();
        std::fs::write(&p, "a\r\nb\r\n").unwrap();
        let l = load(&p).unwrap();
        assert!(l.crlf);
        assert_eq!(l.text, "a\nb\n");
        let out = save(&p, "a\nB\n", l.crlf, &l.stamp, false).unwrap();
        assert!(matches!(out, SaveOutcome::Saved(_)));
        assert_eq!(std::fs::read(&p).unwrap(), b"a\r\nB\r\n");
    }

    #[test]
    fn save_is_atomic_and_updates_stamp() {
        let (d, p) = tmp();
        std::fs::write(&p, "one").unwrap();
        let l = load(&p).unwrap();
        let SaveOutcome::Saved(st) = save(&p, "two", false, &l.stamp, false).unwrap() else { panic!() };
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(check_disk(&p, &st), Disk::Unchanged);
        let files: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(files.len(), 1, "temp file leaked");
    }

    #[test]
    fn external_change_blocks_save_until_forced() {
        let (_d, p) = tmp();
        std::fs::write(&p, "one").unwrap();
        let l = load(&p).unwrap();
        std::fs::write(&p, "external!").unwrap();
        let out = save(&p, "mine", false, &l.stamp, false).unwrap();
        assert!(matches!(out, SaveOutcome::Conflict(Disk::Changed(_))));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "external!", "must not clobber");
        assert!(matches!(save(&p, "mine", false, &l.stamp, true).unwrap(), SaveOutcome::Saved(_)));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "mine");
    }

    #[test]
    fn deleted_file_is_a_conflict() {
        let (_d, p) = tmp();
        std::fs::write(&p, "one").unwrap();
        let l = load(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(save(&p, "x", false, &l.stamp, false).unwrap(), SaveOutcome::Conflict(Disk::Gone));
    }

    #[test]
    fn touch_without_content_change_is_not_a_change() {
        let (_d, p) = tmp();
        std::fs::write(&p, "same").unwrap();
        let l = load(&p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, "same").unwrap();
        assert_eq!(check_disk(&p, &l.stamp), Disk::Unchanged);
    }

    #[test]
    fn line_offsets() {
        let t = "a\nbb\nccc";
        assert_eq!(line_start_offset(t, 1), 0);
        assert_eq!(line_start_offset(t, 2), 2);
        assert_eq!(line_start_offset(t, 3), 5);
        assert_eq!(line_start_offset(t, 99), t.len());
    }
}

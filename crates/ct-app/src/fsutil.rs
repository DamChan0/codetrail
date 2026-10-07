//! Atomic file replacement (temp file in the same directory + fsync + rename).

use std::io::Write;
use std::path::Path;

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(".{name}.ct-tmp-{}", std::process::id()));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        // keep the permission bits of the file we replace
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = f.set_permissions(meta.permissions());
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_leaves_no_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn failed_write_keeps_original() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        std::fs::write(&p, "keep").unwrap();
        // a directory at the destination makes rename fail
        let q = d.path().join("dir");
        std::fs::create_dir(&q).unwrap();
        assert!(atomic_write(&q, b"x").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "keep");
        let leftovers = std::fs::read_dir(d.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("ct-tmp")).count();
        assert_eq!(leftovers, 0);
    }
}

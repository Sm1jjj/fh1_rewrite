//! Case-insensitive disc paths. Xbox 360 discs mix case (`media/cars/FER_FXX_05.zip` vs gamedb's `FER_FXX_05`
//! spelled differently, `Media` vs `media`), which Windows never notices. On case-sensitive file systems (Linux),
//! [`resolve`] finds the real spelling, one path component at a time, when the exact path doesn't exist.

use std::path::{Component, Path, PathBuf};

/// `path` itself when it exists (always on Windows); otherwise the same path with each missing component matched
/// case-insensitively against its directory. Falls back to `path` unchanged when nothing matches.
pub fn resolve(path: &Path) -> PathBuf {
    if cfg!(windows) || path.exists() {
        return path.to_path_buf();
    }
    let mut cur = PathBuf::new();
    for c in path.components() {
        let next = cur.join(c.as_os_str());
        if next.exists() || !matches!(c, Component::Normal(_)) {
            cur = next;
            continue;
        }
        let want = c.as_os_str().to_string_lossy().to_lowercase();
        let dir = if cur.as_os_str().is_empty() { Path::new(".") } else { cur.as_path() };
        let found = std::fs::read_dir(dir).ok().and_then(|rd| {
            rd.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().to_lowercase() == want)
        });
        match found {
            Some(n) => cur.push(n),
            None => return path.to_path_buf(),
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_other_case() {
        let root = std::env::temp_dir().join(format!("fh1-path-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Media/Cars")).unwrap();
        std::fs::write(root.join("Media/Cars/FER_FXX_05.zip"), b"x").unwrap();
        let got = super::resolve(&root.join("media/cars/FER_FXX_05.zip"));
        assert!(got.is_file(), "{}", got.display());
        let _ = std::fs::remove_dir_all(&root);
    }
}

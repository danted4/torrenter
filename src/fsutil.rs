use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    sibling(path, ".tmp")
}

pub(crate) fn prev_path(path: &Path) -> PathBuf {
    sibling(path, ".prev")
}

pub(crate) fn bak_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

/// Write `<path>.tmp`, then rename over `path`. With `private`, the file is
/// created owner-only (0600) on unix before any bytes land in it.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = tmp_path(path);
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    drop(file);
    replace_file(&tmp, path)
}

/// POSIX rename replaces. Windows cannot rename over an existing file, so the
/// live file is moved aside only for that replace, then restored if the new
/// name fails. Never unlink the live file first.
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    if cfg!(windows) && to.exists() {
        let backup = prev_path(to);
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(to, &backup)
            .with_context(|| format!("failed to move aside {}", to.display()))?;
        if let Err(error) = std::fs::rename(from, to) {
            let _ = std::fs::rename(&backup, to);
            return Err(error).with_context(|| format!("failed to replace {}", to.display()));
        }
        let _ = std::fs::remove_file(&backup);
        return Ok(());
    }
    std::fs::rename(from, to).with_context(|| format!("failed to replace {}", to.display()))
}

/// Restore a save that crashed between the two renames.
pub(crate) fn recover_incomplete_save(path: &Path) {
    if path.exists() {
        return;
    }
    for aside in [tmp_path(path), prev_path(path)] {
        if aside.exists() && std::fs::rename(&aside, path).is_ok() {
            return;
        }
    }
}

/// Move an unreadable file to `<path>.bak` so the user's bytes are kept.
pub(crate) fn quarantine(path: &Path) {
    let bak = bak_path(path);
    let _ = std::fs::remove_file(&bak);
    let _ = std::fs::rename(path, bak);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_cleans_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        atomic_write(&path, b"one", false).unwrap();
        atomic_write(&path, b"two", false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!tmp_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        atomic_write(&path, b"{}", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn recovers_tmp_and_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(tmp_path(&path), b"x").unwrap();
        recover_incomplete_save(&path);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        quarantine(&path);
        assert!(!path.exists());
        assert!(bak_path(&path).exists());
    }
}

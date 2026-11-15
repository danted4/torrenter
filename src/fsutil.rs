use std::cmp::Reverse;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// A temp or moved-aside file this old is left by a crashed save, not by a
/// save in progress (those last milliseconds), so it may be removed.
const STALE_ASIDE: Duration = Duration::from_secs(60 * 60);

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// `<name>.<pid>.tmp`: unique per process, so a second process (the CLI's
/// `debrid add` while the GUI is open) never writes the same temp file.
pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    sibling(path, &format!(".{}.tmp", std::process::id()))
}

/// `<name>.<pid>.prev`: where Windows moves the live file aside during a replace.
pub(crate) fn prev_path(path: &Path) -> PathBuf {
    sibling(path, &format!(".{}.prev", std::process::id()))
}

pub(crate) fn bak_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

/// Saves in this process take turns: writes, cleanup and crash recovery.
/// Threads (GUI worker vs UI) do save concurrently. Across processes nothing
/// is locked: temp names differ, so a file is never torn, but the later of
/// two simultaneous edits wins.
static FILES: Mutex<()> = Mutex::new(());

fn files_lock() -> MutexGuard<'static, ()> {
    FILES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Lock for a read-modify-write of one settings file, held around the load,
/// the change and the save so two edits cannot drop each other's setting.
/// Separate from the write lock above, so nesting is safe.
pub(crate) fn edit_lock(lock: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write `<path>.<pid>.tmp`, then rename over `path`. With `private`, the file
/// is created owner-only (0600) on unix before any bytes land in it.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let _files = files_lock();
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
    replace_file(&tmp, path)?;
    remove_stale_asides(path);
    Ok(())
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

struct Aside {
    path: PathBuf,
    is_tmp: bool,
    modified: SystemTime,
}

/// Temp and moved-aside siblings of `path` from any process: `<name>.<pid>.tmp`
/// / `.prev`, plus the fixed `<name>.tmp` / `.prev` older builds used.
fn asides(path: &Path) -> Vec<Aside> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let rest = file_name.to_str()?.strip_prefix(name)?;
            let (middle, is_tmp) = if let Some(middle) = rest.strip_suffix(".tmp") {
                (middle, true)
            } else {
                (rest.strip_suffix(".prev")?, false)
            };
            // "" (older builds) or ".<pid>"; never another file such as
            // `config.json5.tmp` or `config.json.backup.tmp`.
            let ours = middle.is_empty()
                || middle.strip_prefix('.').is_some_and(|pid| {
                    !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
                });
            if !ours {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some(Aside {
                path: entry.path(),
                is_tmp,
                modified,
            })
        })
        .collect()
}

/// Best effort: drop temp files a crashed process (or an older build) left.
fn remove_stale_asides(path: &Path) {
    let now = SystemTime::now();
    for aside in asides(path) {
        let age = now.duration_since(aside.modified).unwrap_or_default();
        if age >= STALE_ASIDE {
            let _ = std::fs::remove_file(&aside.path);
        }
    }
}

/// Restore a save that crashed between writing and replacing. Every file this
/// module saves is JSON, so a candidate is restored only if it parses: a
/// half-written temp (crash mid-write, or another process still writing) is
/// skipped. Newest temp first, then the moved-aside original.
pub(crate) fn recover_incomplete_save(path: &Path) {
    let _files = files_lock();
    if path.exists() {
        return;
    }
    let mut candidates = asides(path);
    candidates.sort_by_key(|aside| (!aside.is_tmp, Reverse(aside.modified)));
    for aside in candidates {
        let complete = std::fs::read(&aside.path)
            .ok()
            .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        if complete && std::fs::rename(&aside.path, path).is_ok() {
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
        assert!(asides(&path).is_empty());
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

    fn age(path: &Path, ago: Duration) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - ago)
            .unwrap();
    }

    #[test]
    fn temp_names_are_per_process() {
        let path = Path::new("/x/config.json");
        let pid = std::process::id();
        assert_eq!(
            tmp_path(path),
            Path::new(&format!("/x/config.json.{pid}.tmp"))
        );
        assert_eq!(
            prev_path(path),
            Path::new(&format!("/x/config.json.{pid}.prev"))
        );
    }

    #[test]
    fn recovers_tmp_and_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(tmp_path(&path), br#"{"x":1}"#).unwrap();
        recover_incomplete_save(&path);
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"x":1}"#);
        quarantine(&path);
        assert!(!path.exists());
        assert!(bak_path(&path).exists());
    }

    #[test]
    fn recovery_takes_the_newest_complete_save_from_any_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let legacy = dir.path().join("a.json.tmp");
        let other = dir.path().join("a.json.4242.tmp");
        let partial = dir.path().join("a.json.4343.tmp");
        let unrelated = dir.path().join("a.json.backup.tmp");
        let lookalike = dir.path().join("a.json5.tmp");
        std::fs::write(&lookalike, br#"{"v":"no"}"#).unwrap();
        std::fs::write(&legacy, br#"{"v":"legacy"}"#).unwrap();
        age(&legacy, Duration::from_secs(300));
        std::fs::write(&other, br#"{"v":"other"}"#).unwrap();
        age(&other, Duration::from_secs(60));
        // Newest, but still being written (or torn by a crash): skipped.
        std::fs::write(&partial, br#"{"v":"par"#).unwrap();
        std::fs::write(&unrelated, br#"{"v":"no"}"#).unwrap();
        recover_incomplete_save(&path);
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"v":"other"}"#);
        assert!(partial.exists() && legacy.exists() && unrelated.exists());
        // Nothing happens while the live file exists.
        recover_incomplete_save(&path);
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"v":"other"}"#);
    }

    #[test]
    fn saves_clean_up_stale_temps_but_not_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let crashed = dir.path().join("a.json.4242.tmp");
        let legacy = dir.path().join("a.json.prev");
        let live = dir.path().join("a.json.4343.tmp");
        for file in [&crashed, &legacy, &live] {
            std::fs::write(file, b"{}").unwrap();
        }
        age(&crashed, STALE_ASIDE + Duration::from_secs(1));
        age(&legacy, STALE_ASIDE * 2);
        atomic_write(&path, b"{}", false).unwrap();
        assert!(!crashed.exists() && !legacy.exists(), "stale temps removed");
        assert!(
            live.exists(),
            "another process may still be writing this one"
        );
        assert!(!tmp_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_temp_is_owner_only_before_the_rename() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        // A stale temp of ours with loose permissions is replaced, not reused.
        std::fs::write(tmp_path(&path), b"old").unwrap();
        std::fs::set_permissions(tmp_path(&path), std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&path, b"{}", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

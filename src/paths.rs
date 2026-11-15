use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::update::AppConfig;

const TORRENTER_DIR: &str = "torrenter";

pub fn default_download_dir() -> PathBuf {
    let downloads = dirs::download_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads")
    });
    downloads.join(TORRENTER_DIR)
}

pub fn configured_dir(config: &AppConfig) -> Option<PathBuf> {
    config
        .download_dir
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| plain_path(path.clone()))
}

/// Windows `canonicalize` returns verbatim paths (`\\?\E:\folder`,
/// `\\?\UNC\server\share`). Give back the everyday form people recognise
/// (`E:\folder`, `\\server\share`); any other path, or a verbatim form with
/// no plain equivalent (`\\?\Volume{…}`), is returned unchanged.
pub fn plain_path(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if drive && (bytes.len() == 2 || bytes[2] == b'\\') {
            return PathBuf::from(rest);
        }
    }
    path
}

pub fn effective_dir(config: &AppConfig) -> PathBuf {
    configured_dir(config).unwrap_or_else(default_download_dir)
}

pub fn resolve_user_dir(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        bail!("download folder path is empty");
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("current directory is not set")?
            .join(path))
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create download folder {}", path.display()))?;
    if !path.is_dir() {
        bail!("download folder is not a directory: {}", path.display());
    }
    Ok(())
}

pub fn persistable_dir(path: impl AsRef<Path>) -> Result<PathBuf> {
    let resolved = resolve_user_dir(path)?;
    ensure_dir(&resolved)?;
    Ok(plain_path(
        std::fs::canonicalize(&resolved).unwrap_or(resolved),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_ends_with_torrenter() {
        assert_eq!(
            default_download_dir()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("torrenter")
        );
    }

    #[test]
    fn missing_config_uses_default() {
        let config = AppConfig::default();
        assert!(configured_dir(&config).is_none());
        assert_eq!(effective_dir(&config), default_download_dir());
    }

    #[test]
    fn configured_dir_wins() {
        let config = AppConfig {
            download_dir: Some(PathBuf::from("/tmp/custom-torrenter")),
            ..AppConfig::default()
        };
        assert_eq!(
            effective_dir(&config),
            PathBuf::from("/tmp/custom-torrenter")
        );
    }

    #[test]
    fn empty_configured_dir_is_ignored() {
        let config = AppConfig {
            download_dir: Some(PathBuf::from("")),
            ..AppConfig::default()
        };
        assert!(configured_dir(&config).is_none());
    }

    #[test]
    fn verbatim_windows_paths_display_plain() {
        let plain = |text: &str| plain_path(PathBuf::from(text)).display().to_string();
        assert_eq!(plain(r"\\?\E:\my\folder"), r"E:\my\folder");
        assert_eq!(plain(r"\\?\C:\"), r"C:\");
        assert_eq!(plain(r"\\?\UNC\nas\share\dl"), r"\\nas\share\dl");
        // No plain equivalent, or already plain: unchanged.
        assert_eq!(plain(r"\\?\Volume{abc}\dl"), r"\\?\Volume{abc}\dl");
        assert_eq!(plain(r"E:\my\folder"), r"E:\my\folder");
        assert_eq!(plain("/home/me/dl"), "/home/me/dl");
        // A folder saved verbatim by an earlier build reads back plain.
        let config = AppConfig {
            download_dir: Some(PathBuf::from(r"\\?\E:\my\folder")),
            ..AppConfig::default()
        };
        assert_eq!(effective_dir(&config), PathBuf::from(r"E:\my\folder"));
    }

    #[test]
    fn relative_paths_become_absolute() {
        let resolved = resolve_user_dir("nested/out").unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with(Path::new("nested").join("out")));
    }

    #[test]
    fn ensure_dir_creates_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("torrenter");
        ensure_dir(&dest).unwrap();
        assert!(dest.is_dir());
        let stored = persistable_dir(&dest).unwrap();
        assert!(stored.is_absolute());
        assert!(stored.is_dir());
    }
}

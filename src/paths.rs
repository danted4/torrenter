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
        .cloned()
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
    Ok(std::fs::canonicalize(&resolved).unwrap_or(resolved))
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

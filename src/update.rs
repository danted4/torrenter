use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::{self, AppKind, VERSION, VERSION_TAG};

const USER_AGENT: &str = concat!("torrenter/", env!("CARGO_PKG_VERSION"));
const LATEST_URL: &str = "https://api.github.com/repos/danted4/torrenter/releases/latest";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub auto_update: bool,
}

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub tag: String,
    pub asset_name: String,
    pub url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

pub fn config_path() -> Result<PathBuf> {
    let base = if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(dir)
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is not set")?)
    } else if cfg!(target_os = "macos") {
        home_dir()?.join("Library/Application Support")
    } else {
        home_dir()?.join(".config")
    };
    Ok(base.join("torrenter").join("config.json"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("home directory is not set")
}

pub fn load_config() -> AppConfig {
    let Ok(path) = config_path() else {
        return AppConfig::default();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return AppConfig::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(config)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

pub fn parse_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let trimmed = tag.trim().trim_start_matches('v');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_tag(latest), parse_tag(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

pub fn cleanup_replaced_binary() {
    if let Ok(current) = std::env::current_exe() {
        let old = old_binary_path(&current);
        let _ = std::fs::remove_file(old);
    }
}

fn old_binary_path(current: &Path) -> PathBuf {
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    current.with_file_name(name)
}

pub async fn check_latest(kind: AppKind) -> Result<Option<AvailableUpdate>> {
    let Some(asset_name) = identity::current_asset_name(kind) else {
        bail!("no GitHub asset name for this OS/architecture");
    };
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build update client")?;
    let release: GhRelease = client
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to query GitHub releases")?
        .error_for_status()
        .context("GitHub releases request failed")?
        .json()
        .await
        .context("failed to parse GitHub release JSON")?;

    if !is_newer(&release.tag_name, VERSION) {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| format!("release {} has no asset {asset_name}", release.tag_name))?;
    Ok(Some(AvailableUpdate {
        tag: release.tag_name,
        asset_name: asset.name,
        url: asset.browser_download_url,
    }))
}

pub async fn install_update(update: &AvailableUpdate) -> Result<PathBuf> {
    let current = std::env::current_exe().context("cannot locate current executable")?;
    let tmp = current.with_file_name(format!(
        ".{}.update",
        current
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("torrenter")
    ));
    download_to_file(&update.url, &tmp).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&tmp)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tmp, permissions)?;
    }
    replace_executable(&current, &tmp)?;
    Ok(current)
}

async fn download_to_file(url: &str, dest: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .context("failed to download release asset")?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk.context("truncated update download")?)
            .await?;
    }
    file.flush().await?;
    Ok(())
}

fn replace_executable(current: &Path, new_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let old = old_binary_path(current);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(current, &old)
            .context("failed to move the running Windows binary aside")?;
        std::fs::rename(new_path, current).context("failed to install the new Windows binary")?;
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(new_path, current).with_context(|| {
            format!(
                "failed to replace {} with the downloaded binary (need write permission)",
                current.display()
            )
        })?;
    }
    Ok(())
}

pub fn relaunch() -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&exe).args(args).exec();
        Err(error).context("failed to relaunch torrenter")
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(&exe)
            .args(args)
            .spawn()
            .context("failed to relaunch torrenter")?;
        std::process::exit(0);
    }
}

pub async fn auto_update_if_enabled(kind: AppKind) -> Result<bool> {
    cleanup_replaced_binary();
    if !load_config().auto_update {
        return Ok(false);
    }
    apply_available_update(kind).await
}

pub async fn apply_available_update(kind: AppKind) -> Result<bool> {
    let Some(update) = check_latest(kind).await? else {
        return Ok(false);
    };
    println!(
        "Updating {VERSION_TAG} -> {} ({})",
        update.tag, update.asset_name
    );
    install_update(&update).await?;
    Ok(true)
}

pub fn set_auto_update(enabled: bool) -> Result<AppConfig> {
    let mut config = load_config();
    config.auto_update = enabled;
    save_config(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_tags() {
        assert_eq!(parse_tag("v3.0.2"), Some((3, 0, 2)));
        assert_eq!(parse_tag("3.0.1"), Some((3, 0, 1)));
        assert!(parse_tag("nightly").is_none());
    }

    #[test]
    fn newer_release_is_detected() {
        assert!(is_newer("v3.1.0", "3.0.2"));
        assert!(!is_newer("v3.0.2", "3.0.2"));
        assert!(!is_newer("v2.9.9", "3.0.2"));
    }

    #[test]
    fn default_auto_update_is_off() {
        assert!(!AppConfig::default().auto_update);
    }
}

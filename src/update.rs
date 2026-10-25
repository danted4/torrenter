use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::identity::{self, AppKind, VERSION, VERSION_TAG};

const USER_AGENT: &str = concat!("torrenter/", env!("CARGO_PKG_VERSION"));
const LATEST_URL: &str = "https://api.github.com/repos/danted4/torrenter/releases/latest";
const CHECKSUMS_NAME: &str = "SHA256SUMS";
const MAX_UPDATE_BYTES: u64 = 80 * 1024 * 1024;
const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default = "default_true")]
    pub seed: bool,
    #[serde(default)]
    pub release_etag: Option<String>,
    #[serde(default)]
    pub cached_tag: Option<String>,
    #[serde(default)]
    pub cached_asset_name: Option<String>,
    #[serde(default)]
    pub cached_asset_url: Option<String>,
    #[serde(default)]
    pub cached_checksums_url: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_update: false,
            seed: true,
            release_etag: None,
            cached_tag: None,
            cached_asset_name: None,
            cached_asset_url: None,
            cached_checksums_url: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub tag: String,
    pub asset_name: String,
    pub url: String,
    pub checksums_url: String,
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

fn update_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("failed to build update client")
}

pub fn is_repo_release_asset_url(url: &str, tag: &str, asset_name: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return false;
    }
    let Some((owner, repo)) = identity::REPO.split_once('/') else {
        return false;
    };
    let mut segments = match parsed.path_segments() {
        Some(segments) => segments,
        None => return false,
    };
    let Some(path_owner) = segments.next() else {
        return false;
    };
    let Some(path_repo) = segments.next() else {
        return false;
    };
    let Some(releases) = segments.next() else {
        return false;
    };
    let Some(download) = segments.next() else {
        return false;
    };
    let Some(path_tag) = segments.next() else {
        return false;
    };
    let Some(file) = segments.next() else {
        return false;
    };
    path_owner == owner
        && path_repo == repo
        && releases == "releases"
        && download == "download"
        && path_tag == tag
        && !path_tag.is_empty()
        && !path_tag.contains("..")
        && file == asset_name
        && segments.next().is_none()
}

pub fn hash_for_asset(sums: &str, asset_name: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        if !is_sha256_hex(hash) {
            continue;
        }
        let file = rest.trim().trim_start_matches('*').trim();
        let name = Path::new(file).file_name()?.to_str()?;
        if name == asset_name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hashes_match(expected: &str, actual: &str) -> bool {
    is_sha256_hex(expected) && is_sha256_hex(actual) && expected.eq_ignore_ascii_case(actual)
}

fn cached_update(config: &AppConfig) -> Option<AvailableUpdate> {
    let update = AvailableUpdate {
        tag: config.cached_tag.clone()?,
        asset_name: config.cached_asset_name.clone()?,
        url: config.cached_asset_url.clone()?,
        checksums_url: config.cached_checksums_url.clone()?,
    };
    if !is_pinned_update(&update) {
        return None;
    }
    Some(update)
}

fn is_pinned_update(update: &AvailableUpdate) -> bool {
    is_repo_release_asset_url(&update.url, &update.tag, &update.asset_name)
        && is_repo_release_asset_url(&update.checksums_url, &update.tag, CHECKSUMS_NAME)
}

fn remember_release(
    config: &mut AppConfig,
    etag: Option<String>,
    update: Option<&AvailableUpdate>,
) {
    config.release_etag = etag;
    match update {
        Some(update) => {
            config.cached_tag = Some(update.tag.clone());
            config.cached_asset_name = Some(update.asset_name.clone());
            config.cached_asset_url = Some(update.url.clone());
            config.cached_checksums_url = Some(update.checksums_url.clone());
        }
        None => {
            config.cached_tag = None;
            config.cached_asset_name = None;
            config.cached_asset_url = None;
            config.cached_checksums_url = None;
        }
    }
}

fn header_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn is_rate_limited(status: StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if status == StatusCode::FORBIDDEN {
        return headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "0");
    }
    false
}

pub async fn check_latest(kind: AppKind) -> Result<Option<AvailableUpdate>> {
    let Some(asset_name) = identity::current_asset_name(kind) else {
        bail!("no GitHub asset name for this OS/architecture");
    };
    let client = update_client()?;
    let mut config = load_config();
    let mut request = client
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json");
    if let Some(etag) = &config.release_etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .await
        .context("failed to query GitHub releases")?;
    let status = response.status();
    let headers = response.headers().clone();
    if is_rate_limited(status, &headers) {
        return Ok(None);
    }
    if status == StatusCode::NOT_MODIFIED {
        return Ok(cached_update(&config).filter(|update| is_newer(&update.tag, VERSION)));
    }
    if !status.is_success() {
        bail!("GitHub releases request failed ({status})");
    }
    let etag = header_etag(&headers);
    let release: GhRelease = response
        .json()
        .await
        .context("failed to parse GitHub release JSON")?;

    if !is_newer(&release.tag_name, VERSION) {
        remember_release(&mut config, etag, None);
        let _ = save_config(&config);
        return Ok(None);
    }
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name == CHECKSUMS_NAME)
        .with_context(|| format!("release {} has no {CHECKSUMS_NAME}", release.tag_name))?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| format!("release {} has no asset {asset_name}", release.tag_name))?;
    if !is_repo_release_asset_url(&asset.browser_download_url, &release.tag_name, &asset.name)
        || !is_repo_release_asset_url(
            &checksums.browser_download_url,
            &release.tag_name,
            CHECKSUMS_NAME,
        )
    {
        bail!("release asset URL is not a danted4/torrenter GitHub download");
    }
    let update = AvailableUpdate {
        tag: release.tag_name,
        asset_name: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        checksums_url: checksums.browser_download_url.clone(),
    };
    remember_release(&mut config, etag, Some(&update));
    let _ = save_config(&config);
    Ok(Some(update))
}

pub async fn install_update(update: &AvailableUpdate) -> Result<PathBuf> {
    if !is_pinned_update(update) {
        bail!("refusing to install an update from outside danted4/torrenter");
    }
    let current = std::env::current_exe().context("cannot locate current executable")?;
    let tmp = current.with_file_name(format!(
        ".{}.update",
        current
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("torrenter")
    ));
    let sums_path = current.with_file_name(format!(
        ".{}.sha256sums",
        current
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("torrenter")
    ));
    let install = async {
        download_to_file(
            &update.checksums_url,
            &sums_path,
            MAX_CHECKSUMS_BYTES,
            &update.tag,
            CHECKSUMS_NAME,
        )
        .await?;
        let sums = tokio::fs::read_to_string(&sums_path).await?;
        let expected = hash_for_asset(&sums, &update.asset_name).with_context(|| {
            format!("{CHECKSUMS_NAME} has no SHA-256 for {}", update.asset_name)
        })?;
        download_to_file(
            &update.url,
            &tmp,
            MAX_UPDATE_BYTES,
            &update.tag,
            &update.asset_name,
        )
        .await?;
        let actual = sha256_file(&tmp).await?;
        if !hashes_match(&expected, &actual) {
            bail!(
                "update hash mismatch for {} (expected {expected}, got {actual})",
                update.asset_name
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&tmp)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&tmp, permissions)?;
        }
        replace_executable(&current, &tmp)?;
        Ok(current.clone())
    }
    .await;
    let _ = std::fs::remove_file(&sums_path);
    if install.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    install
}

async fn download_to_file(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    tag: &str,
    asset_name: &str,
) -> Result<()> {
    use futures_util::StreamExt;

    if !is_repo_release_asset_url(url, tag, asset_name) {
        bail!("refusing to download update from {url}");
    }
    // GitHub 302s this pinned github.com URL to objects.githubusercontent.com.
    // Following that redirect is required; the first hop is what we pin.
    let client = update_client()?;
    let response = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .context("failed to download release asset")?;
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            bail!("update asset is too large ({len} bytes)");
        }
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("truncated update download")?;
        written += chunk.len() as u64;
        if written > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            bail!("update asset exceeded size cap ({max_bytes} bytes)");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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

pub fn set_seed(enabled: bool) -> Result<AppConfig> {
    let mut config = load_config();
    config.seed = enabled;
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

    #[test]
    fn default_seed_is_on() {
        assert!(AppConfig::default().seed);
        let parsed: AppConfig = serde_json::from_str("{}").unwrap();
        assert!(parsed.seed);
        assert!(!parsed.auto_update);
    }

    #[test]
    fn parses_gnu_sha256sums() {
        let sums = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  torrenter-x86_64-unknown-linux-gnu\n\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *torrenter-gui-x86_64-unknown-linux-gnu\n";
        assert_eq!(
            hash_for_asset(sums, "torrenter-x86_64-unknown-linux-gnu").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            hash_for_asset(sums, "torrenter-gui-x86_64-unknown-linux-gnu").as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert!(hash_for_asset(sums, "missing").is_none());
    }

    #[test]
    fn pins_asset_urls_to_this_repo_and_tag() {
        let sums = "https://github.com/danted4/torrenter/releases/download/v3.0.2/SHA256SUMS";
        let cli = "https://github.com/danted4/torrenter/releases/download/v3.0.2/torrenter-x86_64-unknown-linux-gnu";
        assert!(is_repo_release_asset_url(sums, "v3.0.2", "SHA256SUMS"));
        assert!(is_repo_release_asset_url(
            cli,
            "v3.0.2",
            "torrenter-x86_64-unknown-linux-gnu"
        ));
        assert!(!is_repo_release_asset_url(sums, "v9.9.9", "SHA256SUMS"));
        assert!(!is_repo_release_asset_url(
            sums,
            "v3.0.2",
            "torrenter-x86_64-unknown-linux-gnu"
        ));
        assert!(!is_repo_release_asset_url(
            "https://github.com/other/repo/releases/download/v3.0.2/SHA256SUMS",
            "v3.0.2",
            "SHA256SUMS"
        ));
        assert!(!is_repo_release_asset_url(
            "https://objects.githubusercontent.com/github-production-release-asset-2e65be/foo",
            "v3.0.2",
            "foo"
        ));
        assert!(!is_repo_release_asset_url(
            "http://github.com/danted4/torrenter/releases/download/v3.0.2/SHA256SUMS",
            "v3.0.2",
            "SHA256SUMS"
        ));
        assert!(!is_repo_release_asset_url(
            "https://evil.example/payload",
            "v3.0.2",
            "payload"
        ));
    }

    #[test]
    fn cached_foreign_or_mismatched_tag_urls_are_dropped() {
        let name = "torrenter-x86_64-unknown-linux-gnu";
        let foreign = AppConfig {
            cached_tag: Some("v9.9.9".into()),
            cached_asset_name: Some(name.into()),
            cached_asset_url: Some(format!(
                "https://github.com/evil/repo/releases/download/v9.9.9/{name}"
            )),
            cached_checksums_url: Some(
                "https://github.com/evil/repo/releases/download/v9.9.9/SHA256SUMS".into(),
            ),
            ..AppConfig::default()
        };
        assert!(cached_update(&foreign).is_none());

        let wrong_tag = AppConfig {
            cached_tag: Some("v9.9.9".into()),
            cached_asset_name: Some(name.into()),
            cached_asset_url: Some(format!(
                "https://github.com/danted4/torrenter/releases/download/v3.0.0/{name}"
            )),
            cached_checksums_url: Some(
                "https://github.com/danted4/torrenter/releases/download/v3.0.0/SHA256SUMS".into(),
            ),
            ..AppConfig::default()
        };
        assert!(cached_update(&wrong_tag).is_none());
    }

    #[test]
    fn matching_hashes_are_case_insensitive() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(hashes_match(hash, &hash.to_ascii_uppercase()));
        assert!(!hashes_match(hash, &hash.replace('0', "1")));
    }
}

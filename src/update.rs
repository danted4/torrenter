use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use url::Url;

use crate::fsutil::{atomic_write, quarantine, recover_incomplete_save};
use crate::http::PART_COUNT;
use crate::identity::{self, AppKind, VERSION, VERSION_TAG};
use crate::progress::{JobSnapshot, ProgressHandle};

const USER_AGENT: &str = concat!("torrenter/", env!("CARGO_PKG_VERSION"));
const LATEST_URL: &str = "https://api.github.com/repos/danted4/torrenter/releases/latest";
const CHECKSUMS_NAME: &str = "SHA256SUMS";
const MAX_UPDATE_BYTES: u64 = 80 * 1024 * 1024;
const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;
const UPDATE_DOWNLOAD_ATTEMPTS: u32 = 4;
const UPDATE_RETRY_BASE: Duration = Duration::from_millis(400);

fn default_true() -> bool {
    true
}

fn default_theme() -> String {
    "phosphor".into()
}

pub const GLASS_TINT_MIN: u8 = 40;
pub const GLASS_TINT_MAX: u8 = 210;
/// 60% of the min..=max range (still 40–210).
pub const GLASS_TINT_DEFAULT: u8 =
    GLASS_TINT_MIN + ((GLASS_TINT_MAX - GLASS_TINT_MIN) as u16 * 60 / 100) as u8;

fn default_glass_tint() -> u8 {
    GLASS_TINT_DEFAULT
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default = "default_true")]
    pub seed: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_glass_tint")]
    pub glass_tint: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
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
    /// HTTP parts per download for new jobs; `None` is Auto (= `PART_COUNT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u8>,
    /// Remembered debrid route for magnet jobs (`torbox`, …); `None` is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debrid_route: Option<String>,
    /// Delete torrents torrenter added from the provider once every file landed.
    #[serde(default)]
    pub debrid_remove_after: bool,
    /// Keys written by newer builds, kept so a save here does not drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_update: false,
            seed: true,
            theme: default_theme(),
            glass_tint: default_glass_tint(),
            download_dir: None,
            release_etag: None,
            cached_tag: None,
            cached_asset_name: None,
            cached_asset_url: None,
            cached_checksums_url: None,
            connections: None,
            debrid_route: None,
            debrid_remove_after: false,
            extra: BTreeMap::new(),
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
    load_config_from(&path).unwrap_or_default()
}

/// `Err` only for a file that exists but cannot be read or parsed.
fn load_config_from(path: &Path) -> Result<AppConfig> {
    recover_incomplete_save(path);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppConfig::default())
        }
        Err(error) => return Err(error).context("failed to read config"),
    };
    serde_json::from_slice(&bytes).context("failed to parse config")
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    save_config_to(&config_path()?, config)
}

fn save_config_to(path: &Path, config: &AppConfig) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(config)?;
    atomic_write(path, &bytes, false)
}

/// Read-modify-write one setting. A config that no longer parses is moved to
/// `config.json.bak` first instead of being silently replaced by defaults.
pub fn edit_config(change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig> {
    edit_config_at(&config_path()?, change)
}

fn edit_config_at(path: &Path, change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig> {
    let mut config = match load_config_from(path) {
        Ok(config) => config,
        Err(_) => {
            quarantine(path);
            AppConfig::default()
        }
    };
    change(&mut config);
    save_config_to(path, &config)?;
    Ok(config)
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
    if let Ok(current) = running_exe_path() {
        for path in leftover_update_paths(&current) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Linux `/proc/self/exe` grows a ` (deleted)` suffix after we rename a new
/// binary over the running file. Strip it so relaunch/cleanup hit the live path.
fn without_linux_deleted_marker(path: &Path) -> PathBuf {
    const MARKER: &str = " (deleted)";
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.to_path_buf();
    };
    let Some(stripped) = name.strip_suffix(MARKER) else {
        return path.to_path_buf();
    };
    let mut out = path.to_path_buf();
    out.set_file_name(stripped);
    out
}

fn running_exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate current executable")?;
    Ok(without_linux_deleted_marker(&exe))
}

fn leftover_update_paths(current: &Path) -> Vec<PathBuf> {
    let name = current
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("torrenter");
    let mut paths = vec![
        old_binary_path(current),
        current.with_file_name(format!(".{name}.update")),
        current.with_file_name(format!("..{name}.update.merge.tmp")),
        current.with_file_name(format!(".{name}.sha256sums")),
    ];
    for index in 0..PART_COUNT {
        paths.push(current.with_file_name(format!(".{name}.update.part-{index}")));
    }
    paths
}

fn old_binary_path(current: &Path) -> PathBuf {
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    current.with_file_name(name)
}

fn enclosing_app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    app.file_name()?
        .to_str()?
        .ends_with(".app")
        .then(|| app.to_path_buf())
}

fn version_from_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

fn replace_plist_string(xml: &str, key: &str, value: &str) -> Result<String> {
    let header = format!("<key>{key}</key>");
    let Some(key_at) = xml.find(&header) else {
        bail!("Info.plist has no {key}");
    };
    let after_key = key_at + header.len();
    let rest = &xml[after_key..];
    let Some(string_at) = rest.find("<string>") else {
        bail!("Info.plist {key} has no string value");
    };
    if string_at > 80 {
        bail!("Info.plist {key} is not followed by a string");
    }
    let value_at = after_key + string_at + "<string>".len();
    let Some(end) = xml[value_at..].find("</string>") else {
        bail!("Info.plist {key} string is unclosed");
    };
    Ok(format!(
        "{}{value}{}",
        &xml[..value_at],
        &xml[value_at + end..]
    ))
}

fn stamp_app_bundle(app: &Path, tag: &str) -> Result<()> {
    let version = version_from_tag(tag);
    let plist_path = app.join("Contents/Info.plist");
    let xml = std::fs::read_to_string(&plist_path)
        .with_context(|| format!("read {}", plist_path.display()))?;
    let xml = replace_plist_string(&xml, "CFBundleShortVersionString", version)?;
    let xml = replace_plist_string(&xml, "CFBundleVersion", version)?;
    std::fs::write(&plist_path, xml).with_context(|| format!("write {}", plist_path.display()))?;
    Ok(())
}

fn refresh_installed_app(exe: &Path, tag: &str) {
    let Some(app) = enclosing_app_bundle(exe) else {
        return;
    };
    let _ = stamp_app_bundle(&app, tag);
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("codesign")
            .args(["--force", "--deep", "-s", "-"])
            .arg(&app)
            .status();
    }
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

fn persist_release_cache(etag: Option<String>, update: Option<&AvailableUpdate>) {
    let _ = edit_config(|config| remember_release(config, etag, update));
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
    let config = load_config();
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
        persist_release_cache(etag, None);
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
    persist_release_cache(etag, Some(&update));
    Ok(Some(update))
}

pub async fn install_update(update: &AvailableUpdate) -> Result<PathBuf> {
    install_with_progress(update, ProgressHandle::cli(1)).await
}

pub async fn install_update_reported(
    update: &AvailableUpdate,
    snapshot: Arc<JobSnapshot>,
) -> Result<PathBuf> {
    install_with_progress(update, ProgressHandle::gui(1, snapshot)).await
}

async fn install_with_progress(
    update: &AvailableUpdate,
    progress: ProgressHandle,
) -> Result<PathBuf> {
    if !is_pinned_update(update) {
        bail!("refusing to install an update from outside danted4/torrenter");
    }
    let current = running_exe_path()?;
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
    progress.set_title(&update.asset_name);
    progress.set_status("starting");
    progress.set_position(0);
    let install = async {
        download_to_file(
            &update.checksums_url,
            &sums_path,
            MAX_CHECKSUMS_BYTES,
            &update.tag,
            CHECKSUMS_NAME,
            &ProgressHandle::hidden(),
            false,
        )
        .await?;
        let sums = tokio::fs::read_to_string(&sums_path).await?;
        let expected = hash_for_asset(&sums, &update.asset_name).with_context(|| {
            format!("{CHECKSUMS_NAME} has no SHA-256 for {}", update.asset_name)
        })?;
        progress.set_status("downloading");
        progress.set_position(0);
        download_to_file(
            &update.url,
            &tmp,
            MAX_UPDATE_BYTES,
            &update.tag,
            &update.asset_name,
            &progress,
            true,
        )
        .await?;
        progress.set_status("verifying");
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
        refresh_installed_app(&current, &update.tag);
        progress.finish();
        Ok(current.clone())
    }
    .await;
    let _ = std::fs::remove_file(&sums_path);
    if install.is_err() {
        progress.finish_and_clear();
        let _ = std::fs::remove_file(&tmp);
    }
    install
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
    )
}

fn is_retryable_reqwest(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.status().is_some_and(is_retryable_status)
}

fn is_retryable_download(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_retryable_reqwest)
    })
}

async fn download_to_file(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    tag: &str,
    asset_name: &str,
    progress: &ProgressHandle,
    allow_range: bool,
) -> Result<()> {
    if !is_repo_release_asset_url(url, tag, asset_name) {
        bail!("refusing to download update from {url}");
    }
    // GitHub 302s this pinned github.com URL to objects.githubusercontent.com.
    // Following that redirect is required; the first hop is what we pin.
    let client = crate::http::range_client(USER_AGENT)?;
    let mut last_error = None;
    for attempt in 0..UPDATE_DOWNLOAD_ATTEMPTS {
        progress.set_position(0);
        match crate::http::download_file(&client, url, dest, max_bytes, allow_range, progress).await
        {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt + 1 < UPDATE_DOWNLOAD_ATTEMPTS && is_retryable_download(&error) =>
            {
                last_error = Some(error);
                let _ = tokio::fs::remove_file(dest).await;
                clear_update_parts(dest).await;
                tokio::time::sleep(UPDATE_RETRY_BASE * (1 << attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to download release asset")))
}

fn update_part_path(dest: &Path, index: usize) -> PathBuf {
    dest.with_file_name(format!(
        "{}.part-{index}",
        dest.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("update")
    ))
}

async fn clear_update_parts(dest: &Path) {
    for index in 0..PART_COUNT {
        let _ = tokio::fs::remove_file(update_part_path(dest, index)).await;
    }
    if let Some(name) = dest.file_name().and_then(|name| name.to_str()) {
        let _ = tokio::fs::remove_file(dest.with_file_name(format!(".{name}.merge.tmp"))).await;
    }
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
    let exe = running_exe_path()?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // Linux: exec() keeps the old Wayland/X11/GL fds and often targets a
    // `(deleted)` path after rename-over. Spawn a new process, then exit.
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new(&exe)
            .args(args)
            .spawn()
            .with_context(|| format!("failed to relaunch {}", exe.display()))?;
        std::process::exit(0);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
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
    edit_config(|config| config.auto_update = enabled)
}

pub fn set_seed(enabled: bool) -> Result<AppConfig> {
    edit_config(|config| config.seed = enabled)
}

pub fn normalize_theme(name: &str) -> &'static str {
    match name.trim().to_ascii_lowercase().as_str() {
        "amber" => "amber",
        "ice" => "ice",
        "scarlet" => "scarlet",
        "violet" => "violet",
        _ => "phosphor",
    }
}

pub fn set_theme(theme: impl AsRef<str>) -> Result<AppConfig> {
    edit_config(|config| config.theme = normalize_theme(theme.as_ref()).to_string())
}

pub fn normalize_glass_tint(value: u8) -> u8 {
    value.clamp(GLASS_TINT_MIN, GLASS_TINT_MAX)
}

pub fn glass_tint_percent(value: u8) -> u8 {
    let value = u16::from(normalize_glass_tint(value));
    let span = u16::from(GLASS_TINT_MAX - GLASS_TINT_MIN);
    ((value - u16::from(GLASS_TINT_MIN)) * 100 / span) as u8
}

pub fn glass_tint_from_percent(percent: u8) -> u8 {
    let percent = u16::from(percent.min(100));
    let span = u16::from(GLASS_TINT_MAX - GLASS_TINT_MIN);
    GLASS_TINT_MIN + (percent * span / 100) as u8
}

/// Paint tint: native fullscreen uses 100%; otherwise the saved windowed value.
pub fn effective_glass_tint(saved: u8, fullscreen: bool) -> u8 {
    if fullscreen {
        glass_tint_from_percent(100)
    } else {
        normalize_glass_tint(saved)
    }
}

pub fn set_glass_tint(value: u8) -> Result<AppConfig> {
    edit_config(|config| config.glass_tint = normalize_glass_tint(value))
}

/// Windows has no frost. Ignore a leftover slider value and use 100%.
pub fn windows_glass_tint(_saved: u8) -> u8 {
    glass_tint_from_percent(100)
}

/// Rewrite `config.json` when a Windows install still has a pre-opaque tint.
pub fn snap_windows_glass_tint(saved: u8) -> u8 {
    let max = windows_glass_tint(saved);
    if normalize_glass_tint(saved) != max {
        let _ = set_glass_tint(max);
    }
    max
}

pub fn set_connections(parts: Option<u8>) -> Result<AppConfig> {
    edit_config(|config| config.connections = parts.map(clamp_connections))
}

/// Parts for a new HTTP download: the setting, or Auto (= `PART_COUNT`).
pub fn effective_connections(config: &AppConfig) -> usize {
    config
        .connections
        .map(|parts| usize::from(clamp_connections(parts)))
        .unwrap_or(PART_COUNT)
}

pub const MAX_CONNECTIONS: u8 = 16;

fn clamp_connections(parts: u8) -> u8 {
    parts.clamp(1, MAX_CONNECTIONS)
}

pub fn set_debrid_route(service: Option<&str>) -> Result<AppConfig> {
    let service = service.map(|id| id.trim().to_ascii_lowercase());
    edit_config(|config| config.debrid_route = service.filter(|id| !id.is_empty()))
}

pub fn set_debrid_remove_after(enabled: bool) -> Result<AppConfig> {
    edit_config(|config| config.debrid_remove_after = enabled)
}

pub fn set_download_dir(path: impl AsRef<Path>) -> Result<AppConfig> {
    let stored = crate::paths::persistable_dir(path)?;
    edit_config(|config| config.download_dir = Some(stored))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_keeps_unknown_keys_and_quarantines_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"seed":false,"future_key":{"x":1}}"#).unwrap();
        let config = edit_config_at(&path, |config| config.auto_update = true).unwrap();
        assert!(!config.seed);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["future_key"]["x"], 1);
        assert_eq!(raw["auto_update"], true);

        std::fs::write(&path, "{not json").unwrap();
        assert!(load_config_from(&path).is_err());
        let config = edit_config_at(&path, |config| config.theme = "ice".into()).unwrap();
        assert_eq!(config.theme, "ice");
        assert!(
            crate::fsutil::bak_path(&path).exists(),
            "broken config kept aside"
        );
    }

    #[test]
    fn connections_default_to_auto_and_clamp() {
        let mut config = AppConfig::default();
        assert_eq!(effective_connections(&config), PART_COUNT);
        config.connections = Some(0);
        assert_eq!(effective_connections(&config), 1);
        config.connections = Some(99);
        assert_eq!(effective_connections(&config), usize::from(MAX_CONNECTIONS));
        let text = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(!text.contains("connections") && !text.contains("debrid_route"));
    }

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
        assert_eq!(parsed.theme, "phosphor");
        assert_eq!(parsed.glass_tint, GLASS_TINT_DEFAULT);
        assert!(parsed.download_dir.is_none());
    }

    #[test]
    fn release_cache_reload_keeps_theme_and_other_prefs() {
        let mut stale = AppConfig {
            theme: "phosphor".into(),
            auto_update: false,
            ..AppConfig::default()
        };
        remember_release(&mut stale, Some("W/\"new\"".into()), None);
        assert_eq!(stale.theme, "phosphor");

        let mut disk = AppConfig {
            theme: "ice".into(),
            auto_update: true,
            seed: false,
            download_dir: Some(PathBuf::from("/tmp/torrenter-out")),
            ..AppConfig::default()
        };
        remember_release(&mut disk, Some("W/\"new\"".into()), None);
        assert_eq!(disk.theme, "ice");
        assert!(disk.auto_update);
        assert!(!disk.seed);
        assert_eq!(
            disk.download_dir.as_deref(),
            Some(Path::new("/tmp/torrenter-out"))
        );
        assert_eq!(disk.release_etag.as_deref(), Some("W/\"new\""));
        assert!(disk.cached_tag.is_none());
    }

    #[test]
    fn theme_roundtrips_and_unknown_falls_back() {
        let config = AppConfig {
            theme: "amber".into(),
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.theme, "amber");
        assert_eq!(normalize_theme("Ice"), "ice");
        assert_eq!(normalize_theme("scarlet"), "scarlet");
        assert_eq!(normalize_theme("violet"), "violet");
        assert_eq!(normalize_theme("blood"), "phosphor");
        assert_eq!(normalize_theme("  AMBER "), "amber");
        assert_eq!(normalize_theme("nope"), "phosphor");
        assert_eq!(normalize_theme(""), "phosphor");
        let parsed: AppConfig = serde_json::from_str(r#"{"theme":"ice"}"#).unwrap();
        assert_eq!(normalize_theme(&parsed.theme), "ice");
    }

    #[test]
    fn glass_tint_clamps_and_defaults() {
        assert_eq!(normalize_glass_tint(0), GLASS_TINT_MIN);
        assert_eq!(normalize_glass_tint(255), GLASS_TINT_MAX);
        assert_eq!(glass_tint_from_percent(0), GLASS_TINT_MIN);
        assert_eq!(glass_tint_from_percent(100), GLASS_TINT_MAX);
        assert_eq!(glass_tint_from_percent(60), GLASS_TINT_DEFAULT);
        assert_eq!(glass_tint_percent(GLASS_TINT_DEFAULT), 60);
        let parsed: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.glass_tint, GLASS_TINT_DEFAULT);
        let config = AppConfig {
            glass_tint: 90,
            ..AppConfig::default()
        };
        let parsed: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.glass_tint, 90);
    }

    #[test]
    fn fullscreen_uses_max_tint_without_changing_saved() {
        let saved = glass_tint_from_percent(60);
        assert_eq!(effective_glass_tint(saved, true), GLASS_TINT_MAX);
        assert_eq!(
            effective_glass_tint(saved, true),
            glass_tint_from_percent(100)
        );
        assert_eq!(effective_glass_tint(saved, false), saved);
        assert_eq!(saved, GLASS_TINT_DEFAULT);
    }

    #[test]
    fn windows_tint_snaps_to_max_regardless_of_saved() {
        assert_eq!(windows_glass_tint(0), GLASS_TINT_MAX);
        assert_eq!(windows_glass_tint(40), GLASS_TINT_MAX);
        assert_eq!(windows_glass_tint(GLASS_TINT_DEFAULT), GLASS_TINT_MAX);
        assert_eq!(windows_glass_tint(GLASS_TINT_MAX), GLASS_TINT_MAX);
        assert_eq!(glass_tint_percent(windows_glass_tint(90)), 100);
    }

    #[test]
    fn download_dir_roundtrips() {
        let config = AppConfig {
            download_dir: Some(PathBuf::from("/tmp/torrenter-out")),
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.download_dir.as_deref(),
            Some(Path::new("/tmp/torrenter-out"))
        );
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

    #[test]
    fn retries_github_cdn_gateway_failures() {
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert_eq!(UPDATE_DOWNLOAD_ATTEMPTS, 4);
    }

    #[test]
    fn strips_linux_deleted_marker_from_proc_exe() {
        assert_eq!(
            without_linux_deleted_marker(Path::new("/opt/torrenter-gui (deleted)")),
            PathBuf::from("/opt/torrenter-gui")
        );
        assert_eq!(
            without_linux_deleted_marker(Path::new("/opt/torrenter-gui")),
            PathBuf::from("/opt/torrenter-gui")
        );
    }

    #[test]
    fn leftover_paths_sit_beside_the_running_binary() {
        let current = PathBuf::from("/Applications/torrenter.app/Contents/MacOS/torrenter-gui");
        let leftovers = leftover_update_paths(&current);
        assert!(leftovers
            .iter()
            .any(|path| path.ends_with("torrenter-gui.old")));
        assert!(leftovers
            .iter()
            .any(|path| path.ends_with(".torrenter-gui.update")));
        assert!(leftovers
            .iter()
            .any(|path| path.ends_with(".torrenter-gui.sha256sums")));
        assert!(leftovers
            .iter()
            .any(|path| path.ends_with(".torrenter-gui.update.part-0")));
        assert!(leftovers
            .iter()
            .any(|path| path.ends_with("..torrenter-gui.update.merge.tmp")));
    }

    #[test]
    fn app_bundle_is_the_shell_around_macos_binary() {
        let exe = PathBuf::from("/Applications/torrenter.app/Contents/MacOS/torrenter-gui");
        assert_eq!(
            enclosing_app_bundle(&exe).as_deref(),
            Some(Path::new("/Applications/torrenter.app"))
        );
        assert!(enclosing_app_bundle(Path::new("/tmp/torrenter-gui")).is_none());
    }

    #[test]
    fn plist_version_strings_are_replaced() {
        let xml = r#"<key>CFBundleShortVersionString</key>
	<string>3.2.1</string>
	<key>CFBundleVersion</key>
	<string>3.2.1</string>"#;
        let xml = replace_plist_string(xml, "CFBundleShortVersionString", "3.2.3").unwrap();
        let xml = replace_plist_string(&xml, "CFBundleVersion", "3.2.3").unwrap();
        assert!(xml.contains("<string>3.2.3</string>"));
        assert!(!xml.contains("3.2.1"));
        assert_eq!(version_from_tag("v3.2.3"), "3.2.3");
    }
}

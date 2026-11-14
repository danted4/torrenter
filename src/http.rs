use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;
use url::Url;

use crate::progress::{
    abort_if_paused, check_stop, honor_control, is_pause_abort, is_stopped, output_lock,
    until_stop, until_stop_or_pause, wait_while_paused, watch_cli_controls, DownloadPaused,
    DownloadStopped, JobSnapshot, ProgressHandle,
};
use crate::validate::is_valid_download_url;

pub const PART_COUNT: usize = 4;
const RESUME_DIR_NAME: &str = ".resume";
const MERGE_CHUNK: usize = 1024 * 1024;

pub(crate) fn range_client(user_agent: &str) -> Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .gzip(false)
        .brotli(false)
        .build()
        .context("failed to build HTTP client")
}

#[derive(Clone, Debug)]
enum PartPlacement {
    Resume,
    Beside { stem: String },
}

#[derive(Clone, Debug)]
pub struct DownloadLayout {
    pub directory: PathBuf,
    pub part_count: usize,
    parts: PartPlacement,
    /// `.resume/` lives here instead of under `directory` (debrid: job root).
    resume_root: Option<PathBuf>,
    /// Resume identity used instead of the request URL (debrid links change and
    /// may carry the API key, so they must never key or enter the manifest).
    identity: Option<String>,
}

impl DownloadLayout {
    pub fn in_dir(directory: PathBuf) -> Self {
        Self {
            directory,
            part_count: PART_COUNT,
            parts: PartPlacement::Resume,
            resume_root: None,
            identity: None,
        }
    }

    pub fn with_part_count(mut self, part_count: usize) -> Self {
        self.part_count = part_count.max(1);
        self
    }

    /// One file of a multi-file job: output in `directory`, parts and manifest
    /// under `<resume_root>/.resume/`, keyed by `identity` (no URL).
    pub(crate) fn for_job_file(
        directory: PathBuf,
        resume_root: PathBuf,
        identity: String,
        part_count: usize,
    ) -> Self {
        Self {
            directory,
            part_count: part_count.max(1),
            parts: PartPlacement::Resume,
            resume_root: Some(resume_root),
            identity: Some(identity),
        }
    }

    fn resume_identity<'a>(&'a self, download_url: &'a str) -> &'a str {
        self.identity.as_deref().unwrap_or(download_url)
    }

    pub fn beside_file(dest: &Path) -> Self {
        let stem = dest
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download")
            .to_string();
        let directory = dest
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self {
            directory,
            part_count: PART_COUNT,
            parts: PartPlacement::Beside { stem },
            resume_root: None,
            identity: None,
        }
    }
}

/// Write `url` to `dest`. Parts sit beside the file (`{dest}.part-N`), never under dest/.resume.
/// No resume. `max_bytes` is a hard cap. 206 with a mismatched Content-Range total falls back to GET.
pub(crate) async fn download_file(
    client: &Client,
    url: &str,
    dest: &Path,
    max_bytes: u64,
    allow_range: bool,
    progress: &ProgressHandle,
) -> Result<()> {
    let downloaded = Arc::new(AtomicU64::new(0));
    let layout = DownloadLayout::beside_file(dest);
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download")
        .to_string();
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }
    if allow_range {
        if let Some(total) = probe_range_total(client, url).await {
            if total > max_bytes {
                bail!("update asset is too large ({total} bytes)");
            }
            if total > 1 {
                progress.set_length(total);
                match download_multipart(
                    client,
                    url,
                    &filename,
                    total,
                    progress,
                    &downloaded,
                    &layout,
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(error) if error.downcast_ref::<RangesNotHonored>().is_some() => {
                        remove_resume_files(&layout, &resume_key(url)).await?;
                        let _ = fs::remove_file(dest).await;
                        downloaded.store(0, Ordering::Relaxed);
                        progress.set_position(0);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    stream_single(
        client,
        url,
        dest,
        progress,
        &downloaded,
        Some(max_bytes),
        None,
    )
    .await
}

#[derive(Debug)]
struct RangesNotHonored;

impl std::fmt::Display for RangesNotHonored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The server did not honor ranged requests")
    }
}

impl std::error::Error for RangesNotHonored {}

/// A job-file link answered 401/403/404/410 (expired or IP-bound). Only raised
/// for layouts with an identity, so plain HTTP keeps its old fallbacks.
#[derive(Debug)]
pub struct LinkRejected(pub u16);

impl std::fmt::Display for LinkRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "download link rejected (HTTP {})", self.0)
    }
}

impl std::error::Error for LinkRejected {}

fn link_rejected(layout: &DownloadLayout, response: &reqwest::Response) -> Option<LinkRejected> {
    layout.identity.as_ref()?;
    let status = response.status().as_u16();
    let invalid_code = response.headers().contains_key("x-error");
    (matches!(status, 401 | 403 | 404 | 410) || (invalid_code && !response.status().is_success()))
        .then_some(LinkRejected(status))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeManifest {
    url: String,
    size: u64,
    part_count: usize,
    filename: String,
}

struct RemoteMetadata {
    size: Option<u64>,
    filename: String,
}

/// One file of a multi-file job (debrid) whose size and name the provider
/// reported, so no HEAD probe can silently disable resume. Parts resume under
/// the job's `.resume/` keyed by the layout identity, so a fresh link keeps
/// them. Never finishes the job or changes its title; the caller owns stop.
pub(crate) async fn download_job_file(
    client: &Client,
    download_url: &str,
    layout: &DownloadLayout,
    filename: &str,
    size: u64,
    progress: &ProgressHandle,
) -> Result<()> {
    fs::create_dir_all(&layout.directory)
        .await
        .context("failed to create download folder")?;
    let output_path = layout.directory.join(filename);
    let downloaded = Arc::new(AtomicU64::new(0));
    if size > 0 {
        progress.set_length(size);
    }
    loop {
        check_stop(progress.snapshot().map(Arc::as_ref))?;
        wait_while_paused(progress.snapshot().map(Arc::as_ref)).await?;
        let transfer = async {
            if size > 1 {
                match download_multipart(
                    client,
                    download_url,
                    filename,
                    size,
                    progress,
                    &downloaded,
                    layout,
                )
                .await
                {
                    Err(error) if error.downcast_ref::<RangesNotHonored>().is_some() => {
                        downloaded.store(0, Ordering::Relaxed);
                        progress.set_position(0);
                        stream_single(
                            client,
                            download_url,
                            &output_path,
                            progress,
                            &downloaded,
                            None,
                            Some(layout),
                        )
                        .await
                    }
                    other => other,
                }
            } else {
                stream_single(
                    client,
                    download_url,
                    &output_path,
                    progress,
                    &downloaded,
                    None,
                    Some(layout),
                )
                .await
            }
        };
        match transfer.await {
            Ok(()) => return Ok(()),
            Err(error) if is_pause_abort(&error) => continue,
            Err(error) => return Err(error),
        }
    }
}

pub async fn start_url_download(download_url: &str, dest: PathBuf) -> Result<()> {
    start_url_download_in(download_url, DownloadLayout::in_dir(dest)).await
}

pub async fn start_url_download_reported(
    download_url: &str,
    snapshot: Arc<JobSnapshot>,
    dest: PathBuf,
    part_count: usize,
) -> Result<()> {
    let layout = DownloadLayout::in_dir(dest).with_part_count(part_count);
    run_url_download(download_url, layout, Some(snapshot)).await
}

fn http_lock_key(directory: &Path, download_url: &str) -> String {
    format!("http:{}\n{download_url}", directory.display())
}

pub async fn start_url_download_in(download_url: &str, layout: DownloadLayout) -> Result<()> {
    run_url_download(download_url, layout, None).await
}

#[cfg(test)]
pub(crate) async fn start_url_download_reported_in(
    download_url: &str,
    layout: DownloadLayout,
    snapshot: Arc<JobSnapshot>,
) -> Result<()> {
    run_url_download(download_url, layout, Some(snapshot)).await
}

async fn run_url_download(
    download_url: &str,
    layout: DownloadLayout,
    snapshot: Option<Arc<JobSnapshot>>,
) -> Result<()> {
    if !is_valid_download_url(download_url) {
        bail!("Invalid HTTP(S) URL!");
    }

    let client = range_client(concat!(
        env!("CARGO_PKG_NAME"),
        "/",
        env!("CARGO_PKG_VERSION")
    ))?;

    let progress = match snapshot {
        Some(snapshot) => ProgressHandle::gui(1, snapshot),
        None => ProgressHandle::cli(1),
    };
    if progress.announce() {
        println!("\nPreparing direct download...\n");
        println!("[1] pause/resume   [2] stop\n");
    }
    progress.set_message("0");
    let downloaded = Arc::new(AtomicU64::new(0));
    // A restart of the same URL into the same folder waits until the stopped task
    // has drained its part tasks and flushed every write (see `stream_into_file`).
    let output = output_lock(&http_lock_key(&layout.directory, download_url));
    let output_guard = tokio::select! {
        guard = output.lock() => Some(guard),
        _ = until_stop(progress.snapshot().map(Arc::as_ref)) => None,
    };

    let work = async {
        let metadata = tokio::select! {
            metadata = get_remote_metadata(&client, download_url) => metadata,
            _ = until_stop(progress.snapshot().map(Arc::as_ref)) => bail!(DownloadStopped),
        };
        let filename = metadata
            .as_ref()
            .map(|meta| meta.filename.clone())
            .unwrap_or_else(|| filename_from_url(download_url, None));
        let total = metadata.as_ref().and_then(|meta| meta.size).unwrap_or(0);
        progress.set_title(&filename);
        if total > 0 {
            progress.set_length(total);
        }
        if progress.announce() {
            println!("{filename}\n");
        }
        fs::create_dir_all(&layout.directory)
            .await
            .context("failed to create downloads directory")?;
        let output_path = layout.directory.join(&filename);
        loop {
            check_stop(progress.snapshot().map(Arc::as_ref))?;
            wait_while_paused(progress.snapshot().map(Arc::as_ref)).await?;
            let transfer = async {
                if total > 0 {
                    match download_multipart(
                        &client,
                        download_url,
                        &filename,
                        total,
                        &progress,
                        &downloaded,
                        &layout,
                    )
                    .await
                    {
                        Ok(()) => Ok(()),
                        Err(error) if error.downcast_ref::<RangesNotHonored>().is_some() => {
                            downloaded.store(0, Ordering::Relaxed);
                            progress.set_position(0);
                            // Single-stream fallback does not resume; it truncates the destination.
                            stream_single(
                                &client,
                                download_url,
                                &output_path,
                                &progress,
                                &downloaded,
                                None,
                                None,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    stream_single(
                        &client,
                        download_url,
                        &output_path,
                        &progress,
                        &downloaded,
                        None,
                        None,
                    )
                    .await
                }
            };
            match transfer.await {
                Ok(()) => return Ok(()),
                Err(error) if is_pause_abort(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    };

    let mut work = std::pin::pin!(work);
    let result = if output_guard.is_none() {
        Err(DownloadStopped.into())
    } else {
        tokio::select! {
        result = &mut work => result,
        _ = async {
            if progress.announce() {
                if let Some(snapshot) = progress.snapshot() {
                    watch_cli_controls(Arc::clone(snapshot)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } else {
                until_stop(progress.snapshot().map(Arc::as_ref)).await;
            }
        } => {
            if let Some(snapshot) = progress.snapshot() {
                snapshot.request_stop();
            }
            // Let the transfer see the stop and finish its file I/O; dropping it here
            // would leave tokio writes landing after the output lock is released.
            match work.await {
                Ok(()) => Ok(()),
                Err(error) if is_stopped(&error) => Err(error),
                Err(_) => Err(DownloadStopped.into()),
            }
        }
        }
    };
    drop(output_guard);

    match result {
        Ok(()) => {
            let filename = progress
                .snapshot()
                .map(|snapshot| snapshot.title())
                .unwrap_or_default();
            let bytes = downloaded.load(Ordering::Relaxed);
            let final_total = progress.length().unwrap_or(0).max(bytes).max(1);
            progress.set_length(final_total);
            progress.set_position(final_total);
            progress.finish();
            if progress.announce() {
                println!("\nDirect download finished: {filename}");
            }
            Ok(())
        }
        Err(error) if is_stopped(&error) => {
            if let Some(snapshot) = progress.snapshot() {
                snapshot.mark_stopped();
            }
            progress.finish_and_clear();
            if progress.announce() {
                println!("\nDirect download stopped.");
            }
            Ok(())
        }
        Err(error) => {
            progress.finish_and_clear();
            Err(error.context("Direct download failed"))
        }
    }
}

async fn get_remote_metadata(client: &Client, download_url: &str) -> Option<RemoteMetadata> {
    let response = client.head(download_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let headers = response.headers();
    let head_size = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0);
    let filename = filename_from_url(download_url, headers.get(CONTENT_DISPOSITION));
    let size = match probe_range_total(client, download_url).await {
        Some(range_total) => Some(range_total),
        None => head_size,
    };
    Some(RemoteMetadata { size, filename })
}

async fn probe_range_total(client: &Client, download_url: &str) -> Option<u64> {
    let response = client
        .get(download_url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .ok()?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return None;
    }
    parse_content_range_total(response.headers().get(CONTENT_RANGE)?)
}

fn parse_content_range_total(value: &reqwest::header::HeaderValue) -> Option<u64> {
    let text = value.to_str().ok()?;
    let total = text.rsplit('/').next()?;
    if total == "*" {
        return None;
    }
    total.parse().ok().filter(|size| *size > 0)
}

pub(crate) fn filename_from_url(
    download_url: &str,
    disposition: Option<&reqwest::header::HeaderValue>,
) -> String {
    let from_disposition = disposition
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_disposition);
    let from_url = Url::parse(download_url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|name| name.to_string())
        })
        .filter(|name| !name.is_empty());
    let raw = from_disposition
        .or(from_url)
        .unwrap_or_else(|| "download.bin".to_string());
    sanitize_filename(&decode_filename(&raw))
}

fn parse_content_disposition(header: &str) -> Option<String> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r#"(?i)filename\*?=(?:UTF-8''|"?)([^";]+)"#).expect("valid regex")
    });
    pattern
        .captures(header)
        .and_then(|caps| caps.get(1))
        .map(|value| value.as_str().trim_matches('"').to_string())
}

fn decode_filename(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

fn sanitize_filename(name: &str) -> String {
    let without_nuls = name.replace('\0', "");
    let component = Path::new(&without_nuls)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let cleaned: String = component
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download.bin".to_string()
    } else {
        cleaned
    }
}

fn part_range(index: usize, part_size: u64, total: u64) -> Option<(u64, u64, u64)> {
    let start = index as u64 * part_size;
    if start >= total {
        return None;
    }
    let end = (total - 1).min(start + part_size - 1);
    Some((start, end, end - start + 1))
}

/// True when dest/.resume has a matching manifest and at least one part with bytes.
pub fn http_resume_present(dest: &Path, download_url: &str) -> bool {
    let layout = DownloadLayout::in_dir(dest.to_path_buf());
    let key = resume_key(download_url);
    let Ok(bytes) = std::fs::read(manifest_path(&layout, &key)) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<ResumeManifest>(&bytes) else {
        return false;
    };
    if manifest.url != download_url {
        return false;
    }
    (0..manifest.part_count).any(|index| {
        std::fs::metadata(part_path(&layout, &key, index))
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    })
}

pub fn http_output_present(dest: &Path, filename: &str) -> bool {
    let name = Path::new(filename);
    if filename.is_empty() || name.components().count() != 1 {
        return false;
    }
    std::fs::metadata(dest.join(name))
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

pub fn http_payload_present(dest: &Path, download_url: &str, filename: &str) -> bool {
    http_resume_present(dest, download_url) || http_output_present(dest, filename)
}

fn resume_key(download_url: &str) -> String {
    let digest = Sha256::digest(download_url.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn resume_directory(layout: &DownloadLayout) -> PathBuf {
    layout
        .resume_root
        .as_deref()
        .unwrap_or(&layout.directory)
        .join(RESUME_DIR_NAME)
}

fn manifest_path(layout: &DownloadLayout, key: &str) -> PathBuf {
    resume_directory(layout).join(format!("{key}.json"))
}

fn part_path(layout: &DownloadLayout, key: &str, index: usize) -> PathBuf {
    match &layout.parts {
        PartPlacement::Resume => resume_directory(layout).join(format!("{key}.part-{index}")),
        PartPlacement::Beside { stem } => layout.directory.join(format!("{stem}.part-{index}")),
    }
}

async fn get_resume_manifest(layout: &DownloadLayout, key: &str) -> Option<ResumeManifest> {
    let bytes = fs::read(manifest_path(layout, key)).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn save_resume_manifest(
    layout: &DownloadLayout,
    key: &str,
    manifest: &ResumeManifest,
) -> Result<()> {
    fs::create_dir_all(resume_directory(layout)).await?;
    let json = serde_json::to_vec(manifest)?;
    fs::write(manifest_path(layout, key), json).await?;
    Ok(())
}

async fn remove_resume_files(layout: &DownloadLayout, key: &str) -> Result<()> {
    // A manifest from a run with more parts (the setting changed) owns the
    // higher-index files too; remove those instead of orphaning them.
    let mut count = layout.part_count;
    if matches!(layout.parts, PartPlacement::Resume) {
        if let Some(manifest) = get_resume_manifest(layout, key).await {
            count = count.max(manifest.part_count);
        }
        let _ = fs::remove_file(manifest_path(layout, key)).await;
    }
    for index in 0..count {
        let _ = fs::remove_file(part_path(layout, key, index)).await;
    }
    Ok(())
}

async fn download_multipart(
    client: &Client,
    download_url: &str,
    filename: &str,
    total: u64,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
    layout: &DownloadLayout,
) -> Result<()> {
    let identity = layout.resume_identity(download_url);
    let key = resume_key(identity);
    let part_size = total.div_ceil(layout.part_count as u64);
    let resume = matches!(layout.parts, PartPlacement::Resume);
    if resume {
        let manifest = get_resume_manifest(layout, &key).await;
        let can_resume = manifest.as_ref().is_some_and(|manifest| {
            manifest.url == identity
                && manifest.size == total
                && manifest.part_count == layout.part_count
        });
        if !can_resume {
            remove_resume_files(layout, &key).await?;
        }
        save_resume_manifest(
            layout,
            &key,
            &ResumeManifest {
                url: identity.to_string(),
                size: total,
                part_count: layout.part_count,
                filename: filename.to_string(),
            },
        )
        .await?;
    } else {
        remove_resume_files(layout, &key).await?;
    }

    let mut existing_sizes = Vec::with_capacity(layout.part_count);
    for index in 0..layout.part_count {
        let expected = part_range(index, part_size, total)
            .map(|range| range.2)
            .unwrap_or(0);
        existing_sizes
            .push(truncate_part_to_valid_len(&part_path(layout, &key, index), expected).await?);
    }

    let already = existing_sizes.iter().sum::<u64>();
    downloaded.store(already, Ordering::Relaxed);
    progress.set_position(already);

    let mut tasks = JoinSet::new();
    for (index, existing) in existing_sizes.iter().copied().enumerate() {
        let client = client.clone();
        let download_url = download_url.to_string();
        let key = key.clone();
        let progress = progress.clone();
        let downloaded = Arc::clone(downloaded);
        let layout = layout.clone();
        tasks.spawn(async move {
            download_part(
                &client,
                &download_url,
                &layout,
                &key,
                index,
                part_size,
                total,
                existing,
                &progress,
                &downloaded,
            )
            .await
        });
    }

    let mut range_failure = false;
    loop {
        tokio::select! {
            joined = tasks.join_next() => {
                let Some(joined) = joined else {
                    break;
                };
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) if error.downcast_ref::<RangesNotHonored>().is_some() => {
                        range_failure = true;
                        tasks.abort_all();
                    }
                    Ok(Err(error)) if is_stopped(&error) || is_pause_abort(&error) => {
                        // Parts exit on their own at the next network wait; never abort
                        // them mid file op (see `until_stop_or_pause`).
                        while tasks.join_next().await.is_some() {}
                        return Err(error);
                    }
                    Ok(Err(error)) => {
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        return Err(error);
                    }
                    Err(join_error) if join_error.is_cancelled() => {}
                    Err(join_error) => {
                        return Err(join_error).context("download part task failed");
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(120)) => {
                if progress.should_stop() {
                    while tasks.join_next().await.is_some() {}
                    anyhow::bail!(DownloadStopped);
                }
                if progress.is_paused() {
                    while tasks.join_next().await.is_some() {}
                    anyhow::bail!(DownloadPaused);
                }
            }
        }
    }
    if range_failure {
        remove_resume_files(layout, &key).await?;
        return Err(RangesNotHonored.into());
    }

    merge_parts(layout, &key, filename, progress, downloaded, total).await?;
    remove_resume_files(layout, &key).await?;
    Ok(())
}

async fn truncate_part_to_valid_len(path: &Path, expected: u64) -> Result<u64> {
    let Ok(meta) = fs::metadata(path).await else {
        return Ok(0);
    };
    let len = meta.len();
    if len > expected {
        let file = OpenOptions::new().write(true).open(path).await?;
        file.set_len(expected).await?;
        file.sync_all().await?;
        return Ok(expected);
    }
    Ok(len)
}

#[allow(clippy::too_many_arguments)]
async fn download_part(
    client: &Client,
    download_url: &str,
    layout: &DownloadLayout,
    key: &str,
    index: usize,
    part_size: u64,
    total: u64,
    existing_size: u64,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
) -> Result<()> {
    let Some((start, end, expected)) = part_range(index, part_size, total) else {
        return Ok(());
    };
    if existing_size == expected {
        return Ok(());
    }

    check_stop(progress.snapshot().map(Arc::as_ref))?;
    abort_if_paused(progress.snapshot().map(Arc::as_ref))?;
    let range_start = start + existing_size;
    let request = client
        .get(download_url)
        .header(RANGE, format!("bytes={range_start}-{end}"))
        .send();
    let response = tokio::select! {
        response = request => response.context("ranged request failed")?,
        _ = until_stop_or_pause(progress.snapshot().map(Arc::as_ref)) => {
            check_stop(progress.snapshot().map(Arc::as_ref))?;
            bail!(DownloadPaused);
        }
    };
    if let Some(rejected) = link_rejected(layout, &response) {
        bail!(rejected);
    }
    if response.status() != StatusCode::PARTIAL_CONTENT {
        bail!(RangesNotHonored);
    }
    if let Some(range_total) = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(parse_content_range_total)
    {
        if range_total != total {
            bail!(RangesNotHonored);
        }
    }

    let path = part_path(layout, key, index);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(existing_size == 0)
        .open(&path)
        .await?;
    file.set_len(existing_size).await?;
    file.seek(SeekFrom::Start(existing_size)).await?;
    stream_into_file(
        response,
        &mut file,
        progress,
        downloaded,
        Some(expected - existing_size),
        None,
    )
    .await?;
    file.flush().await?;
    let completed = file.metadata().await?.len();
    if completed > expected {
        file.set_len(expected).await?;
        file.flush().await?;
    }
    let completed = fs::metadata(&path).await?.len();
    if completed != expected {
        bail!("A download part was incomplete");
    }
    Ok(())
}

async fn stream_single(
    client: &Client,
    download_url: &str,
    output_path: &Path,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
    byte_cap: Option<u64>,
    job_file: Option<&DownloadLayout>,
) -> Result<()> {
    let response = tokio::select! {
        response = client.get(download_url).send() => response.context("download request failed")?,
        _ = until_stop_or_pause(progress.snapshot().map(Arc::as_ref)) => {
            check_stop(progress.snapshot().map(Arc::as_ref))?;
            bail!(DownloadPaused);
        }
    };
    if let Some(rejected) = job_file.and_then(|layout| link_rejected(layout, &response)) {
        bail!(rejected);
    }
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(error) => return Err(error.into()),
    };
    if let Some(len) = response.content_length().filter(|len| *len > 0) {
        if byte_cap.is_some_and(|cap| len > cap) {
            bail!("update asset is too large ({len} bytes)");
        }
        progress.set_length(len);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_path)
        .await
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    stream_into_file(response, &mut file, progress, downloaded, None, byte_cap).await?;
    file.flush().await?;
    Ok(())
}

async fn stream_into_file(
    response: reqwest::Response,
    file: &mut File,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
    max_bytes: Option<u64>,
    byte_cap: Option<u64>,
) -> Result<()> {
    let mut remaining = max_bytes;
    let mut stream = response.bytes_stream();
    let result: Result<()> = async {
        loop {
            check_stop(progress.snapshot().map(Arc::as_ref))?;
            abort_if_paused(progress.snapshot().map(Arc::as_ref))?;
            // Only the network read is raced; writes below always run to completion.
            let next = tokio::select! {
                next = stream.next() => next,
                _ = until_stop_or_pause(progress.snapshot().map(Arc::as_ref)) => continue,
            };
            let Some(chunk) = next else {
                break;
            };
            let mut chunk = chunk.context("failed to read download stream")?;
            if let Some(left) = remaining {
                if left == 0 {
                    break;
                }
                if chunk.len() as u64 > left {
                    chunk.truncate(left as usize);
                }
                remaining = Some(left - chunk.len() as u64);
            }
            if chunk.is_empty() {
                continue;
            }
            let n = chunk.len() as u64;
            if let Some(cap) = byte_cap {
                if downloaded.load(Ordering::Relaxed) + n > cap {
                    bail!("update asset exceeded size cap ({cap} bytes)");
                }
            }
            file.write_all(&chunk).await?;
            downloaded.fetch_add(n, Ordering::Relaxed);
            progress.inc(n);
            let next = downloaded.load(Ordering::Relaxed);
            if next > progress.length().unwrap_or(0) {
                progress.set_length(next);
            }
            if remaining == Some(0) {
                break;
            }
        }
        if remaining.is_some_and(|left| left != 0) {
            bail!("A download part was incomplete");
        }
        Ok(())
    }
    .await;
    // tokio `write_all` returns before the bytes land; flush on every exit so no
    // write is still in flight when a restart truncates this file.
    let flushed = file.flush().await;
    result?;
    flushed?;
    Ok(())
}

async fn merge_parts(
    layout: &DownloadLayout,
    key: &str,
    filename: &str,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
    total: u64,
) -> Result<()> {
    let part_size = total.div_ceil(layout.part_count as u64);
    let output_path = layout.directory.join(filename);
    let tmp_path = layout.directory.join(format!(".{filename}.merge.tmp"));
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)
        .await
        .with_context(|| format!("failed to create {}", tmp_path.display()))?;

    let mut merged = 0u64;
    let mut buffer = vec![0u8; MERGE_CHUNK];
    let copied: Result<()> = async {
        for index in 0..layout.part_count {
            let Some((_, _, expected)) = part_range(index, part_size, total) else {
                continue;
            };
            let path = part_path(layout, key, index);
            let mut part = File::open(&path)
                .await
                .with_context(|| format!("failed to open part {}", path.display()))?;
            let mut remaining = expected;
            while remaining > 0 {
                honor_control(progress.snapshot().map(Arc::as_ref)).await?;
                let want = remaining.min(buffer.len() as u64) as usize;
                let bytes_read = part.read(&mut buffer[..want]).await?;
                if bytes_read == 0 {
                    bail!("A download part was truncated during merge");
                }
                output.write_all(&buffer[..bytes_read]).await?;
                remaining -= bytes_read as u64;
                merged += bytes_read as u64;
                downloaded.store(merged, Ordering::Relaxed);
                progress.set_position(merged.min(total));
            }
        }
        Ok(())
    }
    .await;
    // Same as `stream_into_file`: never return with a merge write still in flight.
    let flushed = output.flush().await;
    copied?;
    flushed?;
    drop(output);
    let _ = fs::remove_file(&output_path).await;
    fs::rename(&tmp_path, &output_path)
        .await
        .with_context(|| format!("failed to replace {}", output_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const RANGE_HEADER: &str = r"(?i)range:\s*bytes=(\d+)-(\d+)";

    #[derive(Clone)]
    struct ServerConfig {
        body: Arc<Vec<u8>>,
        honor_ranges: bool,
        send_head_length: bool,
        disposition: Option<String>,
        shorten_range_to: Option<usize>,
        mismatch_range_total: Option<u64>,
        range_starts: Arc<tokio::sync::Mutex<Vec<u64>>>,
        get_count: Arc<AtomicUsize>,
        head_count: Arc<AtomicUsize>,
    }

    struct TestServer {
        url: String,
        accept: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.accept.abort();
        }
    }

    async fn spawn_server(config: ServerConfig) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let config = config.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, config).await;
                });
            }
        });
        TestServer {
            url: format!("http://{addr}/payload.bin"),
            accept,
        }
    }

    async fn handle_connection(mut stream: TcpStream, config: ServerConfig) -> io::Result<()> {
        let Some(request) = read_http_request(&mut stream).await? else {
            return Ok(());
        };
        let is_head = request.starts_with("HEAD ");
        let is_get = request.starts_with("GET ");
        if is_head {
            config.head_count.fetch_add(1, Ordering::Relaxed);
        }
        if is_get {
            config.get_count.fetch_add(1, Ordering::Relaxed);
        }

        let range = parse_range(&request);
        if let Some((start, _)) = range {
            config.range_starts.lock().await.push(start);
        }

        if is_head {
            if !config.send_head_length {
                return write_http(
                    &mut stream,
                    "HTTP/1.1 404 Not Found",
                    vec![("Content-Length", "0".into())],
                    &[],
                )
                .await;
            }
            let mut headers = vec![
                ("Content-Length", config.body.len().to_string()),
                ("Accept-Ranges", "bytes".into()),
            ];
            if let Some(name) = &config.disposition {
                headers.push((
                    "Content-Disposition",
                    format!("attachment; filename=\"{name}\""),
                ));
            }
            return write_http(&mut stream, "HTTP/1.1 200 OK", headers, &[]).await;
        }

        if !is_get {
            return write_http(
                &mut stream,
                "HTTP/1.1 405 Method Not Allowed",
                vec![("Content-Length", "0".into())],
                &[],
            )
            .await;
        }

        if config.honor_ranges {
            if let Some((start, end)) = range {
                let end = end.min(config.body.len().saturating_sub(1) as u64);
                let start = start.min(end);
                let slice = &config.body[start as usize..=end as usize];
                let sent = match config.shorten_range_to {
                    Some(max) if slice.len() > max => &slice[..max],
                    _ => slice,
                };
                let reported_total = if start == 0 && end == 0 {
                    config.body.len() as u64
                } else {
                    config
                        .mismatch_range_total
                        .unwrap_or(config.body.len() as u64)
                };
                return write_http(
                    &mut stream,
                    "HTTP/1.1 206 Partial Content",
                    vec![
                        ("Content-Length", sent.len().to_string()),
                        (
                            "Content-Range",
                            format!("bytes {start}-{end}/{reported_total}"),
                        ),
                        ("Accept-Ranges", "bytes".into()),
                    ],
                    sent,
                )
                .await;
            }
        } else if range.is_some() {
            return write_http(
                &mut stream,
                "HTTP/1.1 200 OK",
                vec![("Content-Length", config.body.len().to_string())],
                &config.body,
            )
            .await;
        }

        let headers = if config.send_head_length {
            vec![("Content-Length", config.body.len().to_string())]
        } else {
            vec![]
        };
        write_http(&mut stream, "HTTP/1.1 200 OK", headers, &config.body).await
    }

    async fn write_http(
        stream: &mut TcpStream,
        status: &str,
        headers: Vec<(&str, String)>,
        body: &[u8],
    ) -> io::Result<()> {
        let mut out = format!("{status}\r\n");
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        stream.write_all(out.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await
    }

    async fn read_http_request(stream: &mut TcpStream) -> io::Result<Option<String>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Ok(if buf.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&buf).into_owned())
                });
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            if buf.len() > 64 * 1024 {
                return Err(io::Error::other("request too large"));
            }
        }
    }

    fn parse_range(request: &str) -> Option<(u64, u64)> {
        let pattern = regex::Regex::new(RANGE_HEADER).unwrap();
        let caps = pattern.captures(request)?;
        let start = caps.get(1)?.as_str().parse().ok()?;
        let end = caps.get(2)?.as_str().parse().ok()?;
        Some((start, end))
    }

    fn pattern_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 256) as u8).collect()
    }

    fn layout(dir: &tempfile::TempDir) -> DownloadLayout {
        DownloadLayout::in_dir(dir.path().to_path_buf())
    }

    fn config(body: Vec<u8>, honor_ranges: bool, send_head_length: bool) -> ServerConfig {
        ServerConfig {
            body: Arc::new(body),
            honor_ranges,
            send_head_length,
            disposition: None,
            shorten_range_to: None,
            mismatch_range_total: None,
            range_starts: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            get_count: Arc::new(AtomicUsize::new(0)),
            head_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn download_staging(url: &str, dest: &Path, max_bytes: u64) -> Result<()> {
        let client = range_client("torrenter-test/0").unwrap();
        download_file(
            &client,
            url,
            dest,
            max_bytes,
            true,
            &ProgressHandle::hidden(),
        )
        .await
    }

    #[test]
    fn content_disposition_and_sanitization() {
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="report.iso""#).as_deref(),
            Some("report.iso")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''big%20file.bin").as_deref(),
            Some("big%20file.bin")
        );
        assert_eq!(sanitize_filename("dir/file*.bin"), "file_.bin");
        assert_eq!(sanitize_filename(".."), "download.bin");
        assert_eq!(sanitize_filename("."), "download.bin");
        assert_eq!(sanitize_filename(""), "download.bin");
        assert_eq!(sanitize_filename("../etc/passwd"), "passwd");
        assert_eq!(
            filename_from_url("https://cdn.example/%2e%2e", None),
            "download.bin"
        );
        assert_eq!(decode_filename("big%20file.bin"), "big file.bin");
        assert_eq!(
            filename_from_url("https://cdn.example/from-url.bin", None),
            "from-url.bin"
        );
        assert_eq!(resume_key("https://example/a").len(), 16);
        assert_ne!(
            resume_key("https://example/a"),
            resume_key("https://example/b")
        );
    }

    #[test]
    fn http_presence_resume_parts_and_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path();
        let url = "https://example.com/file.bin";
        assert!(!http_resume_present(dest, url));
        assert!(!http_output_present(dest, "file.bin"));
        assert!(!http_payload_present(dest, url, "file.bin"));
        let layout = DownloadLayout::in_dir(dest.to_path_buf());
        let key = resume_key(url);
        std::fs::create_dir_all(resume_directory(&layout)).unwrap();
        let manifest = ResumeManifest {
            url: url.into(),
            size: 10,
            part_count: 4,
            filename: "file.bin".into(),
        };
        std::fs::write(
            manifest_path(&layout, &key),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(!http_resume_present(dest, url));
        std::fs::write(part_path(&layout, &key, 0), b"hello").unwrap();
        assert!(http_resume_present(dest, url));
        assert!(http_payload_present(dest, url, "other.bin"));
        std::fs::write(dest.join("file.bin"), b"done").unwrap();
        assert!(http_output_present(dest, "file.bin"));
        assert!(!http_output_present(dest, "../escape.bin"));
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let err = start_url_download("ftp://example.com/file", PathBuf::from("."))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid HTTP(S) URL"));
    }

    #[tokio::test]
    async fn multipart_range_download_merges_in_order() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let server = config(body.clone(), true, true);
        let ranges = Arc::clone(&server.range_starts);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();

        start_url_download_in(&url, layout(&dir)).await.unwrap();

        let saved = fs::read(dir.path().join("payload.bin")).await.unwrap();
        assert_eq!(saved, body);
        let starts = ranges.lock().await.clone();
        for start in [0_u64, 64, 128, 192] {
            assert!(
                starts.contains(&start),
                "missing range start {start}, got {starts:?}"
            );
        }
        assert!(resume_dir_clean(&dir, &url));
    }

    #[tokio::test]
    async fn uses_content_disposition_filename() {
        let body = pattern_body(64);
        let dir = tempfile::tempdir().unwrap();
        let mut server = config(body.clone(), true, true);
        server.disposition = Some("from-header.bin".into());
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();

        start_url_download_in(&url, layout(&dir)).await.unwrap();
        assert!(dir.path().join("from-header.bin").exists());
        assert_eq!(
            fs::read(dir.path().join("from-header.bin")).await.unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn resumes_partial_part_file() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let download_layout = layout(&dir);
        let server = config(body.clone(), true, true);
        let ranges = Arc::clone(&server.range_starts);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();
        let key = resume_key(&url);

        fs::create_dir_all(resume_directory(&download_layout))
            .await
            .unwrap();
        save_resume_manifest(
            &download_layout,
            &key,
            &ResumeManifest {
                url: url.clone(),
                size: body.len() as u64,
                part_count: PART_COUNT,
                filename: "payload.bin".into(),
            },
        )
        .await
        .unwrap();
        fs::write(part_path(&download_layout, &key, 0), &body[..40])
            .await
            .unwrap();

        start_url_download_in(&url, download_layout).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
        let starts = ranges.lock().await.clone();
        assert!(
            starts.contains(&40),
            "expected resume range start 40, got {starts:?}"
        );
        assert!(resume_dir_clean(&dir, &url));
    }

    #[tokio::test]
    async fn resumes_from_matching_manifest_without_redownloading_complete_parts() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let download_layout = layout(&dir);
        let server = config(body.clone(), true, true);
        let ranges = Arc::clone(&server.range_starts);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();
        let key = resume_key(&url);
        let part_size = 64usize;

        fs::create_dir_all(resume_directory(&download_layout))
            .await
            .unwrap();
        save_resume_manifest(
            &download_layout,
            &key,
            &ResumeManifest {
                url: url.clone(),
                size: body.len() as u64,
                part_count: PART_COUNT,
                filename: "payload.bin".into(),
            },
        )
        .await
        .unwrap();
        for index in 0..3 {
            let start = index * part_size;
            fs::write(
                part_path(&download_layout, &key, index),
                &body[start..start + part_size],
            )
            .await
            .unwrap();
        }

        start_url_download_in(&url, download_layout).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
        let starts = ranges.lock().await.clone();
        assert!(
            starts.contains(&192),
            "expected remaining part start 192, got {starts:?}"
        );
        assert!(!starts.contains(&64) && !starts.contains(&128));
        assert!(resume_dir_clean(&dir, &url));
    }

    #[tokio::test]
    async fn falls_back_when_server_ignores_ranges() {
        let body = pattern_body(200);
        let dir = tempfile::tempdir().unwrap();
        let server = config(body.clone(), false, true);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();

        start_url_download_in(&url, layout(&dir)).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
        assert!(resume_dir_clean(&dir, &url));
    }

    #[tokio::test]
    async fn streams_when_size_is_unknown() {
        let body = pattern_body(90);
        let dir = tempfile::tempdir().unwrap();
        let server = config(body.clone(), true, false);
        let ranges = Arc::clone(&server.range_starts);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();

        start_url_download_in(&url, layout(&dir)).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
        assert!(ranges.lock().await.is_empty());
    }

    #[tokio::test]
    async fn mismatched_resume_manifest_is_discarded() {
        let body = pattern_body(128);
        let dir = tempfile::tempdir().unwrap();
        let download_layout = layout(&dir);
        let server = config(body.clone(), true, true);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();
        let key = resume_key(&url);

        fs::create_dir_all(resume_directory(&download_layout))
            .await
            .unwrap();
        save_resume_manifest(
            &download_layout,
            &key,
            &ResumeManifest {
                url: url.clone(),
                size: 9999,
                part_count: PART_COUNT,
                filename: "payload.bin".into(),
            },
        )
        .await
        .unwrap();
        fs::write(part_path(&download_layout, &key, 0), b"stale")
            .await
            .unwrap();

        start_url_download_in(&url, download_layout).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn oversized_part_is_truncated_before_merge() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let download_layout = layout(&dir);
        let server = config(body.clone(), true, true);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();
        let key = resume_key(&url);

        fs::create_dir_all(resume_directory(&download_layout))
            .await
            .unwrap();
        save_resume_manifest(
            &download_layout,
            &key,
            &ResumeManifest {
                url: url.clone(),
                size: body.len() as u64,
                part_count: PART_COUNT,
                filename: "payload.bin".into(),
            },
        )
        .await
        .unwrap();
        let mut oversized = body[..64].to_vec();
        oversized.extend_from_slice(&[0xFF; 32]);
        fs::write(part_path(&download_layout, &key, 0), oversized)
            .await
            .unwrap();

        start_url_download_in(&url, download_layout).await.unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn truncated_range_body_fails_instead_of_zero_filling() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let download_layout = layout(&dir);
        let mut server = config(body, true, true);
        server.shorten_range_to = Some(8);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();

        let err = start_url_download_in(&url, download_layout.clone())
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("incomplete"),
            "expected incomplete part, got {message}"
        );

        let key = resume_key(&url);
        // Any of the 4 parts can be the first to see the short 206 and abort the rest,
        // so part-0 may not exist yet (Windows CI often loses that race).
        let mut found_short = false;
        for index in 0..PART_COUNT {
            let Ok(meta) = fs::metadata(part_path(&download_layout, &key, index)).await else {
                continue;
            };
            assert!(
                meta.len() < 64,
                "part should not be zero-padded to expected size, index={index} len={}",
                meta.len()
            );
            found_short = true;
        }
        assert!(
            found_short,
            "expected at least one short part file after the incomplete 206"
        );
    }

    #[tokio::test]
    async fn pause_waits_then_resumes_from_parts() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let server = config(body.clone(), true, true);
        let test_server = spawn_server(server).await;
        let url = test_server.url.clone();
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.toggle_pause();
        let layout = layout(&dir);
        let job = tokio::spawn({
            let url = url.clone();
            let snapshot = Arc::clone(&snapshot);
            async move { start_url_download_reported_in(&url, layout, snapshot).await }
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!job.is_finished());
        snapshot.toggle_pause();
        job.await.unwrap().unwrap();
        assert_eq!(
            tokio::fs::read(dir.path().join("payload.bin"))
                .await
                .unwrap(),
            body
        );
    }

    fn resume_dir_clean(dir: &tempfile::TempDir, url: &str) -> bool {
        let key = resume_key(url);
        let resume = dir.path().join(RESUME_DIR_NAME);
        !resume.join(format!("{key}.json")).exists()
            && (0..PART_COUNT).all(|index| !resume.join(format!("{key}.part-{index}")).exists())
    }

    #[tokio::test]
    async fn staging_range_download_merges_beside_dest() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".torrenter.update");
        let server = config(body.clone(), true, true);
        let ranges = Arc::clone(&server.range_starts);
        let test_server = spawn_server(server).await;

        download_staging(&test_server.url, &dest, 1024)
            .await
            .unwrap();

        assert_eq!(fs::read(&dest).await.unwrap(), body);
        assert!(!dir.path().join(RESUME_DIR_NAME).exists());
        for index in 0..PART_COUNT {
            assert!(!dest
                .with_file_name(format!(".torrenter.update.part-{index}"))
                .exists());
        }
        let starts = ranges.lock().await.clone();
        for start in [0_u64, 64, 128, 192] {
            assert!(
                starts.contains(&start),
                "missing range start {start}, got {starts:?}"
            );
        }
    }

    #[tokio::test]
    async fn restart_waits_for_previous_task_on_same_output() {
        let body = pattern_body(4096);
        let dir = tempfile::tempdir().unwrap();
        let server = config(body.clone(), true, true);
        let gets = Arc::clone(&server.get_count);
        let heads = Arc::clone(&server.head_count);
        let test_server = spawn_server(server).await;

        let held = output_lock(&http_lock_key(dir.path(), &test_server.url));
        let guard = held.lock().await;
        let snapshot = Arc::new(JobSnapshot::default());
        let url = test_server.url.clone();
        let task = tokio::spawn({
            let layout = layout(&dir);
            let snapshot = Arc::clone(&snapshot);
            async move { start_url_download_reported_in(&url, layout, snapshot).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            heads.load(Ordering::Relaxed),
            0,
            "must not probe while locked"
        );
        assert_eq!(gets.load(Ordering::Relaxed), 0, "must not GET while locked");
        assert!(!task.is_finished());

        drop(guard);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("download should finish once the lock is released")
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(dir.path().join("payload.bin")).await.unwrap(),
            body
        );
    }

    fn job_layout(dir: &tempfile::TempDir, identity: &str, parts: usize) -> DownloadLayout {
        DownloadLayout::for_job_file(
            dir.path().join("Album/CD1"),
            dir.path().to_path_buf(),
            identity.to_string(),
            parts,
        )
    }

    #[tokio::test]
    async fn job_file_resumes_parts_across_a_fresh_link() {
        let body = pattern_body(4096);
        let dir = tempfile::tempdir().unwrap();
        let client = range_client("torrenter/test").unwrap();
        let layout = job_layout(&dir, "torbox|42|0", 4);

        // First link delivers short ranges: parts stay partial, the call fails.
        let mut short = config(body.clone(), true, true);
        short.shorten_range_to = Some(100);
        let first = spawn_server(short).await;
        let snapshot = Arc::new(JobSnapshot::default());
        let progress = ProgressHandle::job_file(Arc::clone(&snapshot), 0, 4096, 4096);
        let link_a = format!("{}?token=secret-a", first.url);
        assert!(
            download_job_file(&client, &link_a, &layout, "01.bin", 4096, &progress)
                .await
                .is_err()
        );
        let manifest = std::fs::read_to_string(
            dir.path()
                .join(".resume")
                .join(format!("{}.json", resume_key("torbox|42|0"))),
        )
        .unwrap();
        assert!(
            !manifest.contains("secret-a"),
            "links never enter the manifest"
        );

        // A different (fresh) link continues from the saved parts.
        let fresh = config(body.clone(), true, true);
        let starts = Arc::clone(&fresh.range_starts);
        let second = spawn_server(fresh).await;
        let link_b = format!("{}?token=secret-b", second.url);
        download_job_file(&client, &link_b, &layout, "01.bin", 4096, &progress)
            .await
            .unwrap();
        let starts = starts.lock().await.clone();
        assert!(!starts.is_empty());
        // Parts the first run saved are continued, not discarded; a part the
        // failing run aborted before writing simply starts at its offset.
        assert!(
            starts.iter().all(|start| matches!(start % 1024, 0 | 100)),
            "{starts:?}"
        );
        assert!(
            starts.iter().filter(|start| *start % 1024 == 100).count() >= 2,
            "parts resumed across the new link: {starts:?}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("Album/CD1/01.bin")).unwrap(),
            body
        );
        assert!(!snapshot.is_done(), "a job file never finishes the job");
        assert!(
            !dir.path()
                .join(".resume")
                .join(format!("{}.json", resume_key("torbox|42|0")))
                .exists(),
            "parts and manifest are cleaned after the merge"
        );
    }

    async fn spawn_status_server(status: &'static str) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = read_http_request(&mut stream).await;
                    let _ = write_http(
                        &mut stream,
                        status,
                        vec![("Content-Length", "0".into())],
                        &[],
                    )
                    .await;
                });
            }
        });
        TestServer {
            url: format!("http://{addr}/gone.bin"),
            accept,
        }
    }

    #[tokio::test]
    async fn job_file_reports_a_rejected_link() {
        let dir = tempfile::tempdir().unwrap();
        let client = range_client("torrenter/test").unwrap();
        let server = spawn_status_server("HTTP/1.1 403 Forbidden").await;
        let snapshot = Arc::new(JobSnapshot::default());
        let progress = ProgressHandle::job_file(snapshot, 0, 10, 10);
        let layout = job_layout(&dir, "torbox|1|0", 4);
        let error = download_job_file(&client, &server.url, &layout, "x.bin", 10, &progress)
            .await
            .unwrap_err();
        let rejected = error.downcast_ref::<LinkRejected>().expect("LinkRejected");
        assert_eq!(rejected.0, 403);
    }

    #[tokio::test]
    async fn fewer_parts_remove_orphans_from_a_bigger_split() {
        let dir = tempfile::tempdir().unwrap();
        let big = job_layout(&dir, "id", 8);
        let key = resume_key("id");
        save_resume_manifest(
            &big,
            &key,
            &ResumeManifest {
                url: "id".into(),
                size: 100,
                part_count: 8,
                filename: "f".into(),
            },
        )
        .await
        .unwrap();
        for index in 0..8 {
            std::fs::write(part_path(&big, &key, index), b"x").unwrap();
        }
        let small = job_layout(&dir, "id", 4);
        remove_resume_files(&small, &key).await.unwrap();
        for index in 0..8 {
            assert!(
                !part_path(&small, &key, index).exists(),
                "part {index} left behind"
            );
        }
    }

    #[test]
    fn job_file_progress_adds_up_across_files() {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title("Album");
        let first = ProgressHandle::job_file(Arc::clone(&snapshot), 0, 300, 100);
        first.set_title("01.flac");
        first.set_position(100);
        first.finish();
        assert_eq!(snapshot.downloaded.load(Ordering::Relaxed), 100);
        let second = ProgressHandle::job_file(Arc::clone(&snapshot), 100, 300, 200);
        second.inc(50);
        assert_eq!(snapshot.downloaded.load(Ordering::Relaxed), 150);
        assert_eq!(snapshot.total.load(Ordering::Relaxed), 300);
        assert_eq!(snapshot.title(), "Album");
        assert!(!snapshot.is_done());
    }

    /// Range server that sends `sent` bytes of every part, then stalls forever.
    async fn spawn_stalling_server(
        total: usize,
        sent: usize,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(Some(request)) = read_http_request(&mut stream).await else {
                        return;
                    };
                    if request.starts_with("HEAD ") {
                        let headers = vec![
                            ("Content-Length", total.to_string()),
                            ("Accept-Ranges", "bytes".into()),
                        ];
                        let _ = write_http(&mut stream, "HTTP/1.1 200 OK", headers, &[]).await;
                        return;
                    }
                    let (start, end) = parse_range(&request).unwrap_or((0, total as u64 - 1));
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\n\r\n",
                        end - start + 1
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&vec![7u8; sent]).await;
                    let _ = stream.flush().await;
                    tokio::time::sleep(Duration::from_secs(600)).await;
                });
            }
        });
        (format!("http://{addr}/stall.bin"), accept)
    }

    #[tokio::test]
    async fn stop_during_stalled_parts_drains_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = spawn_stalling_server(4096, 100).await;
        let snapshot = Arc::new(JobSnapshot::default());
        let task = tokio::spawn({
            let layout = layout(&dir);
            let snapshot = Arc::clone(&snapshot);
            async move { start_url_download_reported_in(&url, layout, snapshot).await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while snapshot.downloaded.load(Ordering::Relaxed) < 400 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "parts never streamed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        snapshot.request_stop();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stalled parts must exit on stop without the read timeout")
            .unwrap()
            .unwrap();
        assert!(snapshot.is_stopped());
        let mut on_disk = 0;
        let mut parts = fs::read_dir(dir.path().join(RESUME_DIR_NAME))
            .await
            .unwrap();
        while let Some(entry) = parts.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy().contains(".part-") {
                on_disk += entry.metadata().await.unwrap().len();
            }
        }
        assert_eq!(on_disk, 400, "every streamed byte is flushed before return");
        server.abort();
    }

    #[tokio::test]
    async fn staging_falls_back_when_server_ignores_ranges() {
        let body = pattern_body(200);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".torrenter.update");
        let server = config(body.clone(), false, true);
        let test_server = spawn_server(server).await;

        download_staging(&test_server.url, &dest, 1024)
            .await
            .unwrap();
        assert_eq!(fs::read(&dest).await.unwrap(), body);
        assert!(!dir.path().join(RESUME_DIR_NAME).exists());
    }

    #[tokio::test]
    async fn staging_truncated_range_body_fails() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".torrenter.update");
        let mut server = config(body, true, true);
        server.shorten_range_to = Some(8);
        let test_server = spawn_server(server).await;

        let err = download_staging(&test_server.url, &dest, 1024)
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("incomplete"),
            "expected incomplete part, got {message}"
        );
        let mut found_short = false;
        for index in 0..PART_COUNT {
            let path = dest.with_file_name(format!(".torrenter.update.part-{index}"));
            let Ok(meta) = fs::metadata(&path).await else {
                continue;
            };
            assert!(
                meta.len() < 64,
                "part should not be zero-padded, index={index} len={}",
                meta.len()
            );
            found_short = true;
        }
        assert!(found_short, "expected at least one short staging part");
    }

    #[tokio::test]
    async fn staging_content_range_mismatch_falls_back_to_get() {
        let body = pattern_body(256);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".torrenter.update");
        let mut server = config(body.clone(), true, true);
        server.mismatch_range_total = Some(9999);
        let test_server = spawn_server(server).await;

        download_staging(&test_server.url, &dest, 10 * 1024)
            .await
            .unwrap();
        assert_eq!(fs::read(&dest).await.unwrap(), body);
        for index in 0..PART_COUNT {
            assert!(!dest
                .with_file_name(format!(".torrenter.update.part-{index}"))
                .exists());
        }
    }

    #[tokio::test]
    async fn staging_rejects_oversize_content_length() {
        let body = pattern_body(200);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".torrenter.update");
        let server = config(body, false, true);
        let test_server = spawn_server(server).await;

        let err = download_staging(&test_server.url, &dest, 50)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("too large"), "{err:#}");
        assert!(!dest.exists());
    }
}

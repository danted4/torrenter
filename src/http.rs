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

use crate::progress::{honor_control, is_stopped, watch_cli_controls, JobSnapshot, ProgressHandle};
use crate::validate::is_valid_download_url;

pub const DOWNLOAD_DIRECTORY: &str = "./downloads";
pub const PART_COUNT: usize = 4;
const RESUME_DIR_NAME: &str = ".resume";
const MERGE_CHUNK: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DownloadLayout {
    pub directory: PathBuf,
    pub part_count: usize,
}

impl Default for DownloadLayout {
    fn default() -> Self {
        Self {
            directory: PathBuf::from(DOWNLOAD_DIRECTORY),
            part_count: PART_COUNT,
        }
    }
}

#[derive(Debug)]
struct RangesNotHonored;

impl std::fmt::Display for RangesNotHonored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The server did not honor ranged requests")
    }
}

impl std::error::Error for RangesNotHonored {}

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

pub async fn start_url_download(download_url: &str) -> Result<()> {
    start_url_download_in(download_url, DownloadLayout::default()).await
}

pub async fn start_url_download_reported(
    download_url: &str,
    snapshot: Arc<JobSnapshot>,
) -> Result<()> {
    run_url_download(download_url, DownloadLayout::default(), Some(snapshot)).await
}

pub async fn start_url_download_in(download_url: &str, layout: DownloadLayout) -> Result<()> {
    run_url_download(download_url, layout, None).await
}

async fn run_url_download(
    download_url: &str,
    layout: DownloadLayout,
    snapshot: Option<Arc<JobSnapshot>>,
) -> Result<()> {
    if !is_valid_download_url(download_url) {
        bail!("Invalid HTTP(S) URL!");
    }

    let client = Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .gzip(false)
        .brotli(false)
        .build()
        .context("failed to build HTTP client")?;

    let metadata = get_remote_metadata(&client, download_url).await;
    let filename = metadata
        .as_ref()
        .map(|meta| meta.filename.clone())
        .unwrap_or_else(|| filename_from_url(download_url, None));
    let total = metadata.as_ref().and_then(|meta| meta.size).unwrap_or(0);

    fs::create_dir_all(&layout.directory)
        .await
        .context("failed to create downloads directory")?;

    let output_path = layout.directory.join(&filename);
    let progress = match snapshot {
        Some(snapshot) => ProgressHandle::gui(total.max(1), snapshot),
        None => ProgressHandle::cli(total.max(1)),
    };
    progress.set_title(&filename);
    if progress.announce() {
        println!("\nPreparing direct download...\n");
        println!("{filename}\n");
        println!("[1] pause/resume   [2] stop\n");
    }
    progress.set_message("0");
    let downloaded = Arc::new(AtomicU64::new(0));

    let download = async {
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
                    stream_single(&client, download_url, &output_path, &progress, &downloaded).await
                }
                Err(error) => Err(error),
            }
        } else {
            stream_single(&client, download_url, &output_path, &progress, &downloaded).await
        }
    };

    let result = if progress.announce() {
        let snapshot = progress.snapshot().cloned();
        tokio::select! {
            result = download => result,
            _ = async {
                if let Some(snapshot) = snapshot {
                    watch_cli_controls(snapshot).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                if let Some(snapshot) = progress.snapshot() {
                    snapshot.request_stop();
                }
                Err(crate::progress::DownloadStopped.into())
            }
        }
    } else {
        download.await
    };

    match result {
        Ok(()) => {
            let final_total = if total > 0 {
                total
            } else {
                downloaded.load(Ordering::Relaxed).max(1)
            };
            progress.set_length(final_total);
            progress.set_position(if total > 0 {
                total
            } else {
                downloaded.load(Ordering::Relaxed)
            });
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

fn resume_key(download_url: &str) -> String {
    let digest = Sha256::digest(download_url.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn resume_directory(layout: &DownloadLayout) -> PathBuf {
    layout.directory.join(RESUME_DIR_NAME)
}

fn manifest_path(layout: &DownloadLayout, key: &str) -> PathBuf {
    resume_directory(layout).join(format!("{key}.json"))
}

fn part_path(layout: &DownloadLayout, key: &str, index: usize) -> PathBuf {
    resume_directory(layout).join(format!("{key}.part-{index}"))
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
    let _ = fs::remove_file(manifest_path(layout, key)).await;
    for index in 0..layout.part_count {
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
    let key = resume_key(download_url);
    let part_size = total.div_ceil(layout.part_count as u64);
    let manifest = get_resume_manifest(layout, &key).await;
    let can_resume = manifest.as_ref().is_some_and(|manifest| {
        manifest.url == download_url
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
            url: download_url.to_string(),
            size: total,
            part_count: layout.part_count,
            filename: filename.to_string(),
        },
    )
    .await?;

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
                    Ok(Err(error)) if is_stopped(&error) => {
                        tasks.abort_all();
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
                    tasks.abort_all();
                    anyhow::bail!(crate::progress::DownloadStopped);
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

    let range_start = start + existing_size;
    let response = client
        .get(download_url)
        .header(RANGE, format!("bytes={range_start}-{end}"))
        .send()
        .await
        .context("ranged request failed")?;
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
) -> Result<()> {
    let response = client
        .get(download_url)
        .send()
        .await
        .context("download request failed")?;
    if !response.status().is_success() {
        bail!("Download request failed ({})", response.status());
    }
    if let Some(len) = response.content_length().filter(|len| *len > 0) {
        progress.set_length(len);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_path)
        .await
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    stream_into_file(response, &mut file, progress, downloaded, None).await?;
    file.flush().await?;
    Ok(())
}

async fn stream_into_file(
    response: reqwest::Response,
    file: &mut File,
    progress: &ProgressHandle,
    downloaded: &Arc<AtomicU64>,
    max_bytes: Option<u64>,
) -> Result<()> {
    let mut remaining = max_bytes;
    let mut stream = response.bytes_stream();
    loop {
        honor_control(progress.snapshot().map(Arc::as_ref)).await?;
        let Some(chunk) = stream.next().await else {
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
        file.write_all(&chunk).await?;
        let n = chunk.len() as u64;
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
    output.flush().await?;
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
                return write_http(
                    &mut stream,
                    "HTTP/1.1 206 Partial Content",
                    vec![
                        ("Content-Length", sent.len().to_string()),
                        (
                            "Content-Range",
                            format!("bytes {start}-{end}/{}", config.body.len()),
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
        DownloadLayout {
            directory: dir.path().to_path_buf(),
            part_count: PART_COUNT,
        }
    }

    fn config(body: Vec<u8>, honor_ranges: bool, send_head_length: bool) -> ServerConfig {
        ServerConfig {
            body: Arc::new(body),
            honor_ranges,
            send_head_length,
            disposition: None,
            shorten_range_to: None,
            range_starts: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            get_count: Arc::new(AtomicUsize::new(0)),
            head_count: Arc::new(AtomicUsize::new(0)),
        }
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

    #[tokio::test]
    async fn rejects_invalid_url() {
        let err = start_url_download("ftp://example.com/file")
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
        let part0 = fs::metadata(part_path(&download_layout, &key, 0))
            .await
            .unwrap()
            .len();
        assert!(
            part0 < 64,
            "part should not be zero-padded to expected size, len={part0}"
        );
    }

    fn resume_dir_clean(dir: &tempfile::TempDir, url: &str) -> bool {
        let key = resume_key(url);
        let resume = dir.path().join(RESUME_DIR_NAME);
        !resume.join(format!("{key}.json")).exists()
            && (0..PART_COUNT).all(|index| !resume.join(format!("{key}.part-{index}")).exists())
    }
}

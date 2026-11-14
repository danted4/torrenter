//! Premiumize (www.premiumize.me/api). Research: .cursor/plans/debrid-research/premiumize.md
//!
//! Two routes, told apart by the opaque `remote_id`:
//!
//! - `direct:<info hash hex>`: `cache/check` said the hash is cached and
//!   `transfer/directdl` listed its files. directdl stores nothing on the
//!   account, so `added_by_us` is false and `delete` has nothing to remove.
//!   `link()` re-asks directdl (one answer serves every file for a few minutes;
//!   a second request for the same file means the last link failed, so it asks
//!   again).
//! - `cloud:<transfer id>:<folder id>`: not cached. We `folder/create` a folder
//!   for this job only, `transfer/create` into it, poll `transfer/list` (there
//!   is no per-id lookup), list *our* folder recursively once the transfer is
//!   `finished`/`seeding`, and resolve each file with `item/details`. `delete`
//!   removes that transfer and that folder, never anything else.
//!
//! Quirks encoded here: business errors (a bad key included) arrive as HTTP 200
//! `{"status":"error","message","code"?}`; `code` is new in the 2026 docs and
//! missing on older paths, so the message decides too. `cache/check` is a GET
//! with `items[]` and reports `filesize` as a string on hits. A single-file
//! transfer reports its *parent* as `folder_id`, which may be the user's root,
//! so only the folder we created is ever listed or deleted. The key goes only in
//! `Authorization: Bearer`, never `?apikey=`. Deprecated duplicates
//! (`location`, `stream_link`, `directlink`) are ignored.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::RequestBuilder;
use serde_json::Value;
use url::Url;

use super::api::{number, permille, text, Api, Reply};
use super::{Account, Added, DebridError, Remote, RemoteFile, RemoteState, Secret};

const BASE: &str = "https://www.premiumize.me/api/";
const DIRECT: &str = "direct:";
const CLOUD: &str = "cloud:";
/// How long one directdl answer serves `link()` before it is asked again.
const DIRECT_TTL: Duration = Duration::from_secs(10 * 60);
/// Finished polls with nothing in our folder before giving up (listing lag).
const EMPTY_POLLS_MAX: u32 = 3;
/// Bounds for walking our folder (cycle and size guard).
const FOLDER_DEPTH_MAX: usize = 32;
const FOLDERS_MAX: usize = 2_000;

pub(crate) struct Premiumize {
    api: Api,
    key: Secret,
    direct: Mutex<Option<DirectCache>>,
    empty_polls: AtomicU32,
}

#[derive(Clone)]
struct DirectFile {
    path: String,
    size: u64,
    link: String,
}

/// One directdl answer. Links are secrets of a sort: kept in memory only.
struct DirectCache {
    hash: String,
    fetched: Instant,
    files: Vec<DirectFile>,
    /// Paths whose link `link()` already returned from this answer.
    handed_out: HashSet<String>,
}

enum Job {
    Direct { hash: String },
    Cloud { transfer: String, folder: String },
}

fn parse_job(remote_id: &str) -> Option<Job> {
    if let Some(hash) = remote_id.strip_prefix(DIRECT) {
        return is_hex_hash(hash).then(|| Job::Direct {
            hash: hash.to_ascii_lowercase(),
        });
    }
    let (transfer, folder) = remote_id.strip_prefix(CLOUD)?.split_once(':')?;
    (opaque_id(transfer) && opaque_id(folder)).then(|| Job::Cloud {
        transfer: transfer.to_string(),
        folder: folder.to_string(),
    })
}

fn is_hex_hash(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Provider ids we can embed in `remote_id` (`:` is our separator).
fn opaque_id(value: &str) -> bool {
    !value.is_empty() && !value.contains(':') && !value.chars().any(char::is_whitespace)
}

fn is_http(link: &str) -> bool {
    link.starts_with("https://") || link.starts_with("http://")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Premiumize {
    pub fn new(key: Secret, base: Option<Url>) -> Result<Premiumize, DebridError> {
        let base = base.unwrap_or_else(|| Url::parse(BASE).expect("valid Premiumize base"));
        Ok(Premiumize {
            api: Api::new(base)?,
            key,
            direct: Mutex::new(None),
            empty_polls: AtomicU32::new(0),
        })
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.key.expose())
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.api
            .client()
            .get(self.api.url(path))
            .header("Authorization", self.bearer())
    }

    fn post(&self, path: &str, form: &[(&str, &str)]) -> RequestBuilder {
        self.api
            .client()
            .post(self.api.url(path))
            .header("Authorization", self.bearer())
            .form(form)
    }

    async fn call(&self, request: RequestBuilder) -> Result<Value, DebridError> {
        check(self.api.send(request).await?).map_err(Refusal::into_error)
    }

    pub async fn account(&self) -> Result<Account, DebridError> {
        let data = match self.call(self.get("account/info")).await {
            Ok(data) => data,
            // Some free keys are refused outright instead of `premium_until: null`.
            Err(DebridError::NotPremium(_)) => {
                return Ok(Account {
                    premium: false,
                    expires: None,
                })
            }
            Err(error) => return Err(error),
        };
        let until = data
            .get("premium_until")
            .and_then(number)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs_f64())
            .unwrap_or(0.0);
        Ok(Account {
            premium: until.is_some_and(|seconds| seconds > now),
            expires: until
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds as i64, 0))
                .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        })
    }

    /// Cached: answer straight from directdl. Otherwise fetch into a folder of
    /// our own so cleanup can never touch anything else.
    pub async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        let hash = hash_hex.to_ascii_lowercase();
        if !is_hex_hash(&hash) {
            return Err(DebridError::Service(
                "Premiumize needs a hex info hash".into(),
            ));
        }
        if self.cached(&hash).await? {
            // "A hit is not guaranteed": a refused directdl falls through.
            if let Some(files) = self.directdl(magnet).await? {
                self.remember(&hash, files, None);
                return Ok(Added {
                    remote_id: format!("{DIRECT}{hash}"),
                    added_by_us: false,
                });
            }
        }
        let folder = self.create_folder(&hash).await?;
        let transfer = match self.create_transfer(magnet, &folder).await {
            Ok(transfer) => transfer,
            Err(error) => {
                let _ = self.delete_folder(&folder).await;
                return Err(error);
            }
        };
        Ok(Added {
            remote_id: format!("{CLOUD}{transfer}:{folder}"),
            added_by_us: true,
        })
    }

    async fn cached(&self, hash: &str) -> Result<bool, DebridError> {
        let request = self.get("cache/check").query(&[("items[]", hash)]);
        let data = self.call(request).await?;
        Ok(data
            .get("response")
            .and_then(Value::as_array)
            .and_then(|hits| hits.first())
            .and_then(Value::as_bool)
            == Some(true))
    }

    /// `None` when Premiumize cannot serve the source directly (not cached).
    async fn directdl(&self, magnet: &str) -> Result<Option<Vec<DirectFile>>, DebridError> {
        let reply = self
            .api
            .send(self.post("transfer/directdl", &[("src", magnet)]))
            .await?;
        let data = match check(reply) {
            Ok(data) => data,
            Err(refusal) if refusal.not_cached() => return Ok(None),
            Err(refusal) => return Err(refusal.into_error()),
        };
        let mut seen = HashSet::new();
        let files: Vec<DirectFile> = data
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let path = item.get("path").and_then(text).filter(|p| !p.is_empty())?;
                        let link = item
                            .get("link")
                            .and_then(Value::as_str)
                            .filter(|link| is_http(link))?;
                        Some(DirectFile {
                            path,
                            size: item.get("size").and_then(number).unwrap_or(0.0) as u64,
                            link: link.to_string(),
                        })
                    })
                    // A duplicate path would make `link()` ambiguous.
                    .filter(|file| seen.insert(file.path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ok((!files.is_empty()).then_some(files))
    }

    fn remember(&self, hash: &str, files: Vec<DirectFile>, handed_out: Option<&str>) {
        *lock(&self.direct) = Some(DirectCache {
            hash: hash.to_string(),
            fetched: Instant::now(),
            files,
            handed_out: handed_out.map(str::to_string).into_iter().collect(),
        });
    }

    /// directdl for a direct job, rebuilt from the hash (the cache is keyed by
    /// it). `Gone` once Premiumize stops serving it, so the runner re-adds.
    async fn direct_refresh(&self, hash: &str) -> Result<Vec<DirectFile>, DebridError> {
        self.directdl(&format!("magnet:?xt=urn:btih:{hash}"))
            .await?
            .ok_or(DebridError::Gone)
    }

    async fn create_folder(&self, hash: &str) -> Result<String, DebridError> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let name = format!("torrenter-{}-{stamp}", &hash[..8]);
        let data = self
            .call(self.post("folder/create", &[("name", name.as_str())]))
            .await?;
        data.get("id")
            .and_then(text)
            .filter(|id| opaque_id(id))
            .ok_or_else(|| DebridError::Service("Premiumize returned no folder id".into()))
    }

    async fn create_transfer(&self, magnet: &str, folder: &str) -> Result<String, DebridError> {
        let data = self
            .call(self.post("transfer/create", &[("src", magnet), ("folder_id", folder)]))
            .await?;
        data.get("id")
            .and_then(text)
            .filter(|id| opaque_id(id))
            .ok_or_else(|| DebridError::Service("Premiumize returned no transfer id".into()))
    }

    pub async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        match parse_job(remote_id).ok_or(DebridError::Gone)? {
            Job::Direct { hash } => {
                let cached = lock(&self.direct)
                    .as_ref()
                    .filter(|cache| cache.hash == hash && cache.fetched.elapsed() < DIRECT_TTL)
                    .map(|cache| cache.files.clone());
                let files = match cached {
                    Some(files) => files,
                    None => {
                        let files = self.direct_refresh(&hash).await?;
                        self.remember(&hash, files.clone(), None);
                        files
                    }
                };
                Ok(Remote {
                    state: RemoteState::Ready,
                    files: files
                        .into_iter()
                        .map(|file| RemoteFile {
                            id: file.path.clone(),
                            path: file.path,
                            size: file.size,
                        })
                        .collect(),
                })
            }
            Job::Cloud { transfer, folder } => self.cloud_status(&transfer, &folder).await,
        }
    }

    async fn cloud_status(&self, transfer_id: &str, folder: &str) -> Result<Remote, DebridError> {
        let list = self.call(self.get("transfer/list")).await?;
        let transfer = list
            .get("transfers")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("id").and_then(text).as_deref() == Some(transfer_id))
            })
            .cloned();
        let Some(transfer) = transfer else {
            // Cleared from the list (website, or after finishing): what is in
            // our folder still counts. No folder, or nothing in it: gone.
            let files = self.folder_files(folder).await?;
            return if files.is_empty() {
                Err(DebridError::Gone)
            } else {
                Ok(Remote {
                    state: RemoteState::Ready,
                    files,
                })
            };
        };
        let status = transfer
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let progress = transfer
            .get("progress")
            .and_then(number)
            .map(permille)
            .unwrap_or(0);
        let fetching = |permille| Remote {
            state: RemoteState::Fetching { permille },
            files: Vec::new(),
        };
        match status.as_str() {
            "queued" | "waiting" => Ok(fetching(0)),
            "running" => Ok(fetching(progress.min(999))),
            "finished" | "seeding" => {
                let mut files = self.folder_files(folder).await?;
                if files.is_empty() {
                    // Single file placed outside our folder: read that one
                    // file by id, never its (possibly root) parent folder.
                    if let Some(file_id) = transfer.get("file_id").and_then(text) {
                        files.push(self.item_file(&file_id).await?);
                    }
                }
                if !files.is_empty() {
                    self.empty_polls.store(0, Ordering::Relaxed);
                    return Ok(Remote {
                        state: RemoteState::Ready,
                        files,
                    });
                }
                if self.empty_polls.fetch_add(1, Ordering::Relaxed) + 1 >= EMPTY_POLLS_MAX {
                    Ok(Remote {
                        state: RemoteState::Failed(
                            "Premiumize finished the transfer but its files are not in the torrenter folder".into(),
                        ),
                        files: Vec::new(),
                    })
                } else {
                    Ok(fetching(1000))
                }
            }
            "error" | "timeout" | "deleted" | "banned" => {
                let message = transfer
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|message| !message.is_empty());
                Ok(Remote {
                    state: RemoteState::Failed(match message {
                        Some(message) => {
                            format!("Premiumize reports the transfer as {status}: {message}")
                        }
                        None => format!("Premiumize reports the transfer as {status}"),
                    }),
                    files: Vec::new(),
                })
            }
            other => Ok(Remote {
                state: RemoteState::Failed(format!(
                    "Premiumize reports an unknown transfer state {other:?}"
                )),
                files: Vec::new(),
            }),
        }
    }

    /// Every file under our folder, paths relative to it.
    async fn folder_files(&self, folder: &str) -> Result<Vec<RemoteFile>, DebridError> {
        let mut files = Vec::new();
        let mut visited = HashSet::new();
        let mut pending = vec![(folder.to_string(), String::new(), 0usize)];
        while let Some((id, prefix, depth)) = pending.pop() {
            if !visited.insert(id.clone()) || visited.len() > FOLDERS_MAX {
                continue;
            }
            let data = self
                .call(self.get("folder/list").query(&[("id", id.as_str())]))
                .await?;
            let Some(items) = data.get("content").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let (Some(item_id), Some(name)) = (
                    item.get("id").and_then(text),
                    item.get("name")
                        .and_then(text)
                        .filter(|name| !name.is_empty()),
                ) else {
                    continue;
                };
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("folder") if depth < FOLDER_DEPTH_MAX => {
                        pending.push((item_id, path, depth + 1));
                    }
                    Some("file") => files.push(RemoteFile {
                        id: item_id,
                        path,
                        size: item.get("size").and_then(number).unwrap_or(0.0) as u64,
                    }),
                    _ => {}
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    async fn item_details(&self, file_id: &str) -> Result<Value, DebridError> {
        self.call(self.get("item/details").query(&[("id", file_id)]))
            .await
    }

    async fn item_file(&self, file_id: &str) -> Result<RemoteFile, DebridError> {
        let data = self.item_details(file_id).await?;
        let name = data
            .get("name")
            .and_then(text)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                DebridError::Service("Premiumize returned a file with no name".into())
            })?;
        Ok(RemoteFile {
            id: data
                .get("id")
                .and_then(text)
                .unwrap_or_else(|| file_id.to_string()),
            path: name,
            size: data.get("size").and_then(number).unwrap_or(0.0) as u64,
        })
    }

    pub async fn link(&self, remote_id: &str, file: &RemoteFile) -> Result<String, DebridError> {
        match parse_job(remote_id).ok_or(DebridError::Gone)? {
            Job::Direct { hash } => {
                let reuse = {
                    let mut cache = lock(&self.direct);
                    match cache.as_mut() {
                        Some(cache)
                            if cache.hash == hash
                                && cache.fetched.elapsed() < DIRECT_TTL
                                && !cache.handed_out.contains(&file.id) =>
                        {
                            cache.handed_out.insert(file.id.clone());
                            cache
                                .files
                                .iter()
                                .find(|entry| entry.path == file.id)
                                .map(|entry| entry.link.clone())
                        }
                        _ => None,
                    }
                };
                if let Some(link) = reuse {
                    return Ok(link);
                }
                let files = self.direct_refresh(&hash).await?;
                let link = files
                    .iter()
                    .find(|entry| entry.path == file.id)
                    .map(|entry| entry.link.clone());
                self.remember(&hash, files, Some(&file.id));
                link.ok_or_else(|| {
                    DebridError::Service("Premiumize no longer lists this file".into())
                })
            }
            Job::Cloud { .. } => {
                let data = self.item_details(&file.id).await?;
                data.get("link")
                    .and_then(Value::as_str)
                    .filter(|link| is_http(link))
                    .map(str::to_string)
                    .ok_or_else(|| {
                        DebridError::Service("Premiumize returned no download link".into())
                    })
            }
        }
    }

    /// Direct jobs stored nothing. Cloud jobs: the transfer, then the folder
    /// we created for it (never root: its id only ever comes from `add`).
    pub async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        let Some(Job::Cloud { transfer, folder }) = parse_job(remote_id) else {
            return Ok(());
        };
        let transfer = self
            .call(self.post("transfer/delete", &[("id", transfer.as_str())]))
            .await;
        let folder = self.delete_folder(&folder).await;
        for result in [transfer.map(drop), folder] {
            match result {
                Ok(()) | Err(DebridError::Gone) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn delete_folder(&self, folder: &str) -> Result<(), DebridError> {
        self.call(self.post("folder/delete", &[("id", folder)]))
            .await
            .map(drop)
    }
}

/// An `{"status":"error"}` answer (or a non-JSON failure).
struct Refusal {
    http: u16,
    code: Option<String>,
    message: String,
}

/// Premiumize puts results at the top level next to `status: "success"`.
fn check(reply: Reply) -> Result<Value, Refusal> {
    let status = reply.json.get("status").and_then(Value::as_str);
    if reply.status.is_success() && status == Some("success") {
        return Ok(reply.json);
    }
    let http = reply.status.as_u16();
    Err(Refusal {
        http,
        code: reply
            .json
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_ascii_lowercase),
        message: reply
            .json
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Premiumize answered HTTP {http}")),
    })
}

impl Refusal {
    /// directdl's "this source is not cached" family.
    fn not_cached(&self) -> bool {
        let message = self.message.to_ascii_lowercase();
        matches!(
            self.code.as_deref(),
            Some("not_found" | "service_unsupported" | "link_generation_failed")
        ) || message.contains("error downloading")
            || message.contains("unsupported link")
    }

    /// `code` first, then the message strings community clients match on.
    fn into_error(self) -> DebridError {
        let message = self.message.to_ascii_lowercase();
        let detail = format!("Premiumize: {}", self.message);
        match self.code.as_deref() {
            Some("authentication_failed") => return DebridError::BadKey(self.message),
            Some("rate_limit_reached") => return DebridError::RateLimited(None),
            Some("not_found") => return DebridError::Gone,
            // The runner retries network errors a few times with backoff.
            Some("transient_error" | "link_generation_failed") => {
                return DebridError::Network(detail)
            }
            Some("account_limit_reached" | "service_limit_reached") => {
                return DebridError::Service(detail)
            }
            _ => {}
        }
        if message.contains("not premium") || message.contains("payment issue") {
            DebridError::NotPremium(self.message)
        } else if self.code.is_some() {
            DebridError::Service(detail)
        } else if message.contains("not logged in")
            || message.contains("invalid api key")
            || message.contains("apikey")
        {
            DebridError::BadKey(self.message)
        } else if message.contains("rate limit") {
            DebridError::RateLimited(None)
        } else if message.contains("not found") {
            DebridError::Gone
        } else if matches!(self.http, 401 | 403) {
            DebridError::BadKey(self.message)
        } else {
            DebridError::Service(detail)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::debrid::fake::{serve, FakeRequest, FakeResponse, FakeServer};

    pub const KEY: &str = "pm-test-key";
    pub const HASH: &str = "8f14e45fceea167a5a36dedd4bea2543a1b2c3d4";
    pub const ROOT: &str = "root";
    const USER_FILE: &str = "user-file";

    pub fn magnet() -> String {
        format!("magnet:?xt=urn:btih:{HASH}&dn=Album")
    }

    pub fn torrent_files() -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "Album/CD1/01.bin".to_string(),
                (0..3000u32).map(|i| (i % 251) as u8).collect(),
            ),
            (
                "Album/02.bin".to_string(),
                (0..1500u32).map(|i| (i % 13) as u8).collect(),
            ),
        ]
    }

    pub struct Folder {
        pub name: String,
        pub parent: Option<String>,
    }

    pub struct Item {
        pub name: String,
        pub folder: String,
        pub body: Vec<u8>,
    }

    pub struct Transfer {
        pub id: String,
        pub target: String,
        pub hash: String,
        pub polls: u32,
        pub materialized: bool,
        pub file_id: Option<String>,
    }

    pub struct State {
        pub premium_until: Option<i64>,
        /// Content calls answer "Account not premium.".
        pub free: bool,
        /// `account/info` itself answers "Account not premium.".
        pub info_not_premium: bool,
        pub cached: bool,
        /// cache/check hits but directdl refuses ("A hit is not guaranteed").
        pub directdl_refuses: bool,
        /// Polls before a transfer finishes (first is `queued`, then `running`).
        pub fetch_polls: u32,
        pub fail_transfer: bool,
        /// Premiumize ignores `folder_id` and drops a single file into root.
        pub single_into_root: bool,
        /// Answer the next N API calls with HTTP 429.
        pub throttle: u32,
        /// Answer the next N API calls with a `rate_limit_reached` envelope.
        pub rate_envelope: u32,
        /// Answer the first N file downloads with 403 (expired link).
        pub expire_links: u32,
        pub folders: BTreeMap<String, Folder>,
        pub items: BTreeMap<String, Item>,
        pub transfers: Vec<Transfer>,
        pub next_id: u64,
        pub created_folders: Vec<String>,
        pub deleted_folders: Vec<String>,
        pub deleted_transfers: Vec<String>,
        pub root_listed: bool,
        pub root_delete_attempted: bool,
    }

    impl Default for State {
        fn default() -> Self {
            let mut folders = BTreeMap::new();
            folders.insert(
                ROOT.to_string(),
                Folder {
                    name: "My Files".into(),
                    parent: None,
                },
            );
            let mut items = BTreeMap::new();
            items.insert(
                USER_FILE.to_string(),
                Item {
                    name: "taxes.pdf".into(),
                    folder: ROOT.into(),
                    body: b"keep me".to_vec(),
                },
            );
            State {
                premium_until: Some(4_102_444_800), // 2100-01-01
                free: false,
                info_not_premium: false,
                cached: false,
                directdl_refuses: false,
                fetch_polls: 0,
                fail_transfer: false,
                single_into_root: false,
                throttle: 0,
                rate_envelope: 0,
                expire_links: 0,
                folders,
                items,
                transfers: Vec::new(),
                next_id: 0,
                created_folders: Vec::new(),
                deleted_folders: Vec::new(),
                deleted_transfers: Vec::new(),
                root_listed: false,
                root_delete_attempted: false,
            }
        }
    }

    impl State {
        fn id(&mut self, prefix: &str) -> String {
            self.next_id += 1;
            format!("{prefix}{}", self.next_id)
        }

        /// Create `a/b/` under `parent` and return the innermost folder id.
        fn ensure_path(&mut self, parent: &str, dirs: &[&str]) -> String {
            let mut current = parent.to_string();
            for dir in dirs {
                let existing = self
                    .folders
                    .iter()
                    .find(|(_, f)| f.name == *dir && f.parent.as_deref() == Some(&current))
                    .map(|(id, _)| id.clone());
                current = match existing {
                    Some(id) => id,
                    None => {
                        let id = self.id("fo");
                        self.folders.insert(
                            id.clone(),
                            Folder {
                                name: dir.to_string(),
                                parent: Some(current.clone()),
                            },
                        );
                        id
                    }
                };
            }
            current
        }

        fn materialize(&mut self, index: usize) {
            if self.transfers[index].materialized {
                return;
            }
            self.transfers[index].materialized = true;
            if self.single_into_root {
                let id = self.id("it");
                self.items.insert(
                    id.clone(),
                    Item {
                        name: "movie.bin".into(),
                        folder: ROOT.into(),
                        body: (0..2048u32).map(|i| (i % 7) as u8).collect(),
                    },
                );
                self.transfers[index].file_id = Some(id);
                return;
            }
            let target = self.transfers[index].target.clone();
            for (path, body) in torrent_files() {
                let mut parts: Vec<&str> = path.split('/').collect();
                let name = parts.pop().unwrap().to_string();
                let folder = self.ensure_path(&target, &parts);
                let id = self.id("it");
                self.items.insert(id, Item { name, folder, body });
            }
        }

        fn remove_folder(&mut self, id: &str) {
            let children: Vec<String> = self
                .folders
                .iter()
                .filter(|(_, f)| f.parent.as_deref() == Some(id))
                .map(|(child, _)| child.clone())
                .collect();
            for child in children {
                self.remove_folder(&child);
            }
            self.items.retain(|_, item| item.folder != id);
            self.folders.remove(id);
        }
    }

    fn ok(mut value: Value) -> FakeResponse {
        value["status"] = json!("success");
        FakeResponse::json(200, value)
    }

    /// Business errors are HTTP 200; `code` is optional.
    fn err(code: Option<&str>, message: &str) -> FakeResponse {
        let mut value = json!({"status": "error", "message": message});
        if let Some(code) = code {
            value["code"] = json!(code);
        }
        FakeResponse::json(200, value)
    }

    /// A stricter-than-real request check: our client must use one shape.
    fn reject(message: &str) -> FakeResponse {
        FakeResponse::json(
            400,
            json!({"status": "error", "code": "invalid_request", "message": message}),
        )
    }

    fn hash_of(magnet: &str) -> String {
        magnet
            .split("btih:")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// A Premiumize fake that enforces Bearer-only auth, GET/POST-form shapes,
    /// and never lets a client list or delete the user's root.
    pub async fn premiumize_fake(state: Arc<Mutex<State>>) -> (FakeServer, Url) {
        let origin_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let origin_for_handler = Arc::clone(&origin_slot);
        let server = serve(move |request: &FakeRequest| {
            let mut state = state.lock().unwrap();
            let origin = origin_for_handler.lock().unwrap().clone();
            let path = request.path.as_str();
            if let Some(rest) = path.strip_prefix("/dl/") {
                if state.expire_links > 0 {
                    state.expire_links -= 1;
                    return FakeResponse::empty(403);
                }
                let body = if let Some(index) = rest.strip_prefix("direct/") {
                    index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| torrent_files().get(index).map(|(_, b)| b.clone()))
                } else if let Some(id) = rest.strip_prefix("item/") {
                    state.items.get(id).map(|item| item.body.clone())
                } else {
                    None
                };
                return match body {
                    Some(body) => FakeResponse::file(request, &body, false),
                    None => FakeResponse::empty(404),
                };
            }
            let Some(route) = path.strip_prefix("/api/") else {
                return FakeResponse::empty(404);
            };
            // The key belongs in the header only.
            let leaked = request
                .query
                .iter()
                .chain(request.form().iter())
                .any(|(name, value)| {
                    matches!(name.as_str(), "apikey" | "pin" | "access_token")
                        || value.contains(KEY)
                });
            if leaked {
                return reject("credentials must not be in the query or body");
            }
            if state.throttle > 0 {
                state.throttle -= 1;
                return FakeResponse::json(429, json!({"status": "error"}))
                    .with_header("Retry-After", "2");
            }
            if state.rate_envelope > 0 {
                state.rate_envelope -= 1;
                return err(Some("rate_limit_reached"), "Rate limit exceeded");
            }
            if request.bearer() != Some(KEY) {
                return err(Some("authentication_failed"), "Not logged in.");
            }
            let (method, form_post) = match route {
                "account/info" | "cache/check" | "transfer/list" | "folder/list"
                | "item/details" => ("GET", false),
                _ => ("POST", true),
            };
            if request.method != method || (form_post && !request.is_form()) {
                return reject("wrong method or body");
            }
            if route == "account/info" {
                if state.info_not_premium {
                    return err(None, "Account not premium.");
                }
                return ok(json!({
                    "customer_id": "1234567",
                    "premium_until": state.premium_until,
                    "limit_used": 0.42,
                    "booster_points": 0,
                    "space_used": 0,
                }));
            }
            if state.free {
                return err(None, "Account not premium.");
            }
            let field = |name: &str| {
                if form_post {
                    request.form_value(name)
                } else {
                    request.query(name).map(str::to_string)
                }
            };
            match route {
                "cache/check" => {
                    let items: Vec<&str> = request
                        .query
                        .iter()
                        .filter(|(name, _)| name == "items[]")
                        .map(|(_, value)| value.as_str())
                        .collect();
                    if items.is_empty() {
                        return reject("items[] missing");
                    }
                    let hits: Vec<bool> = items
                        .iter()
                        .map(|item| state.cached && item.eq_ignore_ascii_case(HASH))
                        .collect();
                    ok(json!({
                        "response": hits,
                        "transcoded": hits.iter().map(|_| Value::Null).collect::<Vec<_>>(),
                        "filename": hits.iter().map(|hit| if *hit { json!("Album") } else { Value::Null }).collect::<Vec<_>>(),
                        // Strings on hits, the number 0 on misses.
                        "filesize": hits.iter().map(|hit| if *hit { json!("4500") } else { json!(0) }).collect::<Vec<_>>(),
                    }))
                }
                "transfer/directdl" => {
                    let Some(src) = field("src") else {
                        return reject("src missing");
                    };
                    if !state.cached || state.directdl_refuses || hash_of(&src) != HASH {
                        return err(Some("not_found"), "Error downloading this file");
                    }
                    let content: Vec<Value> = torrent_files()
                        .iter()
                        .enumerate()
                        .map(|(index, (path, body))| {
                            let link = format!("{origin}/dl/direct/{index}");
                            json!({"path": path, "size": body.len(), "link": link,
                                   "stream_link": null, "transcode_status": "not_applicable"})
                        })
                        .collect();
                    let first = content[0].clone();
                    ok(json!({
                        "content": content,
                        "location": first["link"], "filename": "01.bin", "filesize": first["size"],
                    }))
                }
                "folder/create" => {
                    let Some(name) = field("name") else {
                        return reject("name missing");
                    };
                    let parent = field("parent_id").unwrap_or_else(|| ROOT.to_string());
                    let id = state.id("fo");
                    state.folders.insert(
                        id.clone(),
                        Folder {
                            name,
                            parent: Some(parent),
                        },
                    );
                    state.created_folders.push(id.clone());
                    ok(json!({"id": id}))
                }
                "transfer/create" => {
                    let (Some(src), Some(folder)) = (field("src"), field("folder_id")) else {
                        return reject("src and folder_id required");
                    };
                    if !src.starts_with("magnet:?xt=urn:btih:") {
                        return reject("src must be a magnet");
                    }
                    if !state.folders.contains_key(&folder) {
                        return err(Some("not_found"), "Folder not found");
                    }
                    let id = state.id("tr");
                    state.transfers.insert(
                        0,
                        Transfer {
                            id: id.clone(),
                            target: folder,
                            hash: hash_of(&src),
                            polls: 0,
                            materialized: false,
                            file_id: None,
                        },
                    );
                    ok(json!({"id": id, "name": "Album", "type": "torrent"}))
                }
                "transfer/list" => {
                    let fetch_polls = state.fetch_polls;
                    let fail = state.fail_transfer;
                    let mut list = vec![json!({
                        "id": "someone-else", "name": "Other", "status": "finished",
                        "progress": 1.0, "message": "", "folder_id": ROOT,
                        "file_id": USER_FILE, "created_at": 1_700_000_000,
                    })];
                    for index in 0..state.transfers.len() {
                        state.transfers[index].polls += 1;
                        let polls = state.transfers[index].polls;
                        let (status, progress, message) = if fail {
                            ("error", 0.0, "Could not find any peers")
                        } else if polls > fetch_polls {
                            state.materialize(index);
                            let status = if index % 2 == 0 { "finished" } else { "seeding" };
                            (status, 1.0, "")
                        } else if polls == 1 {
                            ("queued", 0.0, "")
                        } else {
                            ("running", 0.5, "Downloading from server")
                        };
                        let transfer = &state.transfers[index];
                        let done = matches!(status, "finished" | "seeding");
                        let folder_id = match (done, &transfer.file_id) {
                            (false, _) => Value::Null,
                            // Single file: the parent folder, here the root.
                            (true, Some(_)) => json!(ROOT),
                            (true, None) => json!(transfer.target),
                        };
                        list.insert(
                            0,
                            json!({
                                "id": transfer.id, "name": format!("Album {}", transfer.hash),
                                "status": status, "progress": progress, "message": message,
                                "folder_id": folder_id, "file_id": transfer.file_id,
                                "created_at": 1_800_000_000,
                                "src": format!("{origin}/api/job/src?id={}", transfer.id),
                            }),
                        );
                    }
                    ok(json!({"transfers": list}))
                }
                "folder/list" => {
                    let id = match field("id") {
                        Some(id) if id != ROOT => id,
                        _ => {
                            state.root_listed = true;
                            ROOT.to_string()
                        }
                    };
                    let Some(folder) = state.folders.get(&id) else {
                        return err(Some("not_found"), "Folder not found");
                    };
                    let mut content: Vec<Value> = state
                        .folders
                        .iter()
                        .filter(|(_, f)| f.parent.as_deref() == Some(id.as_str()))
                        .map(|(child, f)| json!({"id": child, "name": f.name, "type": "folder", "created_at": 1}))
                        .collect();
                    content.extend(
                        state
                            .items
                            .iter()
                            .filter(|(_, item)| item.folder == id)
                            .map(|(item_id, item)| {
                                json!({"id": item_id, "name": item.name, "type": "file",
                                       "size": item.body.len(), "created_at": 1,
                                       "link": format!("{origin}/dl/item/{item_id}"),
                                       "directlink": format!("{origin}/dl/item/{item_id}")})
                            }),
                    );
                    ok(json!({"name": folder.name, "folder_id": id,
                              "parent_id": folder.parent, "content": content}))
                }
                "item/details" => {
                    let Some(id) = field("id") else {
                        return reject("id missing");
                    };
                    match state.items.get(&id) {
                        Some(item) => ok(json!({
                            "id": id, "name": item.name, "size": item.body.len(),
                            "folder_id": item.folder, "type": "file",
                            "link": format!("{origin}/dl/item/{id}"),
                            "stream_link": null, "virus_scan": "ok",
                        })),
                        None => err(Some("not_found"), "File not found"),
                    }
                }
                "transfer/delete" => {
                    let Some(id) = field("id") else {
                        return reject("id missing");
                    };
                    let before = state.transfers.len();
                    state.transfers.retain(|transfer| transfer.id != id);
                    if state.transfers.len() == before {
                        // Guessed wording; no `code` on this older path.
                        return err(None, "Transfer not found");
                    }
                    state.deleted_transfers.push(id);
                    ok(json!({}))
                }
                "folder/delete" => {
                    let Some(id) = field("id") else {
                        return reject("id missing");
                    };
                    if id == ROOT {
                        state.root_delete_attempted = true;
                        return err(Some("permission_denied"), "Cannot delete root folder");
                    }
                    if !state.folders.contains_key(&id) {
                        return err(Some("not_found"), "Folder not found");
                    }
                    state.remove_folder(&id);
                    state.deleted_folders.push(id);
                    ok(json!({}))
                }
                _ => FakeResponse::json(404, json!({"status": "error", "message": "Unknown endpoint"})),
            }
        })
        .await;
        *origin_slot.lock().unwrap() = server.origin.clone();
        let base = Url::parse(&format!("{}/api/", server.origin)).unwrap();
        (server, base)
    }

    fn client(base: &Url, key: &str) -> Premiumize {
        Premiumize::new(Secret::new(key).unwrap(), Some(base.clone())).unwrap()
    }

    async fn poll_ready(pm: &Premiumize, remote_id: &str) -> (Vec<RemoteState>, Remote) {
        let mut seen = Vec::new();
        for _ in 0..10 {
            let remote = pm.status(remote_id).await.unwrap();
            if remote.state == RemoteState::Ready {
                return (seen, remote);
            }
            seen.push(remote.state);
        }
        panic!("never ready: {seen:?}");
    }

    #[tokio::test]
    async fn account_reports_premium_free_and_bad_keys() {
        let state = Arc::new(Mutex::new(State::default()));
        let (server, base) = premiumize_fake(Arc::clone(&state)).await;
        let account = client(&base, KEY).account().await.unwrap();
        assert!(account.premium);
        assert_eq!(account.expires.as_deref(), Some("2100-01-01T00:00:00Z"));
        let info = server.requests().pop().unwrap();
        assert_eq!(info.method, "GET");
        assert_eq!(info.bearer(), Some(KEY));
        assert!(info.query.is_empty(), "key never in the URL");

        state.lock().unwrap().premium_until = None;
        let free = client(&base, KEY).account().await.unwrap();
        assert!(!free.premium && free.expires.is_none());

        state.lock().unwrap().premium_until = Some(1_000_000_000);
        let lapsed = client(&base, KEY).account().await.unwrap();
        assert!(!lapsed.premium);
        assert_eq!(lapsed.expires.as_deref(), Some("2001-09-09T01:46:40Z"));

        state.lock().unwrap().info_not_premium = true;
        assert!(!client(&base, KEY).account().await.unwrap().premium);

        let bad = client(&base, "wrong").account().await.unwrap_err();
        assert!(matches!(bad, DebridError::BadKey(_)), "{bad:?}");
        assert!(!bad.to_string().contains("wrong"));
    }

    #[tokio::test]
    async fn cached_magnet_is_served_by_directdl_and_leaves_nothing_behind() {
        let state = Arc::new(Mutex::new(State {
            cached: true,
            ..State::default()
        }));
        let (server, base) = premiumize_fake(Arc::clone(&state)).await;
        let pm = client(&base, KEY);
        let added = pm.add(&magnet(), &HASH.to_ascii_uppercase()).await.unwrap();
        assert_eq!(added.remote_id, format!("direct:{HASH}"));
        assert!(!added.added_by_us);
        let check = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("cache/check"))
            .unwrap();
        assert_eq!(check.method, "GET");
        assert_eq!(check.query("items[]"), Some(HASH));

        let ready = pm.status(&added.remote_id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        let paths: Vec<_> = ready.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Album/CD1/01.bin", "Album/02.bin"]);
        assert_eq!(ready.files[0].size, 3000);

        let first = pm.link(&added.remote_id, &ready.files[0]).await.unwrap();
        let second = pm.link(&added.remote_id, &ready.files[1]).await.unwrap();
        assert!(first.ends_with("/dl/direct/0") && second.ends_with("/dl/direct/1"));
        assert_eq!(
            server.count("transfer/directdl"),
            1,
            "one answer serves status and every link"
        );
        // Asking for the same file again means its link failed: refresh.
        let again = pm.link(&added.remote_id, &ready.files[0]).await.unwrap();
        assert_eq!(again, first);
        assert_eq!(server.count("transfer/directdl"), 2);

        // A fresh adapter (restored job) rebuilds the source from the hash.
        let restored = client(&base, KEY);
        assert_eq!(
            restored.status(&added.remote_id).await.unwrap().files.len(),
            2
        );

        pm.delete(&added.remote_id).await.unwrap();
        for nothing in [
            "folder/create",
            "transfer/create",
            "transfer/delete",
            "folder/delete",
        ] {
            assert_eq!(server.count(nothing), 0, "{nothing}");
        }

        // Evicted from the cache later: gone, so the runner adds it again.
        state.lock().unwrap().cached = false;
        assert_eq!(
            client(&base, KEY)
                .status(&added.remote_id)
                .await
                .unwrap_err(),
            DebridError::Gone
        );
    }

    #[tokio::test]
    async fn uncached_magnet_fetches_into_its_own_folder_and_cleans_only_that() {
        let state = Arc::new(Mutex::new(State {
            fetch_polls: 2,
            ..State::default()
        }));
        let (server, base) = premiumize_fake(Arc::clone(&state)).await;
        let pm = client(&base, KEY);
        let added = pm.add(&magnet(), HASH).await.unwrap();
        assert!(added.added_by_us);
        let folder = state.lock().unwrap().created_folders[0].clone();
        assert_eq!(added.remote_id, format!("cloud:tr2:{folder}"));
        let created = state.lock().unwrap().folders[&folder].name.clone();
        assert!(created.starts_with("torrenter-8f14e45f-"), "{created}");
        let create = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("transfer/create"))
            .unwrap();
        assert!(create.is_form());
        assert_eq!(create.form_value("src"), Some(magnet()));
        assert_eq!(create.form_value("folder_id"), Some(folder.clone()));

        let (progress, ready) = poll_ready(&pm, &added.remote_id).await;
        assert_eq!(
            progress,
            [
                RemoteState::Fetching { permille: 0 },
                RemoteState::Fetching { permille: 500 }
            ]
        );
        let paths: Vec<_> = ready.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["Album/02.bin", "Album/CD1/01.bin"],
            "subfolder walked"
        );
        let link = pm.link(&added.remote_id, &ready.files[1]).await.unwrap();
        assert!(link.ends_with(&format!("/dl/item/{}", ready.files[1].id)));
        assert!(server.count("item/details") >= 1);

        pm.delete(&added.remote_id).await.unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.deleted_transfers, ["tr2"]);
            assert_eq!(state.deleted_folders, std::slice::from_ref(&folder));
            assert!(state.folders.contains_key(ROOT) && state.items.contains_key(USER_FILE));
            assert_eq!(state.items.len(), 1, "torrent files went with our folder");
            assert!(!state.root_listed && !state.root_delete_attempted);
        }
        assert_eq!(
            pm.status(&added.remote_id).await.unwrap_err(),
            DebridError::Gone
        );
        // Deleting twice is fine.
        pm.delete(&added.remote_id).await.unwrap();
    }

    #[tokio::test]
    async fn a_single_file_dropped_in_root_never_exposes_or_deletes_root() {
        let state = Arc::new(Mutex::new(State {
            single_into_root: true,
            ..State::default()
        }));
        let (_server, base) = premiumize_fake(Arc::clone(&state)).await;
        let pm = client(&base, KEY);
        let added = pm.add(&magnet(), HASH).await.unwrap();
        let (_, ready) = poll_ready(&pm, &added.remote_id).await;
        assert_eq!(ready.files.len(), 1, "only the transfer's own file");
        assert_eq!(ready.files[0].path, "movie.bin");
        assert_eq!(ready.files[0].size, 2048);
        assert!(pm
            .link(&added.remote_id, &ready.files[0])
            .await
            .unwrap()
            .contains("/dl/item/"));
        pm.delete(&added.remote_id).await.unwrap();
        let state = state.lock().unwrap();
        assert!(!state.root_listed, "root was listed");
        assert!(!state.root_delete_attempted, "root delete attempted");
        assert!(state.folders.contains_key(ROOT) && state.items.contains_key(USER_FILE));
        assert_eq!(state.deleted_folders.len(), 1);
        assert_ne!(state.deleted_folders[0], ROOT);
    }

    #[tokio::test]
    async fn provider_errors_map_to_debrid_errors() {
        let state = Arc::new(Mutex::new(State {
            free: true,
            ..State::default()
        }));
        let (_server, base) = premiumize_fake(Arc::clone(&state)).await;
        let pm = client(&base, KEY);
        assert!(matches!(
            pm.add(&magnet(), HASH).await,
            Err(DebridError::NotPremium(_))
        ));
        state.lock().unwrap().free = false;

        state.lock().unwrap().throttle = 1;
        assert_eq!(
            pm.add(&magnet(), HASH).await.unwrap_err(),
            DebridError::RateLimited(Some(Duration::from_secs(2)))
        );
        state.lock().unwrap().rate_envelope = 1;
        assert_eq!(
            pm.status("cloud:tr1:fo1").await.unwrap_err(),
            DebridError::RateLimited(None)
        );

        // Unknown transfer and no such folder: gone. Junk ids too.
        assert_eq!(
            pm.status("cloud:nope:missing").await.unwrap_err(),
            DebridError::Gone
        );
        assert_eq!(pm.status("garbage").await.unwrap_err(), DebridError::Gone);
        assert_eq!(
            pm.status("direct:xyz").await.unwrap_err(),
            DebridError::Gone
        );
        pm.delete("garbage").await.unwrap();

        // A cache hit that directdl refuses falls back to a cloud transfer.
        {
            let mut state = state.lock().unwrap();
            state.cached = true;
            state.directdl_refuses = true;
            state.fail_transfer = true;
        }
        let added = pm.add(&magnet(), HASH).await.unwrap();
        assert!(added.remote_id.starts_with("cloud:") && added.added_by_us);
        match pm.status(&added.remote_id).await.unwrap().state {
            RemoteState::Failed(reason) => {
                assert!(
                    reason.contains("error") && reason.contains("peers"),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }

        let refusal = |code: Option<&str>, message: &str| {
            Refusal {
                http: 200,
                code: code.map(str::to_string),
                message: message.into(),
            }
            .into_error()
        };
        assert!(matches!(
            refusal(None, "Not logged in."),
            DebridError::BadKey(_)
        ));
        assert!(matches!(
            refusal(Some("authentication_failed"), "x"),
            DebridError::BadKey(_)
        ));
        assert!(matches!(
            refusal(Some("permission_denied"), "Account not premium."),
            DebridError::NotPremium(_)
        ));
        assert!(matches!(
            refusal(None, "payment issue"),
            DebridError::NotPremium(_)
        ));
        assert!(matches!(
            refusal(None, "Fair use limit reached!"),
            DebridError::Service(_)
        ));
        assert!(matches!(
            refusal(
                Some("account_limit_reached"),
                "You already have a maximum of 25 active downloads in progress!"
            ),
            DebridError::Service(_)
        ));
        assert_eq!(refusal(None, "Folder not found"), DebridError::Gone);
        assert_eq!(
            refusal(None, "Rate limit exceeded"),
            DebridError::RateLimited(None)
        );
        assert!(matches!(
            refusal(Some("transient_error"), "try again"),
            DebridError::Network(_)
        ));
    }

    mod runner {
        use std::path::Path;

        use super::*;
        use crate::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
        use crate::debrid::Service;
        use crate::progress::JobSnapshot;

        fn run_for(dest: &Path, base: Url, key: &str, state: SharedDebridState) -> DebridRun {
            DebridRun {
                service: Service::Premiumize,
                key: Secret::new(key).unwrap(),
                magnet: magnet(),
                dest: dest.to_path_buf(),
                part_count: 4,
                remove_after: true,
                state,
                base: Some(base),
            }
        }

        /// Single connection cap: every download request reads from byte 0
        /// (one part per file), never a split range.
        fn assert_single_part(server: &FakeServer) {
            for request in server.requests() {
                if request.path.starts_with("/dl/") {
                    if let Some(range) = request.header("range") {
                        assert!(range.starts_with("bytes=0-"), "split range {range}");
                    }
                }
            }
        }

        fn assert_album(dest: &Path) {
            let job = dest.join("Album");
            let files = torrent_files();
            assert_eq!(std::fs::read(job.join("CD1/01.bin")).unwrap(), files[0].1);
            assert_eq!(std::fs::read(job.join("02.bin")).unwrap(), files[1].1);
        }

        #[tokio::test]
        async fn uncached_job_downloads_every_file_and_removes_only_our_folder() {
            let fake = Arc::new(Mutex::new(State {
                expire_links: 1,
                ..State::default()
            }));
            let (server, base) = premiumize_fake(Arc::clone(&fake)).await;
            let dir = tempfile::tempdir().unwrap();
            let state = SharedDebridState::default();
            let snapshot = Arc::new(JobSnapshot::default());
            start_debrid_reported(
                run_for(dir.path(), base, KEY, Arc::clone(&state)),
                Arc::clone(&snapshot),
            )
            .await
            .unwrap();
            assert!(snapshot.is_done(), "status {}", snapshot.status());
            assert_album(dir.path());
            assert_single_part(&server);
            let state = state.lock().unwrap().clone();
            assert!(state.added_by_us && state.files.iter().all(|f| f.done));
            let fake = fake.lock().unwrap();
            assert!(fake.transfers.is_empty());
            assert_eq!(fake.deleted_folders, fake.created_folders);
            assert!(fake.items.contains_key(USER_FILE) && !fake.root_delete_attempted);
            // The expired first link was resolved again.
            assert!(server.count("item/details") >= 3);
        }

        #[tokio::test]
        async fn cached_job_downloads_without_touching_the_account() {
            let fake = Arc::new(Mutex::new(State {
                cached: true,
                ..State::default()
            }));
            let (server, base) = premiumize_fake(Arc::clone(&fake)).await;
            let dir = tempfile::tempdir().unwrap();
            let state = SharedDebridState::default();
            let snapshot = Arc::new(JobSnapshot::default());
            start_debrid_reported(
                run_for(dir.path(), base.clone(), KEY, Arc::clone(&state)),
                Arc::clone(&snapshot),
            )
            .await
            .unwrap();
            assert!(snapshot.is_done(), "status {}", snapshot.status());
            assert_album(dir.path());
            assert_single_part(&server);
            assert!(!state.lock().unwrap().added_by_us);
            for nothing in ["folder/create", "transfer/delete", "folder/delete"] {
                assert_eq!(server.count(nothing), 0, "{nothing}");
            }

            // A bad key fails with text that carries neither key nor link.
            let other = tempfile::tempdir().unwrap();
            let snapshot = Arc::new(JobSnapshot::default());
            let error = start_debrid_reported(
                run_for(
                    other.path(),
                    base,
                    "wrong-key-123",
                    SharedDebridState::default(),
                ),
                snapshot,
            )
            .await
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("API key rejected"), "{text}");
            assert!(
                !text.contains("wrong-key-123") && !text.contains("http"),
                "{text}"
            );
        }
    }
}

//! Real-Debrid (api.real-debrid.com/rest/1.0). Research: .cursor/plans/debrid-research/real-debrid.md
//!
//! Auth is the user's private API token as `Authorization: Bearer` only, never
//! the `?auth_token=` query, so no request URL carries it. The OAuth device
//! flow is not implemented.
//!
//! Quirks encoded here: adding the same hash twice makes a second torrent, so
//! `add` looks for it in `GET torrents` first (skipping failed copies); a
//! torrent waits in `waiting_files_selection` until `selectFiles` with
//! `files=all` (204, or 202 / code 31 when already done); `progress` is a 0–100
//! percent, so `1` means 1 %, not done; `links[]` pairs with the selected files
//! in order, except when RD packs files into an archive and lists fewer links;
//! a `downloaded` torrent with no links has lost its data; errors are
//! `{error, error_code}` where either may be null, so the code decides and the
//! HTTP status is the fallback; 451 / code 35 `infringing_file` is a refusal
//! (shown, never retried or worked around); `instantAvailability` is gone and
//! not used. The CDN ignores the end of a `Range`; the HTTP engine caps parts.
//!
//! File ids are link indexes. When the link count matches the selected files,
//! names and sizes come from the torrent's file list (no extra calls). When RD
//! packed files, each link is unrestricted once to learn the archive's name and
//! size. `link()` always unrestricts again, so every call gives a fresh URL.

use reqwest::RequestBuilder;
use serde_json::Value;
use url::Url;

use super::api::{number, text, Api, Reply};
use super::{Account, Added, DebridError, Remote, RemoteFile, RemoteState, Secret};

const BASE: &str = "https://api.real-debrid.com/rest/1.0/";
/// `GET torrents` page size; the API allows up to 5000.
const LIST_LIMIT: &str = "5000";
/// Terminal failure statuses (ResolveURL / rdt-client / Riven agree).
const FAILED: [&str; 4] = ["magnet_error", "error", "virus", "dead"];
/// `error_code` 31: "Action already done".
const ALREADY_DONE: i64 = 31;

pub(crate) struct RealDebrid {
    api: Api,
    key: Secret,
}

impl RealDebrid {
    pub fn new(key: Secret, base: Option<Url>) -> Result<RealDebrid, DebridError> {
        let base = base.unwrap_or_else(|| Url::parse(BASE).expect("valid Real-Debrid base"));
        Ok(RealDebrid {
            api: Api::new(base)?,
            key,
        })
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.api
            .client()
            .get(self.api.url(path))
            .header("Authorization", format!("Bearer {}", self.key.expose()))
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> RequestBuilder {
        self.api
            .client()
            .post(self.api.url(path))
            .header("Authorization", format!("Bearer {}", self.key.expose()))
            .form(form)
    }

    async fn call(&self, request: RequestBuilder) -> Result<Value, DebridError> {
        checked(self.api.send(request).await?)
    }

    pub async fn account(&self) -> Result<Account, DebridError> {
        let data = self.call(self.get("user")).await?;
        let premium = match data.get("type").and_then(Value::as_str) {
            Some(kind) => kind.eq_ignore_ascii_case("premium"),
            None => data.get("premium").and_then(number).unwrap_or(0.0) > 0.0,
        };
        Ok(Account {
            premium,
            expires: data
                .get("expiration")
                .and_then(text)
                .filter(|expires| !expires.trim().is_empty()),
        })
    }

    /// Re-use a live torrent with this hash (RD does not dedupe adds),
    /// otherwise add the magnet.
    pub async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        if let Some(remote_id) = self.find_by_hash(hash_hex).await? {
            return Ok(Added {
                remote_id,
                added_by_us: false,
            });
        }
        let data = self
            .call(self.post_form("torrents/addMagnet", &[("magnet", magnet)]))
            .await?;
        let remote_id = data
            .get("id")
            .and_then(text)
            .filter(|id| valid_id(id))
            .ok_or_else(|| DebridError::Service("Real-Debrid returned no torrent id".into()))?;
        Ok(Added {
            remote_id,
            added_by_us: true,
        })
    }

    async fn find_by_hash(&self, hash_hex: &str) -> Result<Option<String>, DebridError> {
        let request = self.get("torrents").query(&[("limit", LIST_LIMIT)]);
        let data = match self.call(request).await {
            Ok(data) => data,
            Err(DebridError::Gone) => return Ok(None),
            Err(error) => return Err(error),
        };
        // An empty account can answer 204, and RD sometimes sends a non-array.
        let Some(items) = data.as_array() else {
            return Ok(None);
        };
        Ok(items
            .iter()
            .filter(|item| {
                item.get("hash")
                    .and_then(Value::as_str)
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex))
            })
            .filter(|item| {
                let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                !FAILED.contains(&status.to_ascii_lowercase().as_str())
            })
            .find_map(|item| item.get("id").and_then(text).filter(|id| valid_id(id))))
    }

    async fn info(&self, remote_id: &str) -> Result<Value, DebridError> {
        if !valid_id(remote_id) {
            return Err(DebridError::Gone);
        }
        let data = self
            .call(self.get(&format!("torrents/info/{remote_id}")))
            .await?;
        // The docs show an array; the live API answers an object.
        match data {
            Value::Object(_) => Ok(data),
            Value::Array(mut items) if !items.is_empty() => Ok(items.swap_remove(0)),
            _ => Err(DebridError::Service(
                "Real-Debrid returned no torrent info".into(),
            )),
        }
    }

    async fn select_all(&self, remote_id: &str) -> Result<(), DebridError> {
        let request = self.post_form(
            &format!("torrents/selectFiles/{remote_id}"),
            &[("files", "all")],
        );
        let reply = self.api.send(request).await?;
        if reply.json.get("error_code").and_then(Value::as_i64) == Some(ALREADY_DONE) {
            return Ok(());
        }
        checked(reply).map(|_| ())
    }

    async fn unrestrict(&self, link: &str) -> Result<Value, DebridError> {
        self.call(self.post_form("unrestrict/link", &[("link", link)]))
            .await
    }

    pub async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        let info = self.info(remote_id).await?;
        let status = info
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let fetching = |permille| Remote {
            state: RemoteState::Fetching { permille },
            files: Vec::new(),
        };
        let failed = |reason: String| Remote {
            state: RemoteState::Failed(reason),
            files: Vec::new(),
        };
        match status.as_str() {
            "downloaded" => {}
            "waiting_files_selection" => {
                self.select_all(remote_id).await?;
                return Ok(fetching(0));
            }
            name if FAILED.contains(&name) => {
                return Ok(failed(format!("Real-Debrid reports the torrent as {name}")));
            }
            // magnet_conversion, queued, downloading, compressing, uploading.
            _ => return Ok(fetching(percent_permille(info.get("progress")))),
        }
        let links = links(&info);
        if links.is_empty() {
            return Ok(failed(
                "Real-Debrid finished the torrent but lists no links (its data was dropped)".into(),
            ));
        }
        let selected = selected_files(&info);
        let files = if selected.len() == links.len() {
            selected
                .into_iter()
                .enumerate()
                .map(|(index, (path, size))| RemoteFile {
                    id: index.to_string(),
                    path,
                    size,
                })
                .collect()
        } else {
            // RD packed some files into archives: ask each link what it is.
            let mut files: Vec<RemoteFile> = Vec::with_capacity(links.len());
            for (index, link) in links.iter().enumerate() {
                let data = self.unrestrict(link).await?;
                let mut name = data
                    .get("filename")
                    .and_then(text)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| format!("part{}.bin", index + 1));
                if files.iter().any(|file| file.path == name) {
                    name = format!("{}-{name}", index + 1);
                }
                files.push(RemoteFile {
                    id: index.to_string(),
                    path: name,
                    size: data
                        .get("filesize")
                        .and_then(number)
                        .unwrap_or(0.0)
                        .max(0.0) as u64,
                });
            }
            files
        };
        Ok(Remote {
            state: RemoteState::Ready,
            files,
        })
    }

    /// A fresh CDN URL for one link index: re-read the torrent's links and
    /// unrestrict that one again (generated links expire and are IP-bound).
    pub async fn link(&self, remote_id: &str, file: &RemoteFile) -> Result<String, DebridError> {
        let index: usize = file
            .id
            .parse()
            .map_err(|_| DebridError::Service("Real-Debrid file id is not a link index".into()))?;
        let info = self.info(remote_id).await?;
        let link = links(&info).into_iter().nth(index).ok_or_else(|| {
            DebridError::Service("Real-Debrid no longer lists a link for this file".into())
        })?;
        let data = self.unrestrict(&link).await?;
        data.get("download")
            .and_then(Value::as_str)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .map(str::to_string)
            .ok_or_else(|| DebridError::Service("Real-Debrid returned no download link".into()))
    }

    pub async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        if !valid_id(remote_id) {
            return Ok(());
        }
        let request = self
            .api
            .client()
            .delete(self.api.url(&format!("torrents/delete/{remote_id}")))
            .header("Authorization", format!("Bearer {}", self.key.expose()));
        match self.call(request).await {
            Ok(_) | Err(DebridError::Gone) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// RD torrent ids are short alphanumeric strings; anything else must not be
/// spliced into a request path.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// `progress` is a 0–100 percent (sometimes a float).
fn percent_permille(progress: Option<&Value>) -> u16 {
    let percent = progress.and_then(number).unwrap_or(0.0);
    if !percent.is_finite() || percent <= 0.0 {
        return 0;
    }
    (percent.min(100.0) * 10.0).round() as u16
}

fn links(info: &Value) -> Vec<String> {
    info.get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .filter_map(Value::as_str)
                .filter(|link| !link.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Selected files in `files` order as `(path without the leading "/", bytes)`.
fn selected_files(info: &Value) -> Vec<(String, u64)> {
    info.get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter(|file| file.get("selected").and_then(number).unwrap_or(0.0) >= 1.0)
                .filter_map(|file| {
                    let path = file.get("path").and_then(text)?;
                    let size = file.get("bytes").and_then(number).unwrap_or(0.0).max(0.0) as u64;
                    Some((path.trim_start_matches('/').to_string(), size))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map `{error, error_code}` (either may be null) onto `DebridError`. The code
/// decides; the HTTP status is the fallback. 429 and 503 never get here
/// (`Api::send` reports them as rate limits).
fn checked(reply: Reply) -> Result<Value, DebridError> {
    let code = reply.json.get("error_code").and_then(Value::as_i64);
    let slug = reply
        .json
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|slug| !slug.is_empty() && slug.len() <= 64);
    let status = reply.status.as_u16();
    if reply.status.is_success() && code.is_none() && slug.is_none() {
        return Ok(reply.json);
    }
    let detail = match (slug, code.and_then(code_message)) {
        (Some(slug), Some(message)) => format!("{message} ({slug})"),
        (Some(slug), None) => slug.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => format!("HTTP {status}"),
    };
    Err(match (code, status) {
        (Some(8), _) | (_, 401) => DebridError::BadKey(detail),
        (Some(35), _) | (_, 451) => DebridError::Blocked(format!("Real-Debrid: {detail}")),
        (Some(9 | 14 | 15 | 20), _) | (None, 403) => DebridError::NotPremium(detail),
        (Some(5 | 34), _) => DebridError::RateLimited(None),
        (Some(7), _) | (None, 404) => DebridError::Gone,
        _ => DebridError::Service(format!("Real-Debrid: {detail}")),
    })
}

/// The official `error_code` table.
fn code_message(code: i64) -> Option<&'static str> {
    Some(match code {
        -1 => "Internal error",
        1 => "Missing parameter",
        2 => "Bad parameter value",
        3 => "Unknown method",
        4 => "Method not allowed",
        5 => "Slow down",
        6 => "Ressource unreachable",
        7 => "Resource not found",
        8 => "Bad token",
        9 => "Permission denied",
        10 => "Two-Factor authentication needed",
        11 => "Two-Factor authentication pending",
        12 => "Invalid login",
        13 => "Invalid password",
        14 => "Account locked",
        15 => "Account not activated",
        16 => "Unsupported hoster",
        17 => "Hoster in maintenance",
        18 => "Hoster limit reached",
        19 => "Hoster temporarily unavailable",
        20 => "Hoster not available for free users",
        21 => "Too many active downloads",
        22 => "IP Address not allowed",
        23 => "Traffic exhausted",
        24 => "File unavailable",
        25 => "Service unavailable",
        26 => "Upload too big",
        27 => "Upload error",
        28 => "File not allowed",
        29 => "Torrent too big",
        30 => "Torrent file invalid",
        31 => "Action already done",
        32 => "Image resolution error",
        33 => "Torrent already active",
        34 => "Too many requests",
        35 => "Infringing file",
        36 => "Fair Usage Limit",
        37 => "Disabled endpoint",
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use reqwest::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::debrid::fake::{serve, FakeRequest, FakeResponse, FakeServer};
    use crate::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
    use crate::debrid::Service;
    use crate::progress::JobSnapshot;

    pub const KEY: &str = "rd-test-token-XYZ";
    pub const HASH: &str = "5a8ce26e8a19a877d8ccc927fcc18e34e1f5ff67";

    #[derive(Default)]
    pub struct State {
        pub torrents: BTreeMap<String, Torrent>,
        pub next_id: u32,
        pub premium: bool,
        /// Polls in `magnet_conversion` before `waiting_files_selection`.
        pub conversion_polls: u32,
        /// Polls in `downloading` after `selectFiles`.
        pub download_polls: u32,
        /// Pack every file into one archive link.
        pub pack: bool,
        /// `downloaded` but with no links (RD dropped the data).
        pub drop_links: bool,
        /// Answer the first N CDN downloads with 404 + `X-Error`.
        pub expire_links: u32,
        /// Scripted reply for the next `addMagnet`.
        pub add_error: Option<(u16, Value)>,
        /// CDN code → bytes.
        pub downloads: BTreeMap<String, (String, Vec<u8>)>,
        pub next_code: u32,
    }

    pub struct Torrent {
        pub hash: String,
        pub name: String,
        pub files: Vec<(String, Vec<u8>)>,
        pub polls: u32,
        pub download_polls: u32,
        pub selected: bool,
        pub selects: u32,
        pub failed: Option<&'static str>,
    }

    fn rd_error(status: u16, error: Value, code: Value) -> FakeResponse {
        FakeResponse::json(status, json!({"error": error, "error_code": code}))
    }

    fn unknown() -> FakeResponse {
        rd_error(404, json!("unknown_ressource"), json!(7))
    }

    fn wrong_parameter() -> FakeResponse {
        rd_error(400, json!("wrong_parameter"), json!(2))
    }

    fn current_status(torrent: &Torrent, state: &State) -> &'static str {
        if let Some(failed) = torrent.failed {
            failed
        } else if !torrent.selected {
            if torrent.polls <= state.conversion_polls && torrent.polls > 0 {
                "magnet_conversion"
            } else {
                "waiting_files_selection"
            }
        } else if torrent.download_polls <= state.download_polls {
            "downloading"
        } else {
            "downloaded"
        }
    }

    fn torrent_links(id: &str, torrent: &Torrent, state: &State) -> Vec<String> {
        if state.drop_links || current_status(torrent, state) != "downloaded" {
            return Vec::new();
        }
        let count = if state.pack { 1 } else { torrent.files.len() };
        (0..count)
            .map(|index| format!("https://real-debrid.com/d/{id}-{index}"))
            .collect()
    }

    fn torrent_json(id: &str, torrent: &Torrent, state: &State, with_files: bool) -> Value {
        let status = current_status(torrent, state);
        let mut item = json!({
            "id": id,
            "filename": torrent.name,
            "hash": torrent.hash,
            "bytes": torrent.files.iter().map(|(_, body)| body.len()).sum::<usize>(),
            "host": "real-debrid.com",
            "split": 2000,
            "progress": match status { "downloaded" => 100, "downloading" => 45, _ => 0 },
            "status": status,
            "added": "2026-11-14T10:00:00.000Z",
            "links": torrent_links(id, torrent, state),
        });
        if with_files {
            let selected = if torrent.selected { 1 } else { 0 };
            let files: Vec<Value> = torrent
                .files
                .iter()
                .enumerate()
                .map(|(index, (path, body))| {
                    json!({"id": index + 1, "path": path, "bytes": body.len(), "selected": selected})
                })
                .collect();
            item["files"] = Value::from(files);
        }
        if status == "downloading" {
            item["speed"] = json!(1_000_000);
            item["seeders"] = json!(7);
        }
        if status == "downloaded" {
            item["ended"] = json!("2026-11-14T10:05:00.000Z");
        }
        item
    }

    fn album_files() -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "/CD1/01.bin".to_string(),
                (0..30_011u32).map(|i| (i % 251) as u8).collect(),
            ),
            (
                "/02.bin".to_string(),
                (0..15_007u32).map(|i| (i % 13) as u8).collect(),
            ),
        ]
    }

    /// A Real-Debrid fake that enforces bearer auth, form bodies, verbs, the
    /// file-selection step and the CDN's open-ended ranges.
    pub async fn realdebrid_fake(state: Arc<Mutex<State>>) -> (FakeServer, Url) {
        let origin_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let origin_for_handler = Arc::clone(&origin_slot);
        let server = serve(move |request: &FakeRequest| {
            let mut state = state.lock().unwrap();
            let path = request.path.as_str();
            if let Some(rest) = path.strip_prefix("/d/") {
                // CDN: no API credential, ignores the Range end.
                if request.header("authorization").is_some() {
                    return FakeResponse::empty(400);
                }
                if state.expire_links > 0 {
                    state.expire_links -= 1;
                    return FakeResponse::empty(404)
                        .with_header("X-Error", "invalid_download_code");
                }
                let code = rest.split('/').next().unwrap_or_default();
                return match state.downloads.get(code) {
                    Some((_, body)) => FakeResponse::file(request, body, true),
                    None => {
                        FakeResponse::empty(404).with_header("X-Error", "invalid_download_code")
                    }
                };
            }
            let Some(route) = path.strip_prefix("/rest/1.0/") else {
                return rd_error(404, json!("unknown_method"), json!(3));
            };
            if request.query("auth_token").is_some() {
                return wrong_parameter();
            }
            if request.bearer() != Some(KEY) {
                return rd_error(401, json!("bad_token"), json!(8));
            }
            let method = request.method.as_str();
            let segments: Vec<&str> = route.split('/').collect();
            match (method, segments.as_slice()) {
                ("GET", ["user"]) => FakeResponse::json(
                    200,
                    json!({
                        "id": 42, "username": "tester", "email": "t@example.invalid", "points": 1,
                        "locale": "en", "avatar": "",
                        "type": if state.premium { "premium" } else { "free" },
                        "premium": if state.premium { 666_666 } else { 0 },
                        "expiration": "2032-06-06T04:42:42.000Z",
                    }),
                ),
                ("GET", ["torrents"]) => {
                    if state.torrents.is_empty() {
                        return FakeResponse::empty(204);
                    }
                    let list: Vec<Value> = state
                        .torrents
                        .iter()
                        .map(|(id, torrent)| torrent_json(id, torrent, &state, false))
                        .collect();
                    FakeResponse::json(200, Value::from(list))
                        .with_header("X-Total-Count", &state.torrents.len().to_string())
                }
                ("POST", ["torrents", "addMagnet"]) => {
                    if !request.is_form() {
                        return wrong_parameter();
                    }
                    let Some(magnet) = request.form_value("magnet") else {
                        return rd_error(400, json!("parameter_missing"), json!(1));
                    };
                    if let Some((status, body)) = state.add_error.take() {
                        return FakeResponse::json(status, body);
                    }
                    let hash = magnet
                        .split("btih:")
                        .nth(1)
                        .and_then(|rest| rest.split('&').next())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if hash.len() != 40 {
                        return wrong_parameter();
                    }
                    // RD never dedupes: every add is a new torrent.
                    state.next_id += 1;
                    let id = format!("RDTEST{:07}", state.next_id);
                    state.torrents.insert(
                        id.clone(),
                        Torrent {
                            hash,
                            name: "Album".into(),
                            files: album_files(),
                            polls: 0,
                            download_polls: 0,
                            selected: false,
                            selects: 0,
                            failed: None,
                        },
                    );
                    let origin = origin_for_handler.lock().unwrap().clone();
                    FakeResponse::json(
                        201,
                        json!({"id": id, "uri": format!("{origin}/rest/1.0/torrents/info/{id}")}),
                    )
                }
                ("GET", ["torrents", "info", id]) => {
                    let conversion = state.conversion_polls;
                    let Some(torrent) = state.torrents.get_mut(*id) else {
                        return unknown();
                    };
                    if torrent.selected {
                        torrent.download_polls += 1;
                    } else if torrent.polls <= conversion {
                        torrent.polls += 1;
                    }
                    let torrent = &state.torrents[*id];
                    FakeResponse::json(200, torrent_json(id, torrent, &state, true))
                }
                ("POST", ["torrents", "selectFiles", id]) => {
                    if !request.is_form() || request.form_value("files").as_deref() != Some("all") {
                        return wrong_parameter();
                    }
                    let Some(torrent) = state.torrents.get_mut(*id) else {
                        return unknown();
                    };
                    if torrent.selected {
                        return FakeResponse::empty(202);
                    }
                    torrent.selected = true;
                    torrent.selects += 1;
                    FakeResponse::empty(204)
                }
                ("POST", ["unrestrict", "link"]) => {
                    if !request.is_form() {
                        return wrong_parameter();
                    }
                    let link = request.form_value("link").unwrap_or_default();
                    let Some((id, index)) = link
                        .strip_prefix("https://real-debrid.com/d/")
                        .and_then(|rest| rest.split_once('-'))
                    else {
                        return wrong_parameter();
                    };
                    let index: usize = index.parse().unwrap_or(usize::MAX);
                    let Some(torrent) = state.torrents.get(id) else {
                        return rd_error(404, json!("unavailable_file"), json!(24));
                    };
                    let (name, body) = if state.pack {
                        let archive: Vec<u8> = torrent
                            .files
                            .iter()
                            .flat_map(|(_, body)| body.iter().copied())
                            .collect();
                        (format!("{}.rar", torrent.name), archive)
                    } else {
                        match torrent.files.get(index) {
                            Some((path, body)) => (
                                path.rsplit('/').next().unwrap_or_default().to_string(),
                                body.clone(),
                            ),
                            None => return rd_error(404, json!("unavailable_file"), json!(24)),
                        }
                    };
                    state.next_code += 1;
                    let code = format!("CDN{:010}", state.next_code);
                    let origin = origin_for_handler.lock().unwrap().clone();
                    let encoded: String =
                        url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
                    let download = format!("{origin}/d/{code}/{encoded}");
                    let size = body.len();
                    state.downloads.insert(code.clone(), (name.clone(), body));
                    FakeResponse::json(
                        200,
                        json!({
                            "id": code, "filename": name, "mimeType": "application/octet-stream",
                            "filesize": size, "link": link, "host": "real-debrid.com",
                            "chunks": 8, "crc": 1, "download": download, "streamable": 0,
                        }),
                    )
                }
                ("DELETE", ["torrents", "delete", id]) => match state.torrents.remove(*id) {
                    Some(_) => FakeResponse::empty(204),
                    None => unknown(),
                },
                _ => rd_error(404, json!("unknown_method"), json!(3)),
            }
        })
        .await;
        *origin_slot.lock().unwrap() = server.origin.clone();
        let base = Url::parse(&format!("{}/rest/1.0/", server.origin)).unwrap();
        (server, base)
    }

    pub fn magnet() -> String {
        format!("magnet:?xt=urn:btih:{HASH}&dn=Album")
    }

    fn client(base: &Url, key: &str) -> RealDebrid {
        RealDebrid::new(Secret::new(key).unwrap(), Some(base.clone())).unwrap()
    }

    fn reply(status: u16, json: Value) -> Reply {
        Reply {
            status: StatusCode::from_u16(status).unwrap(),
            json,
        }
    }

    #[tokio::test]
    async fn account_reports_premium_free_and_bad_tokens() {
        let state = Arc::new(Mutex::new(State {
            premium: true,
            ..State::default()
        }));
        let (server, base) = realdebrid_fake(Arc::clone(&state)).await;
        let account = client(&base, KEY).account().await.unwrap();
        assert!(account.premium);
        assert_eq!(account.expires.as_deref(), Some("2032-06-06T04:42:42.000Z"));
        state.lock().unwrap().premium = false;
        assert!(!client(&base, KEY).account().await.unwrap().premium);
        let error = client(&base, "wrong-token").account().await.unwrap_err();
        assert!(matches!(error, DebridError::BadKey(_)), "{error:?}");
        assert!(!error.to_string().contains("wrong-token"));
        // The token only ever travels in the Authorization header.
        assert!(server.requests().iter().all(|request| {
            request.query.iter().all(|(_, value)| !value.contains(KEY))
                && !request.path.contains(KEY)
        }));
    }

    #[tokio::test]
    async fn add_select_poll_link_delete_flow() {
        let state = Arc::new(Mutex::new(State {
            conversion_polls: 1,
            download_polls: 1,
            ..State::default()
        }));
        let (server, base) = realdebrid_fake(Arc::clone(&state)).await;
        let rd = client(&base, KEY);
        let added = rd.add(&magnet(), HASH).await.unwrap();
        assert!(added.added_by_us);
        assert_eq!(added.remote_id, "RDTEST0000001");
        let add = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("addMagnet"))
            .unwrap();
        assert!(add.is_form());
        assert_eq!(add.form_value("magnet"), Some(magnet()));

        let id = added.remote_id.as_str();
        // magnet_conversion → waiting_files_selection (selects all) → downloading → downloaded
        let states: Vec<RemoteState> = {
            let mut out = Vec::new();
            for _ in 0..4 {
                out.push(rd.status(id).await.unwrap().state);
            }
            out
        };
        assert_eq!(
            states,
            [
                RemoteState::Fetching { permille: 0 },
                RemoteState::Fetching { permille: 0 },
                RemoteState::Fetching { permille: 450 },
                RemoteState::Ready,
            ]
        );
        assert_eq!(server.count("/selectFiles/RDTEST0000001"), 1);
        assert_eq!(state.lock().unwrap().torrents[id].selects, 1);
        let select = server
            .requests()
            .into_iter()
            .find(|r| r.path.contains("selectFiles"))
            .unwrap();
        assert_eq!(select.method, "POST");
        assert_eq!(select.form_value("files").as_deref(), Some("all"));

        let ready = rd.status(id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        assert_eq!(
            ready.files,
            [
                RemoteFile {
                    id: "0".into(),
                    path: "CD1/01.bin".into(),
                    size: 30_011
                },
                RemoteFile {
                    id: "1".into(),
                    path: "02.bin".into(),
                    size: 15_007
                },
            ]
        );
        assert_eq!(server.count("/selectFiles/RDTEST0000001"), 1);
        assert_eq!(
            server.count("unrestrict/link"),
            0,
            "counts matched: no unrestrict"
        );

        // Every link() unrestricts again and returns a fresh URL.
        let first = rd.link(id, &ready.files[1]).await.unwrap();
        let again = rd.link(id, &ready.files[1]).await.unwrap();
        assert_ne!(first, again);
        assert!(first.ends_with("/02.bin"), "{first}");
        let unrestrict = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("unrestrict/link"))
            .unwrap();
        assert!(unrestrict.is_form());
        assert_eq!(
            unrestrict.form_value("link").as_deref(),
            Some("https://real-debrid.com/d/RDTEST0000001-1")
        );
        assert!(matches!(
            rd.link(
                id,
                &RemoteFile {
                    id: "9".into(),
                    path: "x".into(),
                    size: 1
                }
            )
            .await,
            Err(DebridError::Service(_))
        ));

        // Re-adding the same hash re-uses it and never claims ownership.
        let again = rd.add(&magnet(), HASH).await.unwrap();
        assert_eq!(again.remote_id, added.remote_id);
        assert!(!again.added_by_us);
        assert_eq!(server.count("addMagnet"), 1);

        rd.delete(id).await.unwrap();
        let delete = server
            .requests()
            .into_iter()
            .find(|r| r.path.contains("/torrents/delete/"))
            .unwrap();
        assert_eq!(delete.method, "DELETE");
        assert!(state.lock().unwrap().torrents.is_empty());
        assert!(matches!(rd.status(id).await, Err(DebridError::Gone)));
        // Deleting twice is fine; an id that is not RD-shaped never hits a path.
        rd.delete(id).await.unwrap();
        assert!(matches!(rd.status("../user").await, Err(DebridError::Gone)));
    }

    #[tokio::test]
    async fn packed_archives_and_failed_or_dropped_torrents() {
        let state = Arc::new(Mutex::new(State {
            pack: true,
            ..State::default()
        }));
        let (server, base) = realdebrid_fake(Arc::clone(&state)).await;
        let rd = client(&base, KEY);
        let id = rd.add(&magnet(), HASH).await.unwrap().remote_id;
        assert_eq!(
            rd.status(&id).await.unwrap().state,
            RemoteState::Fetching { permille: 0 }
        );
        let ready = rd.status(&id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        // Two selected files, one link: name and size come from unrestrict.
        assert_eq!(
            ready.files,
            [RemoteFile {
                id: "0".into(),
                path: "Album.rar".into(),
                size: 45_018
            }]
        );
        assert_eq!(server.count("unrestrict/link"), 1);
        assert!(rd
            .link(&id, &ready.files[0])
            .await
            .unwrap()
            .ends_with("/Album.rar"));

        // A failed copy of the hash is not re-used.
        state.lock().unwrap().torrents.get_mut(&id).unwrap().failed = Some("dead");
        assert_eq!(
            rd.status(&id).await.unwrap().state,
            RemoteState::Failed("Real-Debrid reports the torrent as dead".into())
        );
        let fresh = rd.add(&magnet(), HASH).await.unwrap();
        assert!(fresh.added_by_us);
        assert_ne!(fresh.remote_id, id);

        // `downloaded` with no links means RD dropped the data.
        state.lock().unwrap().drop_links = true;
        rd.status(&fresh.remote_id).await.unwrap();
        assert!(matches!(
            rd.status(&fresh.remote_id).await.unwrap().state,
            RemoteState::Failed(reason) if reason.contains("lists no links")
        ));
    }

    #[tokio::test]
    async fn add_and_status_errors_map_to_debrid_errors() {
        let state = Arc::new(Mutex::new(State::default()));
        let (_server, base) = realdebrid_fake(Arc::clone(&state)).await;
        let rd = client(&base, KEY);
        state.lock().unwrap().add_error =
            Some((451, json!({"error": "infringing_file", "error_code": 35})));
        let error = rd.add(&magnet(), HASH).await.unwrap_err();
        assert_eq!(
            error,
            DebridError::Blocked("Real-Debrid: Infringing file (infringing_file)".into())
        );
        state.lock().unwrap().add_error =
            Some((429, json!({"error": "too_many_requests", "error_code": 34})));
        assert!(matches!(
            rd.add(&magnet(), HASH).await,
            Err(DebridError::RateLimited(_))
        ));
        state.lock().unwrap().add_error =
            Some((403, json!({"error": "permission_denied", "error_code": 9})));
        assert!(matches!(
            rd.add(&magnet(), HASH).await,
            Err(DebridError::NotPremium(_))
        ));
        assert!(matches!(
            rd.status("NOSUCHTORRENT").await,
            Err(DebridError::Gone)
        ));
        assert!(matches!(rd.delete("NOSUCHTORRENT").await, Ok(())));
        let bad = client(&base, "nope").status("NOSUCHTORRENT").await;
        assert!(matches!(bad, Err(DebridError::BadKey(_))), "{bad:?}");
    }

    #[test]
    fn error_envelope_branches_on_code_with_status_fallback() {
        let map = |status, json| checked(reply(status, json)).unwrap_err();
        assert_eq!(
            map(401, json!({"error": "bad_token", "error_code": 8})),
            DebridError::BadKey("Bad token (bad_token)".into())
        );
        // Both fields null: the status decides.
        assert_eq!(
            map(401, json!({"error": null, "error_code": null})),
            DebridError::BadKey("HTTP 401".into())
        );
        assert!(matches!(
            map(403, json!({"error": null, "error_code": null})),
            DebridError::NotPremium(_)
        ));
        assert_eq!(
            map(404, json!({"error": null, "error_code": null})),
            DebridError::Gone
        );
        assert!(matches!(map(451, Value::Null), DebridError::Blocked(_)));
        assert!(matches!(
            map(400, json!({"error": "slow_down", "error_code": 5})),
            DebridError::RateLimited(None)
        ));
        assert!(matches!(
            map(403, json!({"error": "account_locked", "error_code": 14})),
            DebridError::NotPremium(_)
        ));
        assert_eq!(
            map(400, json!({"error": null, "error_code": 21})),
            DebridError::Service("Real-Debrid: Too many active downloads".into())
        );
        assert_eq!(
            map(500, json!({"error": "something_new", "error_code": null})),
            DebridError::Service("Real-Debrid: something_new".into())
        );
        // Bad parameter on a 404 is not "gone".
        assert!(matches!(
            map(404, json!({"error": "wrong_parameter", "error_code": 2})),
            DebridError::Service(_)
        ));
        assert_eq!(checked(reply(204, Value::Null)).unwrap(), Value::Null);
        assert_eq!(percent_permille(Some(&json!(1))), 10, "1 is 1 %, not done");
        assert_eq!(percent_permille(Some(&json!(45.5))), 455);
        assert_eq!(percent_permille(Some(&json!("100"))), 1000);
        assert!(valid_id("FKNNEZ6TODDLS") && !valid_id("a/b") && !valid_id(""));
    }

    fn run_for(
        dest: &std::path::Path,
        base: Url,
        state: SharedDebridState,
        key: &str,
    ) -> DebridRun {
        DebridRun {
            service: Service::RealDebrid,
            key: Secret::new(key).unwrap(),
            magnet: magnet(),
            dest: dest.to_path_buf(),
            part_count: 4,
            remove_after: true,
            state,
            base: Some(base),
        }
    }

    #[tokio::test]
    async fn realdebrid_job_downloads_byte_identical_files() {
        let fake = Arc::new(Mutex::new(State {
            conversion_polls: 0,
            download_polls: 0,
            expire_links: 1,
            ..State::default()
        }));
        let (server, base) = realdebrid_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        tokio::time::timeout(
            Duration::from_secs(30),
            start_debrid_reported(
                run_for(dir.path(), base.clone(), Arc::clone(&state), KEY),
                Arc::clone(&snapshot),
            ),
        )
        .await
        .expect("job finishes")
        .unwrap();
        assert!(snapshot.is_done(), "status {}", snapshot.status());
        let job = dir.path().join("Album");
        let expected = album_files();
        assert_eq!(
            std::fs::read(job.join("CD1/01.bin")).unwrap(),
            expected[0].1
        );
        assert_eq!(std::fs::read(job.join("02.bin")).unwrap(), expected[1].1);
        let state = state.lock().unwrap().clone();
        assert_eq!(state.remote_id.as_deref(), Some("RDTEST0000001"));
        assert!(state.added_by_us);
        assert!(state.files.iter().all(|file| file.done));
        // selectFiles ran once; the expired first link was unrestricted again;
        // the CDN saw several open-ended range requests; remove-after deleted it.
        assert_eq!(server.count("/selectFiles/RDTEST0000001"), 1);
        assert!(server.count("unrestrict/link") >= 3);
        let ranged = server
            .requests()
            .iter()
            .filter(|r| r.path.starts_with("/d/") && r.header("range").is_some())
            .count();
        assert!(ranged >= 4, "multi-part download, got {ranged}");
        assert_eq!(server.count("/torrents/delete/RDTEST0000001"), 1);
        assert!(fake.lock().unwrap().torrents.is_empty());
        assert!(server.requests().iter().all(|request| {
            request.query.iter().all(|(_, value)| !value.contains(KEY))
                && (!request.path.starts_with("/d/") || request.bearer().is_none())
        }));

        // Errors never carry the key or a link: a bad token, then a refusal.
        let snapshot = Arc::new(JobSnapshot::default());
        let wrong = "wrong-rd-token-123";
        let error = start_debrid_reported(
            run_for(
                dir.path(),
                base.clone(),
                SharedDebridState::default(),
                wrong,
            ),
            snapshot,
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("API key rejected"), "{text}");
        assert!(!text.contains(wrong) && !text.contains("http"), "{text}");

        fake.lock().unwrap().add_error =
            Some((451, json!({"error": "infringing_file", "error_code": 35})));
        let error = start_debrid_reported(
            run_for(dir.path(), base, SharedDebridState::default(), KEY),
            Arc::new(JobSnapshot::default()),
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("refused by the provider"), "{text}");
        assert!(!text.contains(KEY) && !text.contains("http"), "{text}");
    }
}

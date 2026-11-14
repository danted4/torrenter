//! Debrid-Link (debrid-link.com/api/v2). Research: .cursor/plans/debrid-research/debrid-link.md
//!
//! The user pastes a private API key, sent as `Authorization: Bearer` (the
//! OAuth device flow is not implemented). Quirks encoded here:
//! - `seedbox/add` returns at once (no `async` param any more); `name` and
//!   `files` may be empty until metadata arrives, so readiness comes from
//!   polling, never from the add reply.
//! - `seedbox/list` without `ids=` collapses a big torrent into one ZIP entry
//!   (`isZip`), so every list call that reads files passes `ids=`. The only
//!   unfiltered calls are the paged hash lookups in `add`, which read just
//!   `id` and `hashString`.
//! - `status` changed meaning (0/1/2/4/8/100 now, Transmission-style 6 before)
//!   and may combine as flags, so readiness keys on `downloadPercent == 100`
//!   plus a `downloadUrl` on every file, never on `status`.
//! - Seedbox files carry their CDN link inline; `link` re-reads the list for a
//!   fresh `downloadUrl` (there is no unlock step).
//! - Errors are `{success: false, error, error_description}` on 4xx/5xx (and
//!   sometimes 200), so the `error` code decides, not the HTTP status.
//! - `seedbox/cached` is gone and is not used.

use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;
use url::Url;

use super::api::{number, permille, text, Api, Reply};
use super::{Account, Added, DebridError, Remote, RemoteFile, RemoteState, Secret};

const BASE: &str = "https://debrid-link.com/api/v2/";
/// `perPage` ceiling documented for `seedbox/list`.
const PER_PAGE: &str = "100";
/// Hash lookups stop after this many pages (10 000 torrents).
const MAX_PAGES: u64 = 100;
/// `floodDetected` says "retry later (~1h)" without a header; back off well
/// clear of the endpoint instead of retrying hot. [guess: no documented value]
const FLOOD_WAIT: Duration = Duration::from_secs(600);

pub(crate) struct DebridLink {
    api: Api,
    key: Secret,
}

impl DebridLink {
    pub fn new(key: Secret, base: Option<Url>) -> Result<DebridLink, DebridError> {
        let base = base.unwrap_or_else(|| Url::parse(BASE).expect("valid Debrid-Link base"));
        Ok(DebridLink {
            api: Api::new(base)?,
            key,
        })
    }

    fn get(&self, url: Url) -> reqwest::RequestBuilder {
        self.api
            .client()
            .get(url)
            .header("Authorization", format!("Bearer {}", self.key.expose()))
            .header("Accept", "application/json")
    }

    /// `accountType`: 0 free, 1 premium, 2 lifetime; `premiumLeft` in seconds.
    pub async fn account(&self) -> Result<Account, DebridError> {
        let value = envelope(
            self.api
                .send(self.get(self.api.url("account/infos")))
                .await?,
        )?;
        let kind = value.get("accountType").and_then(number).unwrap_or(0.0) as i64;
        let left = value.get("premiumLeft").and_then(number).unwrap_or(0.0);
        let expires = match kind {
            2 => Some("lifetime".to_string()),
            1 if left > 0.0 => Some(
                (Utc::now() + chrono::Duration::seconds(left.min(1e10) as i64))
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
            _ => None,
        };
        Ok(Account {
            premium: kind == 1 || kind == 2,
            expires,
        })
    }

    /// Re-use a torrent already in the seedbox for this hash, otherwise add the
    /// full magnet. Whether a duplicate add returns the old torrent is
    /// undocumented, so the lookup comes first.
    pub async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        if let Some(remote_id) = self.find_by_hash(hash_hex).await? {
            return Ok(Added {
                remote_id,
                added_by_us: false,
            });
        }
        // Every client in the research sends a form; the docs say JSON also works.
        let request = self
            .api
            .client()
            .post(self.api.url("seedbox/add"))
            .header("Authorization", format!("Bearer {}", self.key.expose()))
            .header("Accept", "application/json")
            .form(&[("url", magnet)]);
        let value = match envelope(self.api.send(request).await?) {
            Ok(value) => value,
            // A refusal may mean it is already there (added elsewhere meanwhile).
            Err(DebridError::Service(detail)) => {
                return match self.find_by_hash(hash_hex).await? {
                    Some(remote_id) => Ok(Added {
                        remote_id,
                        added_by_us: false,
                    }),
                    None => Err(DebridError::Service(detail)),
                };
            }
            Err(error) => return Err(error),
        };
        let item = match &value {
            Value::Array(items) => items
                .iter()
                .find(|item| same_hash(item, hash_hex))
                .or_else(|| items.first())
                .cloned()
                .unwrap_or(Value::Null),
            other => other.clone(),
        };
        match item.get("id").and_then(text).filter(|id| !id.is_empty()) {
            Some(remote_id) => Ok(Added {
                remote_id,
                added_by_us: true,
            }),
            None => Err(DebridError::Service(
                "Debrid-Link returned no torrent id".into(),
            )),
        }
    }

    /// Page through `seedbox/list` (no `ids=`) comparing `hashString`
    /// case-insensitively. Only `id`/`hashString` are read here: without
    /// `ids=` a big torrent's files collapse into one ZIP entry.
    async fn find_by_hash(&self, hash_hex: &str) -> Result<Option<String>, DebridError> {
        let mut page: u64 = 0;
        while page < MAX_PAGES {
            let mut url = self.api.url("seedbox/list");
            url.query_pairs_mut()
                .append_pair("page", &page.to_string())
                .append_pair("perPage", PER_PAGE);
            let reply = self.api.send(self.get(url)).await?;
            let pagination = reply.json.get("pagination").cloned();
            let value = envelope(reply)?;
            if let Some(found) = value
                .as_array()
                .and_then(|items| items.iter().find(|item| same_hash(item, hash_hex)))
                .and_then(|item| item.get("id").and_then(text))
            {
                return Ok(Some(found));
            }
            let next = pagination
                .as_ref()
                .and_then(|pagination| pagination.get("next"))
                .and_then(number)
                .unwrap_or(-1.0);
            if next < 0.0 || next as u64 <= page {
                break;
            }
            page = next as u64;
        }
        Ok(None)
    }

    /// The torrent object for one id, always via `seedbox/list?ids=`.
    async fn torrent(&self, remote_id: &str) -> Result<Value, DebridError> {
        let mut url = self.api.url("seedbox/list");
        url.query_pairs_mut().append_pair("ids", remote_id);
        let value = envelope(self.api.send(self.get(url)).await?)?;
        let items = match value {
            Value::Array(items) => items,
            Value::Object(_) => vec![value],
            _ => Vec::new(),
        };
        items
            .into_iter()
            .find(|item| item.get("id").and_then(text).as_deref() == Some(remote_id))
            .ok_or(DebridError::Gone)
    }

    pub async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        let item = self.torrent(remote_id).await?;
        let listed = listed_files(&item);
        let files: Vec<RemoteFile> = listed.iter().map(|file| file.remote.clone()).collect();
        let percent = item.get("downloadPercent").and_then(number).unwrap_or(0.0);
        let done = percent >= 100.0;
        let all_linked = !listed.is_empty()
            && listed
                .iter()
                .all(|file| file.url.is_some() && file.percent.is_none_or(|p| p >= 100.0));
        let state = if let Some(reason) = failure(&item) {
            RemoteState::Failed(reason)
        } else if done && item.get("isZip").and_then(Value::as_bool) == Some(true) {
            RemoteState::Failed(
                "Debrid-Link listed the torrent as one ZIP; per-file links are unavailable".into(),
            )
        } else if done && all_linked {
            RemoteState::Ready
        } else if done {
            // Finished, links not listed yet.
            RemoteState::Fetching { permille: 1000 }
        } else {
            RemoteState::Fetching {
                permille: permille(percent / 100.0),
            }
        };
        Ok(Remote { state, files })
    }

    /// The file's current inline `downloadUrl` (re-read, so a refresh after
    /// an expired link gets whatever the seedbox lists now).
    pub async fn link(&self, remote_id: &str, file: &RemoteFile) -> Result<String, DebridError> {
        let item = self.torrent(remote_id).await?;
        let listed = listed_files(&item);
        let entry = listed
            .iter()
            .find(|entry| entry.remote.id == file.id)
            .or_else(|| listed.iter().find(|entry| entry.remote.path == file.path))
            .ok_or_else(|| DebridError::Service("Debrid-Link no longer lists that file".into()))?;
        entry
            .url
            .clone()
            .ok_or_else(|| DebridError::Service("Debrid-Link returned no download link".into()))
    }

    /// `DELETE seedbox/{id}/remove`. An id the seedbox no longer knows is fine.
    pub async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        let mut url = self.api.url("seedbox/");
        url.path_segments_mut()
            .map_err(|_| DebridError::Service("Debrid-Link base URL is unusable".into()))?
            .pop_if_empty()
            .push(remote_id)
            .push("remove");
        let request = self
            .api
            .client()
            .delete(url)
            .header("Authorization", format!("Bearer {}", self.key.expose()))
            .header("Accept", "application/json");
        match envelope(self.api.send(request).await?) {
            Ok(_) | Err(DebridError::Gone) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

struct ListedFile {
    remote: RemoteFile,
    url: Option<String>,
    percent: Option<f64>,
}

fn listed_files(item: &Value) -> Vec<ListedFile> {
    item.get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    let name = file.get("name").and_then(text).filter(|n| !n.is_empty())?;
                    Some(ListedFile {
                        remote: RemoteFile {
                            id: file.get("id").and_then(text).filter(|id| !id.is_empty())?,
                            path: name,
                            size: file.get("size").and_then(number).unwrap_or(0.0).max(0.0) as u64,
                        },
                        url: file
                            .get("downloadUrl")
                            .and_then(Value::as_str)
                            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                            .map(str::to_string),
                        percent: file.get("downloadPercent").and_then(number),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Undocumented `error` / `errorString` torrent fields some clients read.
/// [guess: their exact meaning is not documented]
fn failure(item: &Value) -> Option<String> {
    let message = item
        .get("errorString")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let code = item.get("error").and_then(number).unwrap_or(0.0);
    match (message, code != 0.0) {
        (Some(message), _) => Some(format!("Debrid-Link reports the torrent failed: {message}")),
        (None, true) => Some(format!("Debrid-Link reports torrent error {code}")),
        (None, false) => None,
    }
}

fn same_hash(item: &Value, hash_hex: &str) -> bool {
    item.get("hashString")
        .and_then(Value::as_str)
        .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex))
}

/// Unwrap `{success, value}`; map `{success: false, error, error_description}`
/// by its code whatever the HTTP status.
fn envelope(reply: Reply) -> Result<Value, DebridError> {
    let code = reply
        .json
        .get("error")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .map(str::to_string);
    let success = reply.json.get("success").and_then(Value::as_bool);
    if code.is_none() && success == Some(true) && reply.status.is_success() {
        return Ok(reply.json.get("value").cloned().unwrap_or(Value::Null));
    }
    let description = reply
        .json
        .get("error_description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let detail = match (code.as_deref(), description) {
        (Some(code), Some(description)) => format!("Debrid-Link: {description} ({code})"),
        (Some(code), None) => format!("Debrid-Link: {code}"),
        (None, Some(description)) => format!("Debrid-Link: {description}"),
        (None, None) => format!("Debrid-Link answered HTTP {}", reply.status.as_u16()),
    };
    Err(match code.as_deref() {
        Some("badToken" | "hidedToken" | "badSign" | "accountLocked") => {
            DebridError::BadKey(detail)
        }
        Some("notDebrid" | "unverifiedEmail" | "freeServerOverload" | "notFreeHost") => {
            DebridError::NotPremium(detail)
        }
        Some("floodDetected") => DebridError::RateLimited(Some(FLOOD_WAIT)),
        // Ids come from the seedbox itself, so "invalid id" means it is gone.
        Some("badId") => DebridError::Gone,
        Some("infringingFile") => DebridError::Blocked(detail),
        _ if reply.status.as_u16() == 401 => DebridError::BadKey(detail),
        _ => DebridError::Service(detail),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::debrid::fake::{serve, FakeRequest, FakeResponse, FakeServer};

    pub const KEY: &str = "dl-test-key-7f3a";
    pub const HASH: &str = "c01d0b3954bb3b57a550ebef2cc7c10b20cce3ae";

    pub struct State {
        pub torrents: BTreeMap<String, Torrent>,
        pub next_id: u32,
        /// `list?ids=` polls until a new torrent reaches 100 (0 = cached).
        pub fetch_polls: u32,
        pub account_type: u8,
        pub premium_left: u64,
        /// HTTP status sent with `badToken` (the docs never say; 401 or 400).
        pub bad_token_status: u16,
        /// Without `ids=`, torrents with at least this many files collapse
        /// into one ZIP entry (`isZip`).
        pub zip_threshold: usize,
        /// `perPage` the fake actually honours (the API clamps to 20..100).
        pub page_cap: usize,
        /// Forced `status` value (e.g. legacy 6) instead of 4 / 100.
        pub status_override: Option<u32>,
        /// Leave `downloadUrl` empty even at 100 %.
        pub hide_urls: bool,
        /// Answer the next API call with this error once.
        pub inject: Option<(u16, &'static str)>,
        /// Answer the first N file downloads with 403 (expired link).
        pub expire_links: u32,
        pub removed: Vec<String>,
    }

    impl Default for State {
        fn default() -> Self {
            State {
                torrents: BTreeMap::new(),
                next_id: 0,
                fetch_polls: 0,
                account_type: 1,
                premium_left: 3_628_800,
                bad_token_status: 401,
                zip_threshold: 2,
                page_cap: 100,
                status_override: None,
                hide_urls: false,
                inject: None,
                expire_links: 0,
                removed: Vec::new(),
            }
        }
    }

    pub struct Torrent {
        pub hash: String,
        pub name: String,
        pub polls: u32,
        pub files: Vec<(String, Vec<u8>)>,
        pub failed: bool,
    }

    pub fn sample_files() -> Vec<(String, Vec<u8>)> {
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

    fn ok(value: Value) -> FakeResponse {
        FakeResponse::json(200, json!({"success": true, "value": value}))
    }

    fn err(status: u16, code: &str, description: &str) -> FakeResponse {
        FakeResponse::json(
            status,
            json!({"success": false, "error": code, "error_description": description}),
        )
    }

    fn percent(torrent: &Torrent, fetch_polls: u32) -> u32 {
        (torrent.polls * 100)
            .checked_div(fetch_polls)
            .map_or(100, |percent| percent.min(100))
    }

    fn torrent_json(
        state: &State,
        id: &str,
        torrent: &Torrent,
        origin: &str,
        filtered: bool,
    ) -> Value {
        let percent = percent(torrent, state.fetch_polls);
        let has_meta = torrent.polls > 0 || percent == 100;
        let mut files: Vec<Value> = if has_meta {
            torrent
                .files
                .iter()
                .enumerate()
                .map(|(index, (name, body))| {
                    let url = if percent == 100 && !state.hide_urls {
                        format!("{origin}/dl/{id}/{}", index + 1)
                    } else {
                        String::new()
                    };
                    json!({"id": format!("{id}-{}", index + 1), "name": name, "size": body.len(),
                           "downloadUrl": url, "downloadPercent": percent})
                })
                .collect()
        } else {
            Vec::new()
        };
        let total: usize = torrent.files.iter().map(|(_, body)| body.len()).sum();
        let is_zip = !filtered && files.len() >= state.zip_threshold;
        if is_zip {
            files = vec![
                json!({"id": format!("{id}-zip"), "name": format!("{}.zip", torrent.name),
                                "size": total, "downloadUrl": format!("{origin}/dl/{id}/zip"),
                                "downloadPercent": percent}),
            ];
        }
        let status = state.status_override.unwrap_or(if torrent.failed {
            0
        } else if percent == 100 {
            100
        } else {
            4
        });
        let mut value = json!({
            "id": id, "name": if has_meta { torrent.name.as_str() } else { "" },
            "created": 1794669324, "hashString": torrent.hash, "uploadRatio": 0,
            "wait": false, "peersConnected": 3, "status": status,
            "totalSize": if has_meta { total } else { 0 }, "downloadPercent": percent,
            "downloadSpeed": 0, "uploadSpeed": 0, "isZip": is_zip, "srvMaint": false,
            "files": files, "trackers": [], "zip": {},
        });
        if torrent.failed {
            value["error"] = json!(1);
            value["errorString"] = json!("Tracker gave HTTP 404");
        }
        value
    }

    fn requested_ids(raw: &str) -> Vec<String> {
        if raw.trim_start().starts_with('[') {
            serde_json::from_str::<Vec<Value>>(raw)
                .unwrap_or_default()
                .iter()
                .filter_map(text)
                .collect()
        } else {
            raw.split(',').map(|id| id.trim().to_string()).collect()
        }
    }

    /// A Debrid-Link fake that enforces Bearer auth, accepts form or JSON for
    /// `seedbox/add` (ignoring unknown params like `async`), reproduces the
    /// `isZip` collapse when `ids=` is missing, and paginates the list.
    pub async fn debridlink_fake(state: Arc<Mutex<State>>) -> (FakeServer, Url) {
        let origin_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let origin_for_handler = Arc::clone(&origin_slot);
        let server = serve(move |request: &FakeRequest| {
            let mut state = state.lock().unwrap();
            let origin = origin_for_handler.lock().unwrap().clone();
            let path = request.path.as_str();
            if let Some(rest) = path.strip_prefix("/dl/") {
                let mut parts = rest.split('/');
                let id = parts.next().unwrap_or_default().to_string();
                let index: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                if state.expire_links > 0 {
                    state.expire_links -= 1;
                    return err(403, "badFileUrl", "Link expired.");
                }
                return match state
                    .torrents
                    .get(&id)
                    .and_then(|t| t.files.get(index.wrapping_sub(1)))
                {
                    Some((_, body)) => FakeResponse::file(request, body, false),
                    None => FakeResponse::empty(404),
                };
            }
            let Some(route) = path.strip_prefix("/api/v2/") else {
                return err(404, "unknowR", "Unknown API endpoint.");
            };
            if request.bearer() != Some(KEY) {
                return err(state.bad_token_status, "badToken", "Invalid token.");
            }
            if let Some((status, code)) = state.inject.take() {
                return err(status, code, "Injected error.");
            }
            match (request.method.as_str(), route) {
                ("GET", "account/infos") => ok(json!({
                    "username": "Amy", "email": "p**d@deb*******k", "emailVerified": true,
                    "accountType": state.account_type,
                    "premiumLeft": if state.account_type == 0 { 0 } else { state.premium_left },
                    "pts": 500, "registerDate": "2009-06-20", "serverDetected": false,
                })),
                ("POST", "seedbox/add") => {
                    let url = if request.is_form() {
                        request.form_value("url")
                    } else if request.is_json() {
                        request
                            .json()
                            .and_then(|body| body.get("url").and_then(text))
                    } else {
                        None
                    };
                    let Some(url) = url.filter(|url| !url.is_empty()) else {
                        return err(400, "badArguments", "Missing url.");
                    };
                    let hash = if let Some(rest) = url.split("btih:").nth(1) {
                        rest.split('&').next().unwrap_or_default().to_ascii_lowercase()
                    } else if url.len() == 40 && url.chars().all(|c| c.is_ascii_hexdigit()) {
                        // A bare hash is only accepted when cached; this fake has no cache.
                        return err(422, "notAddTorrent", "The torrent could not be added.");
                    } else {
                        return err(400, "badArguments", "Invalid url.");
                    };
                    let name = url
                        .split("dn=")
                        .nth(1)
                        .and_then(|rest| rest.split('&').next())
                        .unwrap_or("torrent")
                        .to_string();
                    // Undocumented: re-adding a hash answers with the existing torrent.
                    if let Some((id, torrent)) = state.torrents.iter().find(|(_, t)| t.hash == hash) {
                        return ok(torrent_json(&state, id, torrent, &origin, true));
                    }
                    state.next_id += 1;
                    let id = format!("2115ca3cf4356d2451{}", state.next_id);
                    state.torrents.insert(
                        id.clone(),
                        Torrent { hash, name, polls: 0, files: sample_files(), failed: false },
                    );
                    let torrent = &state.torrents[&id];
                    ok(torrent_json(&state, &id, torrent, &origin, true))
                }
                ("GET", "seedbox/list") => {
                    let mut items: Vec<Value> = Vec::new();
                    if let Some(raw) = request.query("ids") {
                        let wanted = requested_ids(raw);
                        if wanted.len() > 100 {
                            return err(400, "badArguments", "Too many ids.");
                        }
                        for id in wanted {
                            if let Some(torrent) = state.torrents.get_mut(&id) {
                                torrent.polls += 1;
                            }
                            if let Some(torrent) = state.torrents.get(&id) {
                                items.push(torrent_json(&state, &id, torrent, &origin, true));
                            }
                        }
                        return FakeResponse::json(200, json!({"success": true, "value": items,
                            "pagination": {"page": 0, "pages": 1, "next": -1, "previous": -1}}));
                    }
                    let per_page = request
                        .query("perPage")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(20)
                        .clamp(20, 100)
                        .min(state.page_cap)
                        .max(1);
                    let page = request.query("page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                    let all: Vec<Value> = state
                        .torrents
                        .iter()
                        .map(|(id, torrent)| torrent_json(&state, id, torrent, &origin, false))
                        .collect();
                    let pages = all.len().div_ceil(per_page).max(1);
                    items = all.into_iter().skip(page * per_page).take(per_page).collect();
                    let next = if page + 1 < pages { (page + 1) as i64 } else { -1 };
                    let previous = if page > 0 { page as i64 - 1 } else { -1 };
                    FakeResponse::json(200, json!({"success": true, "value": items,
                        "pagination": {"page": page, "pages": pages, "next": next, "previous": previous}}))
                }
                ("DELETE", route) if route.starts_with("seedbox/") && route.ends_with("/remove") => {
                    let raw = &route["seedbox/".len()..route.len() - "/remove".len()];
                    let mut removed = Vec::new();
                    for id in requested_ids(raw) {
                        if state.torrents.remove(&id).is_some() {
                            state.removed.push(id.clone());
                            removed.push(id);
                        }
                    }
                    if removed.is_empty() {
                        return err(400, "badId", "The provided ID(s) are invalid.");
                    }
                    ok(json!(removed))
                }
                _ => err(404, "unknowR", "Unknown API endpoint."),
            }
        })
        .await;
        *origin_slot.lock().unwrap() = server.origin.clone();
        let base = Url::parse(&format!("{}/api/v2/", server.origin)).unwrap();
        (server, base)
    }

    pub fn magnet() -> String {
        format!("magnet:?xt=urn:btih:{HASH}&dn=Album")
    }

    fn client(base: &Url, key: &str) -> DebridLink {
        DebridLink::new(Secret::new(key).unwrap(), Some(base.clone())).unwrap()
    }

    fn insert(state: &Arc<Mutex<State>>, id: &str, hash: &str) {
        state.lock().unwrap().torrents.insert(
            id.to_string(),
            Torrent {
                hash: hash.to_string(),
                name: "Other".into(),
                polls: 0,
                files: sample_files(),
                failed: false,
            },
        );
    }

    #[tokio::test]
    async fn account_reads_type_and_premium_left() {
        let state = Arc::new(Mutex::new(State::default()));
        let (server, base) = debridlink_fake(Arc::clone(&state)).await;
        let account = client(&base, KEY).account().await.unwrap();
        assert!(account.premium);
        let expires = account.expires.expect("premium has an expiry");
        let at = chrono::DateTime::parse_from_rfc3339(&expires).unwrap();
        let days = (at.with_timezone(&Utc) - Utc::now()).num_days();
        assert!((41..=42).contains(&days), "{expires}");

        state.lock().unwrap().account_type = 2;
        let lifetime = client(&base, KEY).account().await.unwrap();
        assert!(lifetime.premium);
        assert_eq!(lifetime.expires.as_deref(), Some("lifetime"));

        state.lock().unwrap().account_type = 0;
        let free = client(&base, KEY).account().await.unwrap();
        assert!(!free.premium);
        assert_eq!(free.expires, None);

        for status in [401, 400] {
            state.lock().unwrap().bad_token_status = status;
            let error = client(&base, "wrong-key-xyz").account().await.unwrap_err();
            assert!(matches!(error, DebridError::BadKey(_)), "{error:?}");
            assert!(error.to_string().contains("badToken"));
            assert!(!error.to_string().contains("wrong-key-xyz"));
        }
        let auth = server.requests()[0].clone();
        assert_eq!(auth.bearer(), Some(KEY));
        assert!(auth.query.is_empty(), "the key never goes in the query");
    }

    #[tokio::test]
    async fn add_poll_link_delete_flow() {
        let state = Arc::new(Mutex::new(State {
            fetch_polls: 2,
            ..State::default()
        }));
        let (server, base) = debridlink_fake(Arc::clone(&state)).await;
        let debrid = client(&base, KEY);
        let added = debrid.add(&magnet(), HASH).await.unwrap();
        assert!(added.added_by_us);
        let add = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("seedbox/add"))
            .unwrap();
        assert_eq!(add.method, "POST");
        assert!(add.is_form());
        assert_eq!(add.form_value("url"), Some(magnet()));

        let first = debrid.status(&added.remote_id).await.unwrap();
        assert_eq!(first.state, RemoteState::Fetching { permille: 500 });
        assert!(first.files.iter().all(|file| !file.path.is_empty()));
        let ready = debrid.status(&added.remote_id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        assert_eq!(ready.files.len(), 2, "never the collapsed ZIP");
        assert_eq!(ready.files[0].id, format!("{}-1", added.remote_id));
        assert_eq!(ready.files[0].path, "Album/CD1/01.bin");
        assert_eq!(ready.files[1].size, 1500);

        for (index, file) in ready.files.iter().enumerate() {
            let link = debrid.link(&added.remote_id, file).await.unwrap();
            assert!(
                link.ends_with(&format!("/dl/{}/{}", added.remote_id, index + 1)),
                "{link}"
            );
        }

        // Re-adding the same hash re-uses it and never claims ownership.
        let again = debrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(again.remote_id, added.remote_id);
        assert!(!again.added_by_us);
        assert_eq!(server.count("seedbox/add"), 1);

        debrid.delete(&added.remote_id).await.unwrap();
        assert_eq!(
            state.lock().unwrap().removed,
            std::slice::from_ref(&added.remote_id)
        );
        let remove = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("/remove"))
            .unwrap();
        assert_eq!(remove.method, "DELETE");
        assert_eq!(
            remove.path,
            format!("/api/v2/seedbox/{}/remove", added.remote_id)
        );
        assert_eq!(
            debrid.status(&added.remote_id).await.unwrap_err(),
            DebridError::Gone
        );
        // Deleting twice (badId) is fine.
        debrid.delete(&added.remote_id).await.unwrap();
        assert!(server.requests().iter().all(|r| r.bearer() == Some(KEY)));
    }

    #[tokio::test]
    async fn every_file_read_passes_ids_so_the_zip_collapse_never_shows() {
        let state = Arc::new(Mutex::new(State {
            page_cap: 1,
            ..State::default()
        }));
        let (server, base) = debridlink_fake(Arc::clone(&state)).await;
        // The trap is real: an unfiltered list collapses the files into one ZIP.
        insert(&state, "aaa1", &"1".repeat(40));
        insert(&state, "bbb2", &HASH.to_ascii_uppercase());
        let raw: Value = reqwest::Client::new()
            .get(format!("{}seedbox/list?page=1&perPage=100", base))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(raw["value"][0]["isZip"], json!(true));
        assert_eq!(raw["value"][0]["files"].as_array().unwrap().len(), 1);
        server.log.lock().unwrap().clear();

        // The hash sits on page 2 (one per page); matching ignores case.
        let debrid = client(&base, KEY);
        let added = debrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(added.remote_id, "bbb2");
        assert!(!added.added_by_us);
        let lookups: Vec<_> = server.requests();
        assert_eq!(lookups.len(), 2, "two pages, no add");
        assert!(lookups
            .iter()
            .all(|r| r.query("perPage") == Some("100") && r.query("ids").is_none()));
        server.log.lock().unwrap().clear();

        let ready = debrid.status("bbb2").await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        assert_eq!(ready.files.len(), 2);
        for file in &ready.files {
            debrid.link("bbb2", file).await.unwrap();
        }
        let lists: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path.ends_with("seedbox/list"))
            .collect();
        assert_eq!(lists.len(), 3);
        assert!(lists.iter().all(|r| r.query("ids") == Some("bbb2")));
    }

    #[tokio::test]
    async fn readiness_keys_on_percent_and_links_not_status() {
        let state = Arc::new(Mutex::new(State {
            fetch_polls: 2,
            // Legacy "finished" 6 (or verifying|downloading flags) all along.
            status_override: Some(6),
            ..State::default()
        }));
        let (_server, base) = debridlink_fake(Arc::clone(&state)).await;
        let debrid = client(&base, KEY);
        let added = debrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(
            debrid.status(&added.remote_id).await.unwrap().state,
            RemoteState::Fetching { permille: 500 }
        );
        state.lock().unwrap().hide_urls = true;
        assert_eq!(
            debrid.status(&added.remote_id).await.unwrap().state,
            RemoteState::Fetching { permille: 1000 },
            "100 % without links is not ready"
        );
        state.lock().unwrap().hide_urls = false;
        assert_eq!(
            debrid.status(&added.remote_id).await.unwrap().state,
            RemoteState::Ready
        );
        state.lock().unwrap().status_override = Some(4);
        assert_eq!(
            debrid.status(&added.remote_id).await.unwrap().state,
            RemoteState::Ready
        );
        state
            .lock()
            .unwrap()
            .torrents
            .get_mut(&added.remote_id)
            .unwrap()
            .failed = true;
        assert!(matches!(
            debrid.status(&added.remote_id).await.unwrap().state,
            RemoteState::Failed(_)
        ));
    }

    #[tokio::test]
    async fn errors_map_by_code() {
        let state = Arc::new(Mutex::new(State::default()));
        let (_server, base) = debridlink_fake(Arc::clone(&state)).await;
        let debrid = client(&base, KEY);
        type Check = fn(&DebridError) -> bool;
        let cases: [(u16, &'static str, Check); 7] = [
            (400, "floodDetected", |e| {
                matches!(e, DebridError::RateLimited(Some(_)))
            }),
            (429, "floodDetected", |e| {
                matches!(e, DebridError::RateLimited(_))
            }),
            (403, "notDebrid", |e| {
                matches!(e, DebridError::NotPremium(_))
            }),
            (451, "infringingFile", |e| {
                matches!(e, DebridError::Blocked(_))
            }),
            (
                400,
                "maxTorrent",
                |e| matches!(e, DebridError::Service(d) if d.contains("maxTorrent")),
            ),
            (500, "internalError", |e| {
                matches!(e, DebridError::Service(_))
            }),
            (401, "hidedToken", |e| matches!(e, DebridError::BadKey(_))),
        ];
        for (status, code, check) in cases {
            state.lock().unwrap().inject = Some((status, code));
            let error = debrid.add(&magnet(), HASH).await.unwrap_err();
            assert!(check(&error), "{code}: {error:?}");
            assert!(!error.to_string().contains(KEY));
        }
        // An unknown id reads as gone; a bad route stays a service error.
        assert_eq!(debrid.status("nope").await.unwrap_err(), DebridError::Gone);
        let file = RemoteFile {
            id: "nope-1".into(),
            path: "x".into(),
            size: 1,
        };
        assert_eq!(
            debrid.link("nope", &file).await.unwrap_err(),
            DebridError::Gone
        );
        state.lock().unwrap().inject = Some((400, "badId"));
        assert_eq!(debrid.status("nope").await.unwrap_err(), DebridError::Gone);
        debrid.delete("nope").await.unwrap();
        let wrong = client(&Url::parse(&format!("{base}nowhere/")).unwrap(), KEY);
        let error = wrong.account().await.unwrap_err();
        assert!(
            matches!(&error, DebridError::Service(d) if d.contains("unknowR")),
            "{error:?}"
        );
        // A bare uncached hash is refused, not mistaken for a re-use.
        let error = debrid.add(HASH, HASH).await.unwrap_err();
        assert!(
            matches!(&error, DebridError::Service(d) if d.contains("notAddTorrent")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn fake_accepts_json_add_and_ignores_unknown_params() {
        let state = Arc::new(Mutex::new(State::default()));
        let (_server, base) = debridlink_fake(Arc::clone(&state)).await;
        let reply: Value = reqwest::Client::new()
            .post(format!("{base}seedbox/add"))
            .bearer_auth(KEY)
            .json(&json!({"url": magnet(), "async": true, "ip": "1.2.3.4"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply["value"]["hashString"], json!(HASH));
        // Cached (fetch_polls 0): ready at once, files listed.
        let id = reply["value"]["id"].as_str().unwrap().to_string();
        let remote = client(&base, KEY).status(&id).await.unwrap();
        assert_eq!(remote.state, RemoteState::Ready);
    }

    #[tokio::test]
    async fn runner_downloads_every_file_and_removes_the_torrent() {
        use crate::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
        use crate::debrid::Service;
        use crate::progress::JobSnapshot;

        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 1,
            expire_links: 1,
            ..State::default()
        }));
        let (server, base) = debridlink_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        let run = DebridRun {
            service: Service::DebridLink,
            key: Secret::new(KEY).unwrap(),
            magnet: magnet(),
            dest: dir.path().to_path_buf(),
            part_count: 4,
            remove_after: true,
            state: Arc::clone(&state),
            base: Some(base.clone()),
        };
        start_debrid_reported(run, Arc::clone(&snapshot))
            .await
            .unwrap();
        assert!(snapshot.is_done(), "status {}", snapshot.status());
        let job = dir.path().join("Album");
        for (name, body) in sample_files() {
            let relative = name.strip_prefix("Album/").unwrap();
            assert_eq!(std::fs::read(job.join(relative)).unwrap(), body, "{name}");
        }
        let remote_id = state.lock().unwrap().remote_id.clone().unwrap();
        assert!(state.lock().unwrap().added_by_us);
        assert!(fake.lock().unwrap().torrents.is_empty());
        assert_eq!(fake.lock().unwrap().removed, [remote_id]);
        assert_eq!(
            fake.lock().unwrap().expire_links,
            0,
            "expired link refreshed"
        );
        for request in server.requests() {
            if request.path.ends_with("seedbox/list") {
                assert!(request.query("ids").is_some() || request.query("page").is_some());
            }
            assert!(!request.path.contains(KEY));
            assert!(request.query.iter().all(|(_, value)| !value.contains(KEY)));
        }

        // A bad key fails with text that carries neither the key nor a link.
        let snapshot = Arc::new(JobSnapshot::default());
        let run = DebridRun {
            service: Service::DebridLink,
            key: Secret::new("wrong-dl-key-991").unwrap(),
            magnet: magnet(),
            dest: dir.path().to_path_buf(),
            part_count: 4,
            remove_after: true,
            state: SharedDebridState::default(),
            base: Some(base),
        };
        let error = start_debrid_reported(run, snapshot).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("API key rejected"), "{text}");
        assert!(
            !text.contains("wrong-dl-key-991") && !text.contains("http"),
            "{text}"
        );
    }
}

//! AllDebrid (api.alldebrid.com). Research: .cursor/plans/debrid-research/alldebrid.md
//!
//! Quirks encoded here: application errors (bad key, unknown id, …) arrive as
//! HTTP 200 with `{"status":"error","error":{code,message}}` (only unknown
//! routes are 404, rate limits 429/503), and `magnet/upload` / `magnet/files`
//! put per-item errors inside a success envelope. `v4.1/magnet/status` with an
//! `id` answers `data.magnets` as an object, not an array (arrays and maps keyed
//! by id are accepted too); `statusCode` 0–3 is fetching, 4 ready, anything
//! else failed. Files come from `magnet/files` as a tree of `{n, e}` folders
//! and `{n, s, l}` files; `l` is an `alldebrid.com/f/…` page, not a download,
//! so every file goes through `link/unlock`, which may answer `delayed`
//! (then `link/delayed` until status 2). A magnet can read Ready a moment
//! before every `l` is filled, which is reported as still fetching. Ids flip
//! between numbers and strings. `magnet/instant` is gone: there is no cache
//! probe. The key only travels as `Authorization: Bearer`.
//!
//! Re-use: before uploading, the magnet list (`v4.1/magnet/status` without an
//! id) is searched for the hash (`hash` is undocumented there but widely
//! returned); failed entries are skipped. An upload that answers an id that was
//! already on that list (AllDebrid may dedupe) is re-use too, so "remove after"
//! never deletes a copy the user had before.
//!
//! `RemoteFile::id` is the file's index in the flattened tree (never the `l`
//! link: ids are persisted). `link()` lists the files again and finds the entry
//! by path, then by index.

use std::time::Duration;

use reqwest::RequestBuilder;
use serde_json::Value;
use url::Url;

use super::api::{number, permille, text, Api, Reply};
use super::{redact, Account, Added, DebridError, Remote, RemoteFile, RemoteState, Secret};

const BASE: &str = "https://api.alldebrid.com/";
/// Optional since 2025; harmless and names the app.
const AGENT: &str = "torrenter";
/// `link/delayed`: "poll every 5 seconds or more".
const DELAYED_POLL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(5)
};
/// Give up on a delayed link after about ten minutes.
const DELAYED_MAX_POLLS: u32 = 120;
/// Deepest folder nesting followed in a file tree.
const TREE_MAX_DEPTH: usize = 64;

pub(crate) struct AllDebrid {
    api: Api,
    key: Secret,
}

/// One file of a magnet's tree.
struct TreeFile {
    path: String,
    size: u64,
    /// `alldebrid.com/f/…`; empty while AllDebrid has not published it.
    link: String,
}

impl AllDebrid {
    pub fn new(key: Secret, base: Option<Url>) -> Result<AllDebrid, DebridError> {
        let base = base.unwrap_or_else(|| Url::parse(BASE).expect("valid AllDebrid base"));
        Ok(AllDebrid {
            api: Api::new(base)?,
            key,
        })
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.key.expose())
    }

    /// POST with a form body: the documented default since 2025.
    fn post(&self, path: &str, form: &[(&str, &str)]) -> RequestBuilder {
        self.api
            .client()
            .post(self.api.url(path))
            .header("Authorization", self.bearer())
            .query(&[("agent", AGENT)])
            .form(form)
    }

    async fn call(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, DebridError> {
        envelope(self.api.send(self.post(path, form)).await?)
    }

    pub async fn account(&self) -> Result<Account, DebridError> {
        let request = self
            .api
            .client()
            .get(self.api.url("v4/user"))
            .header("Authorization", self.bearer())
            .query(&[("agent", AGENT)]);
        let data = envelope(self.api.send(request).await?)?;
        let user = data.get("user").unwrap_or(&Value::Null);
        let premium = match user.get("isPremium") {
            Some(Value::Bool(premium)) => *premium,
            Some(other) => number(other).is_some_and(|value| value > 0.0),
            None => false,
        };
        // `0` when not premium, else a unix time (sometimes as a string).
        let expires = user
            .get("premiumUntil")
            .and_then(number)
            .filter(|seconds| *seconds > 0.0)
            .and_then(|seconds| iso_time(seconds as i64));
        Ok(Account { premium, expires })
    }

    pub async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        let listed = self.list().await?;
        let existing = listed.iter().find(|item| {
            item.get("hash")
                .and_then(Value::as_str)
                .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex))
                && !status_code(item).is_some_and(|code| code > 4)
        });
        if let Some(remote_id) = existing.and_then(|item| item.get("id").and_then(text)) {
            return Ok(Added {
                remote_id,
                added_by_us: false,
            });
        }
        let data = self
            .call("v4/magnet/upload", &[("magnets[]", magnet)])
            .await?;
        let item = items(data.get("magnets").unwrap_or(&Value::Null))
            .into_iter()
            .next()
            .ok_or_else(|| DebridError::Service("AllDebrid returned no magnet".into()))?;
        if let Some(error) = item.get("error") {
            return Err(api_error(error));
        }
        let remote_id = item
            .get("id")
            .and_then(text)
            .ok_or_else(|| DebridError::Service("AllDebrid returned no magnet id".into()))?;
        let known = listed
            .iter()
            .any(|item| item.get("id").and_then(text).as_deref() == Some(remote_id.as_str()));
        Ok(Added {
            remote_id,
            added_by_us: !known,
        })
    }

    /// Every magnet on the account (list form of `magnet/status`).
    async fn list(&self) -> Result<Vec<Value>, DebridError> {
        let data = self.call("v4.1/magnet/status", &[]).await?;
        Ok(items(data.get("magnets").unwrap_or(&Value::Null))
            .into_iter()
            .cloned()
            .collect())
    }

    pub async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        let data = self
            .call("v4.1/magnet/status", &[("id", remote_id)])
            .await?;
        let magnet = pick(data.get("magnets").unwrap_or(&Value::Null), remote_id)
            .ok_or(DebridError::Gone)?;
        if let Some(error) = magnet.get("error") {
            return Err(api_error(error));
        }
        let code = status_code(magnet);
        let state = match code {
            Some(code @ 0..=3) => RemoteState::Fetching {
                permille: progress(magnet, code),
            },
            Some(4) => {
                let tree = self.files(remote_id).await?;
                let files = remote_files(&tree);
                // Ready a moment before the links appear: not downloadable yet.
                let state = if tree.is_empty() || tree.iter().any(|file| file.link.is_empty()) {
                    RemoteState::Fetching { permille: 1000 }
                } else {
                    RemoteState::Ready
                };
                return Ok(Remote { state, files });
            }
            other => {
                let reason = magnet
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|reason| redact(reason, None))
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or_else(|| "unknown status".into());
                let code = other.map_or_else(|| "none".to_string(), |code| code.to_string());
                RemoteState::Failed(format!(
                    "AllDebrid could not fetch the torrent: {reason} (status {code})"
                ))
            }
        };
        Ok(Remote {
            state,
            files: Vec::new(),
        })
    }

    async fn files(&self, remote_id: &str) -> Result<Vec<TreeFile>, DebridError> {
        let data = self.call("v4/magnet/files", &[("id[]", remote_id)]).await?;
        let entry = pick(data.get("magnets").unwrap_or(&Value::Null), remote_id)
            .ok_or(DebridError::Gone)?;
        if let Some(error) = entry.get("error") {
            return Err(api_error(error));
        }
        let mut out = Vec::new();
        if let Some(nodes) = entry.get("files").and_then(Value::as_array) {
            flatten(nodes, "", 0, &mut out);
        }
        Ok(out)
    }

    /// Unlock the file's `/f/` link into a direct download link, waiting out
    /// a `delayed` generation.
    pub async fn link(&self, remote_id: &str, file: &RemoteFile) -> Result<String, DebridError> {
        let tree = self.files(remote_id).await?;
        let index: Option<usize> = file.id.parse().ok();
        let entry = tree
            .iter()
            .find(|entry| entry.path == file.path)
            .or_else(|| index.and_then(|index| tree.get(index)))
            .ok_or_else(|| DebridError::Service("AllDebrid no longer lists this file".into()))?;
        if entry.link.is_empty() {
            return Err(DebridError::Service(
                "AllDebrid has not published a link for this file yet".into(),
            ));
        }
        let data = self
            .call("v4/link/unlock", &[("link", entry.link.as_str())])
            .await?;
        if let Some(link) = direct_link(&data) {
            return Ok(link);
        }
        let delayed = data
            .get("delayed")
            .and_then(text)
            .filter(|id| !id.is_empty() && id != "0")
            .ok_or_else(|| DebridError::Service("AllDebrid returned no download link".into()))?;
        for _ in 0..DELAYED_MAX_POLLS {
            tokio::time::sleep(DELAYED_POLL).await;
            let data = self
                .call("v4/link/delayed", &[("id", delayed.as_str())])
                .await?;
            match data
                .get("status")
                .and_then(number)
                .map(|value| value as i64)
            {
                Some(2) => {
                    return direct_link(&data).ok_or_else(|| {
                        DebridError::Service("AllDebrid returned no download link".into())
                    })
                }
                Some(1) | None => {}
                Some(_) => {
                    return Err(DebridError::Service(
                        "AllDebrid could not generate the download link".into(),
                    ))
                }
            }
        }
        Err(DebridError::Service(
            "AllDebrid took too long to generate the download link".into(),
        ))
    }

    pub async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        match self.call("v4/magnet/delete", &[("id", remote_id)]).await {
            Ok(_) | Err(DebridError::Gone) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Unwrap `{status, data}` / `{status, error}`. Application errors come with
/// HTTP 200, so the body decides; anything without a `status` failed.
fn envelope(reply: Reply) -> Result<Value, DebridError> {
    let status = reply.json.get("status").and_then(Value::as_str);
    if status == Some("success") && reply.status.is_success() {
        return Ok(reply.json.get("data").cloned().unwrap_or(Value::Null));
    }
    if let Some(error) = reply.json.get("error").filter(|error| error.is_object()) {
        return Err(api_error(error));
    }
    let http = reply.status.as_u16();
    Err(match http {
        401 | 403 => DebridError::BadKey(format!("AllDebrid answered HTTP {http}")),
        _ => DebridError::Service(format!("AllDebrid answered HTTP {http} without a result")),
    })
}

/// Map `{code, message}`. Codes decide (messages vary by endpoint).
fn api_error(error: &Value) -> DebridError {
    let code = error.get("code").and_then(text).unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(|message| redact(message, None))
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "no detail".into());
    let detail = if code.is_empty() {
        message
    } else {
        format!("{message} ({code})")
    };
    match code.as_str() {
        code if code.starts_with("AUTH_") || code == "ACCOUNT_INVALID" => {
            DebridError::BadKey(detail)
        }
        code if code.ends_with("_PREMIUM") || code == "FREE_TRIAL_LIMIT_REACHED" => {
            DebridError::NotPremium(detail)
        }
        "MAGNET_INVALID_ID" => DebridError::Gone,
        _ => DebridError::Service(format!("AllDebrid: {detail}")),
    }
}

/// The entries of `data.magnets`, whatever shape it takes: an array, a single
/// object (the `id` form), or a map keyed by id.
fn items(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().filter(|item| item.is_object()).collect(),
        Value::Object(_) if is_entry(value) => vec![value],
        Value::Object(map) => map.values().filter(|item| item.is_object()).collect(),
        _ => Vec::new(),
    }
}

fn is_entry(value: &Value) -> bool {
    ["id", "statusCode", "files", "error", "hash"]
        .iter()
        .any(|field| value.get(field).is_some())
}

/// The entry for `remote_id`, or the only entry when it carries no id.
fn pick<'a>(value: &'a Value, remote_id: &str) -> Option<&'a Value> {
    if !is_entry(value) {
        if let Some(entry) = value.get(remote_id).filter(|entry| entry.is_object()) {
            return Some(entry);
        }
    }
    let entries = items(value);
    entries
        .iter()
        .copied()
        .find(|entry| entry.get("id").and_then(text).as_deref() == Some(remote_id))
        .or_else(|| (entries.len() == 1 && entries[0].get("id").is_none()).then(|| entries[0]))
}

fn status_code(magnet: &Value) -> Option<i64> {
    magnet
        .get("statusCode")
        .and_then(number)
        .filter(|code| code.fract() == 0.0)
        .map(|code| code as i64)
}

/// downloaded / size, or uploaded / size while uploading (statusCode 3).
fn progress(magnet: &Value, code: i64) -> u16 {
    let size = magnet.get("size").and_then(number).unwrap_or(0.0);
    let done_field = if code == 3 { "uploaded" } else { "downloaded" };
    let done = magnet.get(done_field).and_then(number).unwrap_or(0.0);
    if size > 0.0 && done.is_finite() {
        permille((done / size).clamp(0.0, 1.0))
    } else {
        0
    }
}

/// Depth-first walk of `{n, e}` folders and `{n, s, l}` files, joining folder
/// names with '/'. Names stay untrusted; the runner sanitises the paths.
fn flatten(nodes: &[Value], prefix: &str, depth: usize, out: &mut Vec<TreeFile>) {
    for node in nodes {
        let Some(name) = node.get("n").and_then(text).filter(|name| !name.is_empty()) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match node.get("e") {
            Some(Value::Array(children)) => {
                if depth < TREE_MAX_DEPTH {
                    flatten(children, &path, depth + 1, out);
                }
            }
            Some(_) => {}
            None => out.push(TreeFile {
                path,
                size: node.get("s").and_then(number).unwrap_or(0.0).max(0.0) as u64,
                link: node
                    .get("l")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            }),
        }
    }
}

fn remote_files(tree: &[TreeFile]) -> Vec<RemoteFile> {
    tree.iter()
        .enumerate()
        .map(|(index, file)| RemoteFile {
            id: index.to_string(),
            path: file.path.clone(),
            size: file.size,
        })
        .collect()
}

fn direct_link(data: &Value) -> Option<String> {
    data.get("link")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|link| link.starts_with("https://") || link.starts_with("http://"))
        .map(str::to_string)
}

fn iso_time(seconds: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    use reqwest::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::debrid::fake::{serve, FakeRequest, FakeResponse, FakeServer};

    pub const KEY: &str = "ad-test-key";
    pub const HASH: &str = "842783e3005495d5d1637f5364b59343c7844707";

    pub struct State {
        pub magnets: BTreeMap<u64, Magnet>,
        pub next_id: u64,
        pub premium: bool,
        /// Status polls a new magnet spends fetching (0 = cached, `ready`).
        pub fetch_polls: u32,
        /// `magnet/files` calls after Ready that still miss a link.
        pub links_late: u32,
        /// `link/unlock` answers `delayed` for files whose path contains one.
        pub delay_names: Vec<String>,
        /// Delayed generation id → (polls still processing, direct link).
        pub delayed: BTreeMap<u64, (u32, String)>,
        pub next_delayed: u64,
        /// Answer the first N direct downloads with 403 (expired link).
        pub expire_links: u32,
        /// Answer the next N API calls with 429.
        pub rate_limit: u32,
        /// Per-item error code for the next upload.
        pub upload_item_error: Option<&'static str>,
        /// Leave the (undocumented) `hash` out of the magnet list.
        pub list_hides_hash: bool,
        /// Files every new magnet gets.
        pub files: Vec<(String, Vec<u8>)>,
    }

    impl Default for State {
        fn default() -> Self {
            State {
                magnets: BTreeMap::new(),
                next_id: 0,
                premium: true,
                fetch_polls: 0,
                links_late: 0,
                delay_names: Vec::new(),
                delayed: BTreeMap::new(),
                next_delayed: 0,
                expire_links: 0,
                rate_limit: 0,
                upload_item_error: None,
                list_hides_hash: false,
                files: vec![
                    ("Album/CD1/01.bin".into(), pattern(3000, 251)),
                    ("Album/02.bin".into(), pattern(1500, 13)),
                    ("Album/Extras/Deep/03.bin".into(), pattern(700, 97)),
                ],
            }
        }
    }

    pub struct Magnet {
        pub hash: String,
        pub polls: u32,
        pub files: Vec<(String, Vec<u8>)>,
        /// statusCode to report instead of progress (5–15).
        pub failed: Option<u32>,
    }

    fn pattern(len: u32, modulo: u32) -> Vec<u8> {
        (0..len).map(|i| (i % modulo) as u8).collect()
    }

    fn ok(data: Value) -> FakeResponse {
        FakeResponse::json(200, json!({"status": "success", "data": data}))
    }

    /// Application errors are HTTP 200 since 2023.
    fn err(code: &str, message: &str) -> FakeResponse {
        FakeResponse::json(
            200,
            json!({"status": "error", "error": {"code": code, "message": message}}),
        )
    }

    fn not_found() -> FakeResponse {
        FakeResponse::json(
            404,
            json!({"status": "error", "error": {"code": "404", "message": "Endpoint doesn't exist"}}),
        )
    }

    /// Query merged with a urlencoded POST body (GET or POST both work).
    fn params(request: &FakeRequest) -> Vec<(String, String)> {
        let mut out = request.query.clone();
        if request.method == "POST" && request.is_form() {
            out.extend(request.form());
        }
        out
    }

    /// Values for `name`, `name[]` or `name[N]` (spellings are loose).
    fn values(params: &[(String, String)], name: &str) -> Vec<String> {
        params
            .iter()
            .filter(|(key, _)| {
                key == name
                    || key.strip_prefix(name).is_some_and(|rest| {
                        rest.strip_prefix('[')
                            .and_then(|rest| rest.strip_suffix(']'))
                            .is_some_and(|index| index.chars().all(|ch| ch.is_ascii_digit()))
                    })
            })
            .map(|(_, value)| value.clone())
            .collect()
    }

    fn hash_of(magnet: &str) -> String {
        let raw = magnet
            .split("btih:")
            .nth(1)
            .map(|rest| rest.split('&').next().unwrap_or_default())
            .unwrap_or(magnet);
        raw.to_ascii_lowercase()
    }

    fn total(magnet: &Magnet) -> usize {
        magnet.files.iter().map(|(_, body)| body.len()).sum()
    }

    /// statusCode 0 → 1 → 3 (uploading) while fetching, then 4.
    fn status_json(id: u64, magnet: &Magnet, fetch_polls: u32, hide_hash: bool) -> Value {
        let size = total(magnet);
        let mut item = if let Some(code) = magnet.failed {
            json!({"id": id, "filename": "Album", "size": size,
                   "status": "File not available, no peer", "statusCode": code})
        } else if fetch_polls == 0 || magnet.polls > fetch_polls {
            // Ready: the progress fields are absent, as in the docs.
            json!({"id": id, "filename": "Album", "size": size, "status": "Ready",
                   "statusCode": 4, "uploadDate": 1_700_000_000, "completionDate": 1_700_000_100})
        } else {
            match magnet.polls.max(1) {
                1 => json!({"id": id, "filename": "Album", "size": size,
                            "status": "In Queue", "statusCode": 0, "downloaded": 0}),
                2 => json!({"id": id, "filename": "Album", "size": size,
                            "status": "Downloading", "statusCode": 1,
                            "downloaded": size / 2, "downloadSpeed": 1000, "seeders": 4}),
                _ => json!({"id": id, "filename": "Album", "size": size,
                            "status": "Uploading", "statusCode": 3,
                            "downloaded": size, "uploaded": size * 3 / 4}),
            }
        };
        if !hide_hash {
            item["hash"] = json!(magnet.hash);
        }
        item
    }

    /// `{n, e}` folders and `{n, s, l}` files from slash paths; `link(i)` is
    /// `None` to leave a file's `l` out.
    fn tree(magnet: &Magnet, link: impl Fn(usize) -> Option<String>) -> Vec<Value> {
        let mut root: Vec<Value> = Vec::new();
        for (index, (path, body)) in magnet.files.iter().enumerate() {
            let parts: Vec<&str> = path.split('/').collect();
            let (name, folders) = parts.split_last().unwrap();
            let mut level = &mut root;
            for folder in folders {
                let position = level
                    .iter()
                    .position(|node| node["n"] == *folder && node.get("e").is_some());
                let position = match position {
                    Some(position) => position,
                    None => {
                        level.push(json!({"n": folder, "e": []}));
                        level.len() - 1
                    }
                };
                level = level[position]["e"].as_array_mut().unwrap();
            }
            let mut node = json!({"n": name, "s": body.len()});
            if let Some(link) = link(index) {
                node["l"] = json!(link);
            }
            level.push(node);
        }
        root
    }

    /// An AllDebrid fake that enforces Bearer auth, form bodies and the
    /// documented parameter spellings, and answers like the real API (HTTP 200
    /// error envelopes, the object form of single-id status, per-item errors).
    pub async fn alldebrid_fake(state: Arc<Mutex<State>>) -> (FakeServer, Url) {
        let origin_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let origin_for_handler = Arc::clone(&origin_slot);
        let server = serve(move |request: &FakeRequest| {
            let mut state = state.lock().unwrap();
            let origin = origin_for_handler.lock().unwrap().clone();
            let path = request.path.as_str();
            // The download host: direct links, no auth.
            if let Some(rest) = path.strip_prefix("/dl/") {
                let generation = rest.split('/').next().unwrap_or_default();
                let (id, index) = generation.split_once('-').unwrap_or_default();
                let id: u64 = id.parse().unwrap_or(0);
                let index: usize = index.parse().unwrap_or(usize::MAX);
                if state.expire_links > 0 {
                    state.expire_links -= 1;
                    return FakeResponse::empty(403);
                }
                return match state.magnets.get(&id).and_then(|m| m.files.get(index)) {
                    Some((_, body)) => FakeResponse::file(request, body, false),
                    None => FakeResponse::empty(404),
                };
            }
            // `l` links are web pages, never the file.
            if path.starts_with("/f/") {
                return FakeResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/html".into())],
                    body: b"<html>AllDebrid file page</html>".to_vec(),
                };
            }
            let (version, route) = if let Some(route) = path.strip_prefix("/v4.1/") {
                ("4.1", route)
            } else if let Some(route) = path.strip_prefix("/v4/") {
                ("4", route)
            } else {
                return not_found();
            };
            if !matches!(request.method.as_str(), "GET" | "POST") {
                return not_found();
            }
            if state.rate_limit > 0 {
                state.rate_limit -= 1;
                return FakeResponse::json(
                    429,
                    json!({"status": "error", "error": {"code": "GENERIC", "message": "Too many requests"}}),
                );
            }
            match request.bearer() {
                None => return err("AUTH_MISSING_APIKEY", "The auth apikey was not sent"),
                Some(key) if key != KEY => {
                    return err("AUTH_BAD_APIKEY", "The auth apikey is invalid")
                }
                _ => {}
            }
            let params = params(request);
            let fetch_polls = state.fetch_polls;
            match route {
                "user" => ok(json!({"user": {
                    "username": "someone", "email": "someone@example.com",
                    "isPremium": state.premium, "isSubscribed": false, "isTrial": false,
                    // A numeric string, as in the docs' example.
                    "premiumUntil": if state.premium { json!("1798761600") } else { json!(0) },
                    "lang": "en", "preferedDomain": "com", "fidelityPoints": 0,
                    "limitedHostersQuotas": {}, "notifications": [],
                }})),
                "magnet/upload" => {
                    let magnets = values(&params, "magnets");
                    let Some(magnet) = magnets.first() else {
                        return err("MAGNET_NO_URI", "No magnet sent");
                    };
                    if !state.premium {
                        return err(
                            "MAGNET_MUST_BE_PREMIUM",
                            "You must be premium to use this feature",
                        );
                    }
                    if let Some(code) = state.upload_item_error.take() {
                        return ok(json!({"magnets": [{"magnet": magnet,
                            "error": {"code": code, "message": "Magnets limit reached"}}]}));
                    }
                    let hash = hash_of(magnet);
                    if hash.len() != 40 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
                        return ok(json!({"magnets": [{"magnet": magnet,
                            "error": {"code": "MAGNET_INVALID_URI", "message": "Magnet is not valid"}}]}));
                    }
                    // Dedupe by hash (undocumented; assumed).
                    let existing = state
                        .magnets
                        .iter()
                        .find(|(_, m)| m.hash == hash)
                        .map(|(id, _)| *id);
                    let id = match existing {
                        Some(id) => id,
                        None => {
                            state.next_id += 1;
                            let id = state.next_id;
                            let files = state.files.clone();
                            state.magnets.insert(
                                id,
                                Magnet {
                                    hash: hash.clone(),
                                    polls: 0,
                                    files,
                                    failed: None,
                                },
                            );
                            id
                        }
                    };
                    let size = total(&state.magnets[&id]);
                    ok(json!({"magnets": [{"magnet": magnet, "hash": hash, "name": "Album",
                        "size": size, "ready": fetch_polls == 0, "id": id}]}))
                }
                // v4 status was discontinued in favour of v4.1.
                "magnet/status" if version == "4" => FakeResponse::json(
                    200,
                    json!({"status": "error", "deprecated": true,
                           "error": {"code": "DISCONTINUED", "message": "Use v4.1"}}),
                ),
                "magnet/status" => {
                    if let Some(raw) = values(&params, "id").first() {
                        let Some((id, magnet)) = raw
                            .parse::<u64>()
                            .ok()
                            .and_then(|id| Some((id, state.magnets.get_mut(&id)?)))
                        else {
                            return err(
                                "MAGNET_INVALID_ID",
                                "This magnet ID does not exists or is invalid",
                            );
                        };
                        magnet.polls += 1;
                        // An object, not an array, when an id is given.
                        return ok(json!({"magnets": status_json(id, magnet, fetch_polls, false)}));
                    }
                    let hide = state.list_hides_hash;
                    let list: Vec<Value> = state
                        .magnets
                        .iter()
                        .map(|(id, magnet)| status_json(*id, magnet, fetch_polls, hide))
                        .collect();
                    ok(json!({"magnets": list}))
                }
                "magnet/files" => {
                    let ids = values(&params, "id");
                    if ids.is_empty() {
                        return err("MAGNET_INVALID_ID", "This magnet ID does not exists or is invalid");
                    }
                    let mut out = Vec::new();
                    for raw in ids {
                        let id: u64 = raw.parse().unwrap_or(0);
                        let Some(magnet) = state.magnets.get(&id) else {
                            out.push(json!({"id": raw, "error": {"code": "MAGNET_INVALID_ID",
                                "message": "This magnet ID does not exists or is invalid"}}));
                            continue;
                        };
                        let ready = magnet.failed.is_none()
                            && (fetch_polls == 0 || magnet.polls > fetch_polls);
                        if !ready {
                            out.push(json!({"id": raw, "files": []}));
                            continue;
                        }
                        let late = state.links_late > 0;
                        let last = magnet.files.len().saturating_sub(1);
                        let files = tree(magnet, |index| {
                            (!(late && index == last)).then(|| format!("{origin}/f/{id}-{index}"))
                        });
                        // Ids are strings here.
                        out.push(json!({"id": raw, "files": files}));
                        if late {
                            state.links_late -= 1;
                        }
                    }
                    ok(json!({"magnets": out}))
                }
                "link/unlock" => {
                    let Some(link) = values(&params, "link").into_iter().next() else {
                        return err("LINK_IS_MISSING", "No link was sent");
                    };
                    let Some(token) = link.strip_prefix(&format!("{origin}/f/")) else {
                        return err("LINK_HOST_NOT_SUPPORTED", "This host or link is not supported");
                    };
                    let (id, index) = token.split_once('-').unwrap_or_default();
                    let id: u64 = id.parse().unwrap_or(0);
                    let index: usize = index.parse().unwrap_or(usize::MAX);
                    let Some((path, body)) = state.magnets.get(&id).and_then(|m| m.files.get(index))
                    else {
                        return err("LINK_DOWN", "This link is not available on the file hoster website");
                    };
                    let filename = path.rsplit('/').next().unwrap_or_default().to_string();
                    let size = body.len();
                    let direct = format!(
                        "{origin}/dl/{id}-{index}/{}",
                        utf8_percent_encode(&filename, NON_ALPHANUMERIC)
                    );
                    if state.delay_names.iter().any(|name| path.contains(name.as_str())) {
                        state.next_delayed += 1;
                        let delayed = state.next_delayed;
                        state.delayed.insert(delayed, (1, direct));
                        return ok(json!({"link": "", "filename": filename, "filesize": size,
                            "delayed": delayed}));
                    }
                    ok(json!({"link": direct, "host": "fake", "filename": filename,
                        "paws": false, "filesize": size, "streams": [],
                        "id": format!("gen{id}{index}"), "hostDomain": "fake.test"}))
                }
                "link/delayed" => {
                    let id: u64 = values(&params, "id")
                        .first()
                        .and_then(|raw| raw.parse().ok())
                        .unwrap_or(0);
                    match state.delayed.get_mut(&id) {
                        None => err("DELAYED_INVALID_ID", "This delayed link id is invalid"),
                        Some((left, _)) if *left > 0 => {
                            *left -= 1;
                            ok(json!({"status": 1, "time_left": 5}))
                        }
                        Some((_, link)) => {
                            ok(json!({"status": 2, "time_left": 0, "link": link.clone()}))
                        }
                    }
                }
                "magnet/delete" => {
                    let id: u64 = values(&params, "id")
                        .first()
                        .and_then(|raw| raw.parse().ok())
                        .unwrap_or(0);
                    if state.magnets.remove(&id).is_none() {
                        return err("MAGNET_INVALID_ID", "Magnet ID is invalid");
                    }
                    ok(json!({"message": "Magnet was successfully deleted"}))
                }
                // Includes the withdrawn `magnet/instant`.
                _ => not_found(),
            }
        })
        .await;
        *origin_slot.lock().unwrap() = server.origin.clone();
        let base = Url::parse(&format!("{}/", server.origin)).unwrap();
        (server, base)
    }

    pub fn magnet() -> String {
        format!("magnet:?xt=urn:btih:{HASH}&dn=Album")
    }

    fn client(base: &Url, key: &str) -> AllDebrid {
        AllDebrid::new(Secret::new(key).unwrap(), Some(base.clone())).unwrap()
    }

    fn fake_state(state: State) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(state))
    }

    #[tokio::test]
    async fn account_reports_premium_free_and_bad_keys() {
        let state = fake_state(State::default());
        let (server, base) = alldebrid_fake(Arc::clone(&state)).await;
        let account = client(&base, KEY).account().await.unwrap();
        assert!(account.premium);
        assert_eq!(account.expires.as_deref(), Some("2027-01-01T00:00:00Z"));
        let user = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/v4/user")
            .unwrap();
        assert_eq!(user.bearer(), Some(KEY));
        assert_eq!(user.query("agent"), Some("torrenter"));
        assert!(user.query.iter().all(|(_, value)| !value.contains(KEY)));

        // A refused key is an HTTP 200 error envelope.
        let error = client(&base, "wrong-key-xyz").account().await.unwrap_err();
        assert!(matches!(error, DebridError::BadKey(_)), "{error:?}");
        assert!(error.to_string().contains("AUTH_BAD_APIKEY"));
        assert!(!error.to_string().contains("wrong-key-xyz"));

        state.lock().unwrap().premium = false;
        let free = client(&base, KEY).account().await.unwrap();
        assert!(!free.premium);
        assert_eq!(free.expires, None);
    }

    #[tokio::test]
    async fn add_poll_files_unlock_delete_flow() {
        let state = fake_state(State {
            fetch_polls: 3,
            links_late: 1,
            delay_names: vec!["02.bin".into()],
            ..State::default()
        });
        let (server, base) = alldebrid_fake(Arc::clone(&state)).await;
        let alldebrid = client(&base, KEY);
        let added = alldebrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(added.remote_id, "1");
        assert!(added.added_by_us);
        let upload = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("magnet/upload"))
            .unwrap();
        assert_eq!(upload.method, "POST");
        assert!(upload.is_form());
        assert_eq!(upload.form_value("magnets[]"), Some(magnet()));

        // statusCode 0 → 1 → 3, progress from downloaded (uploaded while 3).
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(alldebrid.status(&added.remote_id).await.unwrap().state);
        }
        assert_eq!(
            seen,
            [
                RemoteState::Fetching { permille: 0 },
                RemoteState::Fetching { permille: 500 },
                RemoteState::Fetching { permille: 750 },
            ]
        );
        // Ready, but one `l` is still missing: keep waiting.
        let early = alldebrid.status(&added.remote_id).await.unwrap();
        assert_eq!(early.state, RemoteState::Fetching { permille: 1000 });
        let ready = alldebrid.status(&added.remote_id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        let paths: Vec<_> = ready.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Album/CD1/01.bin",
                "Album/02.bin",
                "Album/Extras/Deep/03.bin"
            ]
        );
        let sizes: Vec<_> = ready.files.iter().map(|f| f.size).collect();
        assert_eq!(sizes, [3000, 1500, 700]);
        assert_eq!(ready.files[0].id, "0");
        let requests = server.requests();
        assert!(requests
            .iter()
            .filter(|r| r.path.ends_with("magnet/status"))
            .all(|r| r.path == "/v4.1/magnet/status" && r.method == "POST"));
        let files_call = requests
            .iter()
            .find(|r| r.path.ends_with("magnet/files"))
            .unwrap();
        assert_eq!(files_call.form_value("id[]").as_deref(), Some("1"));

        // Each `/f/` link is unlocked; one comes back delayed.
        let first = alldebrid
            .link(&added.remote_id, &ready.files[0])
            .await
            .unwrap();
        assert!(first.ends_with("/dl/1-0/01%2Ebin"), "{first}");
        let second = alldebrid
            .link(&added.remote_id, &ready.files[1])
            .await
            .unwrap();
        assert!(second.ends_with("/dl/1-1/02%2Ebin"), "{second}");
        assert_eq!(server.count("link/delayed"), 2, "processing, then ready");
        let third = alldebrid
            .link(&added.remote_id, &ready.files[2])
            .await
            .unwrap();
        assert!(third.contains("/dl/1-2/"));
        let unlock = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("link/unlock"))
            .unwrap();
        assert!(unlock.form_value("link").unwrap().contains("/f/1-0"));

        // Re-adding the same hash re-uses it from the list without uploading.
        let again = alldebrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(again.remote_id, added.remote_id);
        assert!(!again.added_by_us);
        assert_eq!(server.count("magnet/upload"), 1);

        alldebrid.delete(&added.remote_id).await.unwrap();
        assert!(state.lock().unwrap().magnets.is_empty());
        assert!(matches!(
            alldebrid.status(&added.remote_id).await,
            Err(DebridError::Gone)
        ));
        // Deleting twice is fine.
        alldebrid.delete(&added.remote_id).await.unwrap();
        // The key only ever travelled in the Authorization header.
        assert!(server.requests().iter().all(|r| !r.path.contains(KEY)
            && r.query.iter().all(|(_, v)| !v.contains(KEY))
            && r.form().iter().all(|(_, v)| !v.contains(KEY))));
    }

    #[tokio::test]
    async fn upload_answering_a_known_id_is_reuse() {
        // Without `hash` in the list, a dedupe by upload is still noticed.
        let state = fake_state(State {
            list_hides_hash: true,
            ..State::default()
        });
        let (server, base) = alldebrid_fake(Arc::clone(&state)).await;
        let alldebrid = client(&base, KEY);
        let first = alldebrid.add(&magnet(), HASH).await.unwrap();
        assert!(first.added_by_us);
        let again = alldebrid.add(&magnet(), HASH).await.unwrap();
        assert_eq!(again.remote_id, first.remote_id);
        assert!(!again.added_by_us);
        assert_eq!(server.count("magnet/upload"), 2);
    }

    #[tokio::test]
    async fn errors_map_to_debrid_errors() {
        let state = fake_state(State::default());
        let (_server, base) = alldebrid_fake(Arc::clone(&state)).await;
        let alldebrid = client(&base, KEY);

        // Per-item error inside a success envelope.
        state.lock().unwrap().upload_item_error = Some("MAGNET_TOO_MANY");
        match alldebrid.add(&magnet(), HASH).await {
            Err(DebridError::Service(detail)) => {
                assert!(detail.contains("MAGNET_TOO_MANY"), "{detail}")
            }
            other => panic!("{other:?}"),
        }
        state.lock().unwrap().upload_item_error = Some("MAGNET_MUST_BE_PREMIUM");
        assert!(matches!(
            alldebrid.add(&magnet(), HASH).await,
            Err(DebridError::NotPremium(_))
        ));
        // A free account is refused at the top level.
        state.lock().unwrap().premium = false;
        assert!(matches!(
            alldebrid.add(&magnet(), HASH).await,
            Err(DebridError::NotPremium(_))
        ));
        state.lock().unwrap().premium = true;

        // 429 is a rate limit.
        state.lock().unwrap().rate_limit = 1;
        assert!(matches!(
            alldebrid.account().await,
            Err(DebridError::RateLimited(None))
        ));
        assert!(alldebrid.account().await.is_ok());

        // Unknown ids are gone, for status, files and delete.
        assert!(matches!(
            alldebrid.status("999").await,
            Err(DebridError::Gone)
        ));
        let file = RemoteFile {
            id: "0".into(),
            path: "Album/CD1/01.bin".into(),
            size: 3000,
        };
        assert!(matches!(
            alldebrid.link("999", &file).await,
            Err(DebridError::Gone)
        ));
        alldebrid.delete("999").await.unwrap();

        // statusCode above 4 is a failure with the provider's reason.
        let added = alldebrid.add(&magnet(), HASH).await.unwrap();
        state
            .lock()
            .unwrap()
            .magnets
            .get_mut(&added.remote_id.parse().unwrap())
            .unwrap()
            .failed = Some(15);
        match alldebrid.status(&added.remote_id).await.unwrap().state {
            RemoteState::Failed(reason) => {
                assert!(
                    reason.contains("no peer") && reason.contains("15"),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }

        // The withdrawn instant endpoint and other unknown routes are 404s.
        match alldebrid.call("v4/magnet/instant", &[]).await {
            Err(DebridError::Service(detail)) => assert!(detail.contains("404"), "{detail}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn envelope_codes_and_shapes() {
        let reply = |status: u16, json: Value| Reply {
            status: StatusCode::from_u16(status).unwrap(),
            json,
        };
        let error = |code: &str, message: &str| {
            envelope(reply(
                200,
                json!({"status": "error", "error": {"code": code, "message": message}}),
            ))
            .unwrap_err()
        };
        assert!(matches!(error("AUTH_BLOCKED", "x"), DebridError::BadKey(_)));
        assert!(matches!(
            error("AUTH_MISSING_APIKEY", "x"),
            DebridError::BadKey(_)
        ));
        assert!(matches!(
            error("MUST_BE_PREMIUM", "x"),
            DebridError::NotPremium(_)
        ));
        assert!(matches!(
            error("FREE_TRIAL_LIMIT_REACHED", "x"),
            DebridError::NotPremium(_)
        ));
        assert_eq!(error("MAGNET_INVALID_ID", "x"), DebridError::Gone);
        let no_server = error(
            "NO_SERVER",
            "Servers are not allowed. Visit https://alldebrid.com/vpn now",
        )
        .to_string();
        assert!(
            no_server.starts_with("AllDebrid:") && !no_server.contains("https://"),
            "{no_server}"
        );
        // No `status` attribute means failure; a bare 401 is a key problem.
        assert!(matches!(
            envelope(reply(200, json!({"data": {}}))),
            Err(DebridError::Service(_))
        ));
        assert!(matches!(
            envelope(reply(401, Value::Null)),
            Err(DebridError::BadKey(_))
        ));

        // `data.magnets` as an object, an array, or a map keyed by id.
        let object = json!({"id": 7, "statusCode": 4});
        assert_eq!(pick(&object, "7"), Some(&object));
        let array = json!([{"id": "6"}, {"id": "7", "statusCode": 1}]);
        assert_eq!(pick(&array, "7").unwrap()["statusCode"], 1);
        let map = json!({"7": {"statusCode": 2}});
        assert_eq!(pick(&map, "7").unwrap()["statusCode"], 2);
        assert_eq!(pick(&json!([]), "7"), None);

        // Nested folders flatten to slash paths; folders without names and
        // over-deep trees are skipped.
        let nodes = json!([
            {"n": "Sub", "e": [
                {"n": "a.txt", "s": "12", "l": "https://x/f/1"},
                {"n": "Deeper", "e": [{"n": "b.txt", "s": 3, "l": "https://x/f/2"}]}
            ]},
            {"n": "", "s": 1, "l": "https://x/f/3"},
            {"n": "c.txt", "s": 5}
        ]);
        let mut out = Vec::new();
        flatten(nodes.as_array().unwrap(), "", 0, &mut out);
        let flat: Vec<_> = out
            .iter()
            .map(|f| (f.path.as_str(), f.size, f.link.is_empty()))
            .collect();
        assert_eq!(
            flat,
            [
                ("Sub/a.txt", 12, false),
                ("Sub/Deeper/b.txt", 3, false),
                ("c.txt", 5, true)
            ]
        );
        assert_eq!(iso_time(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn runner_downloads_an_alldebrid_job_end_to_end() {
        use crate::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
        use crate::debrid::Service;
        use crate::progress::JobSnapshot;

        let state = fake_state(State {
            expire_links: 1,
            delay_names: vec!["03.bin".into()],
            ..State::default()
        });
        let expected = state.lock().unwrap().files.clone();
        let (server, base) = alldebrid_fake(Arc::clone(&state)).await;
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        let run = |key: &str, shared: SharedDebridState| DebridRun {
            service: Service::AllDebrid,
            key: Secret::new(key).unwrap(),
            magnet: magnet(),
            dest: dir.path().to_path_buf(),
            part_count: 4,
            remove_after: true,
            state: shared,
            base: Some(base.clone()),
        };
        start_debrid_reported(run(KEY, Arc::clone(&shared)), Arc::clone(&snapshot))
            .await
            .unwrap();
        assert!(snapshot.is_done(), "status {}", snapshot.status());
        let job = dir.path().join("Album");
        for (path, body) in &expected {
            let relative = path.strip_prefix("Album/").unwrap();
            let written = std::fs::read(job.join(relative)).unwrap();
            assert!(written == *body, "{relative} differs");
        }
        let shared = shared.lock().unwrap().clone();
        assert_eq!(shared.remote_id.as_deref(), Some("1"));
        assert!(shared.added_by_us);
        assert!(shared.files.iter().all(|file| file.done));
        // Remove-after deleted the magnet; the delayed link was waited out.
        assert!(state.lock().unwrap().magnets.is_empty());
        assert_eq!(server.count("magnet/delete"), 1);
        assert!(server.count("link/delayed") >= 2);
        assert!(server.requests().iter().all(|r| !r.path.contains(KEY)
            && r.query.iter().all(|(_, v)| !v.contains(KEY))
            && r.form().iter().all(|(_, v)| !v.contains(KEY))));

        // A bad key fails with a message that carries no key and no link.
        let error = start_debrid_reported(
            run("wrong-key-123", SharedDebridState::default()),
            Arc::new(JobSnapshot::default()),
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

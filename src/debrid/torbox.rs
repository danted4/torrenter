//! TorBox (api.torbox.app/v1/api). Research: .cursor/plans/debrid-research/torbox.md
//!
//! Quirks encoded here: `createtorrent` takes a form (JSON is rejected),
//! `controltorrent` takes JSON, `requestdl` takes the key as `?token=` (so its
//! URL is a secret), `mylist` is cached for 10 minutes unless `bypass_cache`,
//! `mylist?id=` may answer a one-element array, "not found" is a 404 with
//! `success: true`, many client errors arrive as HTTP 500 (branch on `error`),
//! a torrent can read "cached" before its files appear, and file id 0 is valid.

use serde_json::{json, Value};
use url::Url;

use super::api::{number, permille, text, Api, Reply};
use super::{Account, Added, DebridError, Remote, RemoteFile, RemoteState, Secret};

const BASE: &str = "https://api.torbox.app/v1/api/";

pub(crate) struct TorBox {
    api: Api,
    key: Secret,
}

impl TorBox {
    pub fn new(key: Secret, base: Option<Url>) -> Result<TorBox, DebridError> {
        let base = base.unwrap_or_else(|| Url::parse(BASE).expect("valid TorBox base"));
        Ok(TorBox {
            api: Api::new(base)?,
            key,
        })
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.key.expose())
    }

    pub async fn account(&self) -> Result<Account, DebridError> {
        let request = self
            .api
            .client()
            .get(self.api.url("user/me"))
            .header("Authorization", self.bearer());
        let data = envelope(self.api.send(request).await?)?;
        let plan = data.get("plan").and_then(number).unwrap_or(0.0);
        Ok(Account {
            premium: plan > 0.0,
            expires: data.get("premium_expires_at").and_then(text),
        })
    }

    /// Re-use the torrent when the account already has it (TorBox keeps one
    /// copy per hash), otherwise add it.
    pub async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        if let Some(remote_id) = self.find_by_hash(hash_hex).await? {
            return Ok(Added {
                remote_id,
                added_by_us: false,
            });
        }
        let request = self
            .api
            .client()
            .post(self.api.url("torrents/createtorrent"))
            .header("Authorization", self.bearer())
            .form(&[("magnet", magnet), ("seed", "3"), ("allow_zip", "false")]);
        let data = match envelope(self.api.send(request).await?) {
            Ok(data) => data,
            // "Download already queued" and similar: it is on the account.
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
        let item = match &data {
            Value::Array(items) => items
                .iter()
                .find(|item| {
                    item.get("hash")
                        .and_then(Value::as_str)
                        .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex))
                })
                .or_else(|| items.first())
                .cloned()
                .unwrap_or(Value::Null),
            other => other.clone(),
        };
        let remote_id = item
            .get("torrent_id")
            .or_else(|| item.get("id"))
            .and_then(text);
        match remote_id {
            Some(remote_id) => Ok(Added {
                remote_id,
                added_by_us: true,
            }),
            None if item.get("queued_id").is_some() => Err(DebridError::Service(
                "TorBox queued it behind your active-download limit; retry later".into(),
            )),
            None => Err(DebridError::Service("TorBox returned no torrent id".into())),
        }
    }

    async fn find_by_hash(&self, hash_hex: &str) -> Result<Option<String>, DebridError> {
        let request = self
            .api
            .client()
            .get(self.api.url("torrents/mylist"))
            .header("Authorization", self.bearer())
            .query(&[("bypass_cache", "true")]);
        let data = match envelope(self.api.send(request).await?) {
            Ok(data) => data,
            Err(DebridError::Gone) => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(items) = data.as_array() else {
            return Ok(None);
        };
        Ok(items
            .iter()
            .find(|item| {
                let primary = item
                    .get("hash")
                    .and_then(Value::as_str)
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex));
                let alternative = item
                    .get("alternative_hashes")
                    .and_then(Value::as_array)
                    .is_some_and(|hashes| {
                        hashes.iter().any(|hash| {
                            hash.as_str()
                                .is_some_and(|hash| hash.eq_ignore_ascii_case(hash_hex))
                        })
                    });
                primary || alternative
            })
            .and_then(|item| item.get("id").and_then(text)))
    }

    pub async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        let request = self
            .api
            .client()
            .get(self.api.url("torrents/mylist"))
            .header("Authorization", self.bearer())
            .query(&[("id", remote_id), ("bypass_cache", "true")]);
        let data = envelope(self.api.send(request).await?)?;
        let item = match data {
            Value::Array(mut items) if !items.is_empty() => items.swap_remove(0),
            Value::Object(_) => data,
            _ => return Err(DebridError::Gone),
        };
        let files: Vec<RemoteFile> = item
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|file| {
                        Some(RemoteFile {
                            id: file.get("id").and_then(text)?,
                            path: file
                                .get("name")
                                .and_then(text)
                                .or_else(|| file.get("short_name").and_then(text))?,
                            size: file.get("size").and_then(number).unwrap_or(0.0) as u64,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let finished = item.get("download_finished").and_then(Value::as_bool) == Some(true)
            && item.get("download_present").and_then(Value::as_bool) == Some(true);
        let raw_state = item
            .get("download_state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let state_name = raw_state
            .split(" (")
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let state = if matches!(
            state_name.as_str(),
            "error" | "missingfiles" | "missing" | "failed"
        ) {
            RemoteState::Failed(format!("TorBox reports the torrent as {raw_state}"))
        } else if finished && !files.is_empty() {
            RemoteState::Ready
        } else if finished || state_name == "cached" {
            // Cached before its file list appears: done, just not listed yet.
            RemoteState::Fetching { permille: 1000 }
        } else {
            RemoteState::Fetching {
                permille: item
                    .get("progress")
                    .and_then(number)
                    .map(permille)
                    .unwrap_or(0),
            }
        };
        Ok(Remote { state, files })
    }

    /// A CDN link for one file. Resolved once per file (`redirect=false`); the
    /// request URL carries the key, so it is never logged or returned.
    pub async fn link(&self, remote_id: &str, file: &RemoteFile) -> Result<String, DebridError> {
        let request = self
            .api
            .client()
            .get(self.api.url("torrents/requestdl"))
            .query(&[
                ("token", self.key.expose()),
                ("torrent_id", remote_id),
                ("file_id", file.id.as_str()),
                ("redirect", "false"),
            ]);
        let data = envelope(self.api.send(request).await?)?;
        data.as_str()
            .filter(|link| link.starts_with("https://") || link.starts_with("http://"))
            .map(str::to_string)
            .ok_or_else(|| DebridError::Service("TorBox returned no download link".into()))
    }

    pub async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        let torrent_id = remote_id
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(remote_id));
        let request = self
            .api
            .client()
            .post(self.api.url("torrents/controltorrent"))
            .header("Authorization", self.bearer())
            .json(&json!({"torrent_id": torrent_id, "operation": "delete"}));
        match envelope(self.api.send(request).await?) {
            Ok(_) | Err(DebridError::Gone) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Unwrap `{success, error, detail, data}`. TorBox sends `success: true` with
/// `error: "ITEM_NOT_FOUND"` on 404, and many client errors as HTTP 500, so the
/// `error` code decides, not the status.
fn envelope(reply: Reply) -> Result<Value, DebridError> {
    let code = reply
        .json
        .get("error")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .map(str::to_string);
    let success = reply.json.get("success").and_then(Value::as_bool);
    if code.is_none() && success == Some(true) && reply.status.is_success() {
        return Ok(reply.json.get("data").cloned().unwrap_or(Value::Null));
    }
    let detail = reply
        .json
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("TorBox answered HTTP {}", reply.status.as_u16()));
    Err(match code.as_deref() {
        Some("NO_AUTH" | "BAD_TOKEN" | "AUTH_ERROR") => DebridError::BadKey(detail),
        Some("PLAN_RESTRICTED_FEATURE") => DebridError::NotPremium(detail),
        Some("ITEM_NOT_FOUND") => DebridError::Gone,
        Some("RATE_LIMIT") => DebridError::RateLimited(None),
        _ if reply.status.as_u16() == 401 || reply.status.as_u16() == 403 => {
            DebridError::BadKey(detail)
        }
        _ => DebridError::Service(format!("TorBox: {detail}")),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::debrid::fake::{serve, FakeRequest, FakeResponse, FakeServer};

    pub const KEY: &str = "tb-test-key";
    pub const HASH: &str = "c9e15763f722f23e98a29decdfae341b98d53056";

    #[derive(Default)]
    pub struct State {
        pub torrents: BTreeMap<u64, Torrent>,
        pub next_id: u64,
        /// Polls until a new torrent finishes (0 = cached).
        pub fetch_polls: u32,
        /// Answer the first N file-link downloads with 403 (expired link).
        pub expire_links: u32,
        pub plan: u64,
    }

    pub struct Torrent {
        pub hash: String,
        pub polls: u32,
        pub files: Vec<(String, Vec<u8>)>,
        pub failed: bool,
    }

    fn envelope_ok(data: Value) -> FakeResponse {
        FakeResponse::json(
            200,
            json!({"success": true, "error": null, "detail": "ok", "data": data}),
        )
    }

    fn envelope_err(status: u16, code: &str, detail: &str) -> FakeResponse {
        FakeResponse::json(
            status,
            json!({"success": false, "error": code, "detail": detail, "data": null}),
        )
    }

    fn torrent_json(id: u64, torrent: &Torrent, fetch_polls: u32) -> Value {
        let done = torrent.polls >= fetch_polls;
        let files: Vec<Value> = torrent
            .files
            .iter()
            .enumerate()
            .map(|(index, (name, body))| {
                json!({"id": index, "name": name, "short_name": name.rsplit('/').next(), "size": body.len()})
            })
            .collect();
        json!({
            "id": id, "hash": torrent.hash, "name": "Album",
            "download_state": if torrent.failed { "error" } else if done { "cached" } else { "downloading" },
            // TorBox sometimes reports progress as a percent.
            "progress": if done { 1.0 } else { 45.0 },
            "download_finished": done && !torrent.failed,
            "download_present": done && !torrent.failed,
            "files": if done { Value::from(files) } else { Value::from(Vec::<Value>::new()) },
        })
    }

    /// A TorBox fake that enforces the documented body formats and auth styles.
    pub async fn torbox_fake(state: Arc<Mutex<State>>) -> (FakeServer, Url) {
        let origin_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let origin_for_handler = Arc::clone(&origin_slot);
        let server = serve(move |request: &FakeRequest| {
            let mut state = state.lock().unwrap();
            let path = request.path.as_str();
            if let Some(rest) = path.strip_prefix("/dld/") {
                let mut parts = rest.split('/');
                let id: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let file: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                if state.expire_links > 0 {
                    state.expire_links -= 1;
                    return FakeResponse::empty(403);
                }
                return match state.torrents.get(&id).and_then(|t| t.files.get(file)) {
                    Some((_, body)) => FakeResponse::file(request, body, false),
                    None => FakeResponse::empty(404),
                };
            }
            let Some(route) = path.strip_prefix("/v1/api/") else {
                return envelope_err(404, "ENDPOINT_NOT_FOUND", "Endpoint not found.");
            };
            if route == "torrents/requestdl" {
                if request.query("token") != Some(KEY) {
                    return envelope_err(403, "BAD_TOKEN", "Your token is invalid or has expired.");
                }
                let id: u64 = request.query("torrent_id").and_then(|v| v.parse().ok()).unwrap_or(0);
                let file = request.query("file_id").unwrap_or("0");
                if !state.torrents.contains_key(&id) {
                    return envelope_err(500, "UNKNOWN_ERROR", "There was an error.");
                }
                let origin = origin_for_handler.lock().unwrap().clone();
                return envelope_ok(json!(format!("{origin}/dld/{id}/{file}?token=cdn-{KEY}")));
            }
            match request.bearer() {
                None => return envelope_err(403, "NO_AUTH", "Please provide a valid API token."),
                Some(key) if key != KEY => {
                    return envelope_err(403, "BAD_TOKEN", "Your token is invalid or has expired.")
                }
                _ => {}
            }
            match route {
                "user/me" => envelope_ok(json!({"plan": state.plan, "premium_expires_at": "2027-01-01T00:00:00Z"})),
                "torrents/createtorrent" => {
                    if request.method != "POST" {
                        return envelope_err(405, "INVALID_OPTION", "Method not allowed.");
                    }
                    if request.is_json() || !request.is_form() {
                        return FakeResponse::json(400, json!({"detail": "Body must be a form."}));
                    }
                    let Some(magnet) = request.form_value("magnet") else {
                        return envelope_err(400, "MISSING_REQUIRED_OPTION", "You must provide either a file or magnet link.");
                    };
                    let hash = magnet
                        .split("btih:")
                        .nth(1)
                        .and_then(|rest| rest.split('&').next())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if state.torrents.values().any(|t| t.hash == hash) {
                        return envelope_err(400, "DIFF_ISSUE", "Download already queued.");
                    }
                    state.next_id += 1;
                    let id = state.next_id;
                    let files = vec![
                        ("Album/CD1/01.bin".to_string(), (0..3000u32).map(|i| (i % 251) as u8).collect()),
                        ("Album/02.bin".to_string(), (0..1500u32).map(|i| (i % 13) as u8).collect()),
                    ];
                    state.torrents.insert(id, Torrent { hash: hash.clone(), polls: 0, files, failed: false });
                    envelope_ok(json!({"hash": hash, "torrent_id": id, "auth_id": "x"}))
                }
                "torrents/mylist" => {
                    let fetch_polls = state.fetch_polls;
                    if let Some(id) = request.query("id").and_then(|v| v.parse::<u64>().ok()) {
                        let Some(torrent) = state.torrents.get_mut(&id) else {
                            return FakeResponse::json(404, json!({"success": true, "error": "ITEM_NOT_FOUND", "detail": "No torrents found for this user.", "data": null}));
                        };
                        torrent.polls += 1;
                        // Sometimes a one-element array instead of an object.
                        let item = torrent_json(id, torrent, fetch_polls);
                        return envelope_ok(if id % 2 == 0 { json!([item]) } else { item });
                    }
                    let list: Vec<Value> = state
                        .torrents
                        .iter()
                        .map(|(id, torrent)| torrent_json(*id, torrent, fetch_polls))
                        .collect();
                    envelope_ok(Value::from(list))
                }
                "torrents/controltorrent" => {
                    if !request.is_json() {
                        return FakeResponse::json(422, json!({"detail": "JSON body required."}));
                    }
                    let body = request.json().unwrap_or_default();
                    let id = body.get("torrent_id").and_then(Value::as_u64).unwrap_or(0);
                    if state.torrents.remove(&id).is_none() {
                        return FakeResponse::json(404, json!({"success": true, "error": "ITEM_NOT_FOUND", "detail": "No torrents found.", "data": null}));
                    }
                    envelope_ok(Value::Null)
                }
                _ => envelope_err(404, "ENDPOINT_NOT_FOUND", "Endpoint not found."),
            }
        })
        .await;
        *origin_slot.lock().unwrap() = server.origin.clone();
        let base = Url::parse(&format!("{}/v1/api/", server.origin)).unwrap();
        (server, base)
    }

    pub fn magnet() -> String {
        format!("magnet:?xt=urn:btih:{HASH}&dn=Album")
    }

    fn client(base: &Url, key: &str) -> TorBox {
        TorBox::new(Secret::new(key).unwrap(), Some(base.clone())).unwrap()
    }

    #[tokio::test]
    async fn account_reports_plan_and_bad_keys() {
        let state = Arc::new(Mutex::new(State {
            plan: 2,
            ..State::default()
        }));
        let (_server, base) = torbox_fake(Arc::clone(&state)).await;
        let account = client(&base, KEY).account().await.unwrap();
        assert!(account.premium);
        assert_eq!(account.expires.as_deref(), Some("2027-01-01T00:00:00Z"));
        assert!(matches!(
            client(&base, "wrong").account().await,
            Err(DebridError::BadKey(_))
        ));
        state.lock().unwrap().plan = 0;
        assert!(!client(&base, KEY).account().await.unwrap().premium);
    }

    #[tokio::test]
    async fn add_poll_link_delete_flow() {
        let state = Arc::new(Mutex::new(State {
            fetch_polls: 2,
            ..State::default()
        }));
        let (server, base) = torbox_fake(Arc::clone(&state)).await;
        let torbox = client(&base, KEY);
        let added = torbox.add(&magnet(), HASH).await.unwrap();
        assert!(added.added_by_us);
        let create = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("createtorrent"))
            .unwrap();
        assert!(create.is_form(), "createtorrent must be a form");

        let first = torbox.status(&added.remote_id).await.unwrap();
        assert_eq!(first.state, RemoteState::Fetching { permille: 450 });
        let ready = torbox.status(&added.remote_id).await.unwrap();
        assert_eq!(ready.state, RemoteState::Ready);
        assert_eq!(ready.files.len(), 2);
        assert_eq!(ready.files[0].id, "0", "file id 0 is valid");
        assert_eq!(ready.files[0].path, "Album/CD1/01.bin");

        let link = torbox
            .link(&added.remote_id, &ready.files[1])
            .await
            .unwrap();
        assert!(link.contains("/dld/1/1"));
        let requestdl = server
            .requests()
            .into_iter()
            .find(|r| r.path.ends_with("requestdl"))
            .unwrap();
        assert_eq!(requestdl.query("token"), Some(KEY));
        assert_eq!(requestdl.query("redirect"), Some("false"));
        assert!(server
            .requests()
            .iter()
            .filter(|r| r.path.ends_with("mylist") && r.query("id").is_some())
            .all(|r| r.query("bypass_cache") == Some("true")));

        // Re-adding the same hash re-uses it and never claims ownership.
        let again = torbox.add(&magnet(), HASH).await.unwrap();
        assert_eq!(again.remote_id, added.remote_id);
        assert!(!again.added_by_us);

        torbox.delete(&added.remote_id).await.unwrap();
        assert!(matches!(
            torbox.status(&added.remote_id).await,
            Err(DebridError::Gone)
        ));
        // Deleting twice is fine.
        torbox.delete(&added.remote_id).await.unwrap();
    }

    #[tokio::test]
    async fn failed_state_and_array_status_are_understood() {
        let state = Arc::new(Mutex::new(State::default()));
        let (_server, base) = torbox_fake(Arc::clone(&state)).await;
        let torbox = client(&base, KEY);
        // id 2 answers as a one-element array
        torbox
            .add(
                "magnet:?xt=urn:btih:1111111111111111111111111111111111111111",
                &"1".repeat(40),
            )
            .await
            .unwrap();
        let second = torbox.add(&magnet(), HASH).await.unwrap();
        assert_eq!(second.remote_id, "2");
        assert_eq!(torbox.status("2").await.unwrap().state, RemoteState::Ready);
        state.lock().unwrap().torrents.get_mut(&2).unwrap().failed = true;
        assert!(matches!(
            torbox.status("2").await.unwrap().state,
            RemoteState::Failed(_)
        ));
    }
}

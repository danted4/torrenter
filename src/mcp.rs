//! Stdio MCP server: Cursor/Claude spawn `torrenter mcp`. Stdout is JSON-RPC only.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::ServiceExt;
use rmcp::transport::io::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use torrenter::debrid::Service;
use torrenter::identity::{self, VERSION};
use torrenter::jobs::JobKind;
use torrenter::paths;
use torrenter::session::{self, Job, Session, SessionEvent};
use torrenter::torrent_file;
use torrenter::update::load_config;
use torrenter::validate::{is_valid_download_url, is_valid_magnet_url};

const TICK: Duration = Duration::from_millis(200);

#[derive(Clone)]
struct Mcp {
    session: Arc<Session>,
    notices: Arc<Mutex<Vec<String>>>,
    #[expect(dead_code, reason = "tool_handler macro reads this field")]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MagnetArg {
    uri: String,
    /// Optional debrid account: its name, or a provider id when that provider
    /// has one account (see `debrid_services`). Omit to download from peers.
    /// Uses the user's own configured key; never ask for one.
    #[serde(default)]
    debrid: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TorrentFileArg {
    /// Absolute path of a local .torrent file (`~/` is expanded).
    path: String,
    /// Optional debrid account, as for `queue_magnet`. Omit to download from
    /// peers.
    #[serde(default)]
    debrid: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct HttpArg {
    url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IdArg {
    id: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct JobToolView {
    id: u64,
    kind: String,
    source: String,
    title: String,
    status: String,
    downloaded: u64,
    total: u64,
    uploaded: u64,
    upload_bps: u64,
    peers: u64,
    speed: f64,
    up_speed: f64,
    seed: bool,
    dest: String,
    added: String,
    paused: bool,
    finished: bool,
    active: bool,
    error: Option<String>,
    /// Debrid route only: account name, provider fetch progress (0–100) and file i/n.
    #[serde(skip_serializing_if = "Option::is_none")]
    debrid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    debrid_progress: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_count: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DebridServiceView {
    id: String,
    label: String,
    /// A key is configured (env or settings). The key itself is never exposed.
    configured: bool,
    /// Not yet confirmed with a real account; failures are worth reporting.
    experimental: bool,
    /// Names of the user's accounts for this provider (pass one as `debrid`).
    accounts: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DebridServicesOut {
    services: Vec<DebridServiceView>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct JobsOut {
    jobs: Vec<JobToolView>,
    notices: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct QueueOut {
    id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetFilesArg {
    id: u64,
    /// File ids from `list_files` to download; the rest stop. All ids = the
    /// whole torrent; an empty list stops every file.
    ids: Vec<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct FilesOut {
    id: u64,
    groups: Vec<FileGroupOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct FileGroupOut {
    /// Parent folder inside the torrent; empty for files at its root.
    folder: String,
    files: Vec<FileOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct FileOut {
    id: u32,
    name: String,
    size: u64,
    /// Bytes on disk.
    have: u64,
    selected: bool,
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Account names by id, read once per tool call.
fn account_names() -> std::collections::HashMap<String, String> {
    torrenter::credentials::accounts()
        .into_iter()
        .map(|account| (account.id, account.name))
        .collect()
}

fn job_view(job: &Job, names: &std::collections::HashMap<String, String>) -> JobToolView {
    let error = job.snapshot.error();
    let debrid = job
        .debrid
        .as_ref()
        .map(|debrid| (debrid.account.clone(), debrid.view()));
    JobToolView {
        id: job.id,
        kind: job.kind.as_str().to_string(),
        source: job.source.clone(),
        title: job.snapshot.title(),
        status: error.clone().unwrap_or_else(|| job.snapshot.status()),
        downloaded: job.snapshot.downloaded.load(Ordering::Relaxed),
        total: job.snapshot.total.load(Ordering::Relaxed),
        uploaded: job.snapshot.uploaded.load(Ordering::Relaxed),
        upload_bps: job.snapshot.upload_bps.load(Ordering::Relaxed),
        peers: job.snapshot.peers.load(Ordering::Relaxed),
        speed: job.speed,
        up_speed: job.up_speed,
        seed: job.seed,
        dest: job.dest.display().to_string(),
        added: job.added.clone(),
        paused: job.snapshot.is_paused(),
        finished: job.snapshot.is_done(),
        active: job.snapshot.is_active(),
        error,
        debrid: debrid.as_ref().map(|(account, _)| {
            names
                .get(account)
                .cloned()
                .unwrap_or_else(|| account.clone())
        }),
        debrid_progress: debrid
            .as_ref()
            .map(|(_, state)| f32::from(state.remote_permille) / 10.0),
        file_index: debrid
            .as_ref()
            .filter(|(_, state)| state.file_index > 0)
            .map(|(_, state)| state.file_index),
        file_count: debrid
            .as_ref()
            .filter(|(_, state)| !state.files.is_empty())
            .map(|(_, state)| state.files.len()),
    }
}

fn push_notice(notices: &Mutex<Vec<String>>, text: String) {
    mutex_lock(notices).push(text);
}

fn take_notices(notices: &Mutex<Vec<String>>) -> Vec<String> {
    std::mem::take(&mut *mutex_lock(notices))
}

fn event_text(event: SessionEvent) -> String {
    match event {
        SessionEvent::RestoredMissing(1) => "1 saved download is missing from its folder".into(),
        SessionEvent::RestoredMissing(n) => {
            format!("{n} saved downloads are missing from their folders")
        }
        SessionEvent::DownloadFailed { error, .. } => error,
    }
}

async fn tick_loop(
    session: Arc<Session>,
    notices: Arc<Mutex<Vec<String>>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(TICK) => {
                for event in session.drain_events() {
                    push_notice(&notices, event_text(event));
                }
                session.persist(!session.any_active());
            }
        }
    }
}

impl Mcp {
    fn open() -> Result<Self> {
        let config = load_config();
        let dest = paths::effective_dir(&config);
        if let Err(error) = paths::ensure_dir(&dest) {
            eprintln!("download folder: {error:#}");
        }
        let jobs_path = torrenter::jobs::jobs_path().unwrap_or_else(|_| dest.join("jobs.json"));
        let session = Session::open_leech(dest, jobs_path);
        session.set_connections(config.connections);
        session.set_debrid_remove_after(config.debrid_remove_after);
        let notices = Arc::new(Mutex::new(Vec::new()));
        match session.hydrate() {
            session::Hydrate::Empty | session::Hydrate::Loaded => {}
            session::Hydrate::Corrupt(error) => {
                let text = format!("could not read saved jobs: {error}");
                eprintln!("{text}");
                push_notice(&notices, text);
            }
        }
        Ok(Self {
            session: Arc::new(session),
            notices,
            tool_router: Self::tool_router(),
        })
    }
}

#[tool_router]
impl Mcp {
    #[tool(description = "torrenter version banner")]
    fn banner(&self) -> String {
        identity::banner()
    }

    #[tool(description = "Download folder (read-only)")]
    fn dest(&self) -> String {
        self.session.dest().display().to_string()
    }

    #[tool(
        description = "Queue a magnet download (leech). Pass `debrid` (an account name from debrid_services) only when the user asks to download through their debrid account."
    )]
    fn queue_magnet(
        &self,
        Parameters(MagnetArg { uri, debrid }): Parameters<MagnetArg>,
    ) -> Result<Json<QueueOut>, String> {
        if !is_valid_magnet_url(&uri) {
            return Err("Invalid magnet URL!".into());
        }
        let via = debrid_account(debrid.as_deref())?;
        self.session
            .queue_with(JobKind::Magnet, &uri, via.as_deref())
            .map(|id| Json(QueueOut { id }))
    }

    #[tool(
        description = "Queue a local .torrent file (leech), by its path. Pass `debrid` only when the user asks to download through their debrid account."
    )]
    fn queue_torrent_file(
        &self,
        Parameters(TorrentFileArg { path, debrid }): Parameters<TorrentFileArg>,
    ) -> Result<Json<QueueOut>, String> {
        let path = expand_home(path.trim());
        let bytes = torrent_file::read_capped(&path).map_err(|error| format!("{error:#}"))?;
        let via = debrid_account(debrid.as_deref())?;
        self.session
            .queue_torrent(&bytes, via.as_deref())
            .map(|id| Json(QueueOut { id }))
    }

    #[tool(
        description = "Debrid providers this build supports, with the names of the user's accounts for each (keys are never shown). Experimental providers are not yet confirmed with a real account."
    )]
    fn debrid_services(&self) -> Json<DebridServicesOut> {
        let all = torrenter::credentials::accounts();
        Json(DebridServicesOut {
            services: Service::offered()
                .iter()
                .map(|service| {
                    let accounts: Vec<String> = all
                        .iter()
                        .filter(|account| account.service == *service)
                        .map(|account| account.name.clone())
                        .collect();
                    DebridServiceView {
                        id: service.id().into(),
                        label: service.label().into(),
                        configured: !accounts.is_empty(),
                        experimental: !service.verified(),
                        accounts,
                    }
                })
                .collect(),
        })
    }

    #[tool(description = "Queue an HTTP(S) download")]
    fn queue_http(
        &self,
        Parameters(HttpArg { url }): Parameters<HttpArg>,
    ) -> Result<Json<QueueOut>, String> {
        if !is_valid_download_url(&url) {
            return Err("Invalid HTTP(S) URL!".into());
        }
        self.session
            .queue(JobKind::Http, &url)
            .map(|id| Json(QueueOut { id }))
    }

    #[tool(description = "List jobs and one-shot notices")]
    fn jobs(&self) -> Json<JobsOut> {
        Json(JobsOut {
            jobs: {
                let names = account_names();
                self.session
                    .jobs()
                    .iter()
                    .map(|job| job_view(job, &names))
                    .collect()
            },
            notices: take_notices(&self.notices),
        })
    }

    #[tool(description = "Pause or resume a job")]
    fn pause(&self, Parameters(IdArg { id }): Parameters<IdArg>) -> Result<String, String> {
        if !self.session.has_job(id) {
            return Err("job gone".into());
        }
        self.session.toggle_pause(id);
        Ok("ok".into())
    }

    #[tool(description = "Stop a job")]
    fn stop(&self, Parameters(IdArg { id }): Parameters<IdArg>) -> Result<String, String> {
        if !self.session.has_job(id) {
            return Err("job gone".into());
        }
        self.session.stop(id);
        Ok("ok".into())
    }

    #[tool(
        description = "Remove a job from the list (downloaded files, and any copy on a debrid account, are left as they are)"
    )]
    fn remove(&self, Parameters(IdArg { id }): Parameters<IdArg>) -> Result<String, String> {
        if !self.session.has_job(id) {
            return Err("job gone".into());
        }
        self.session.remove(id);
        Ok("ok".into())
    }

    #[tool(
        description = "List a torrent job's files, grouped by folder, with sizes, bytes on disk and whether each is selected (not for HTTP or debrid jobs, or a magnet still fetching its metadata)"
    )]
    fn list_files(
        &self,
        Parameters(IdArg { id }): Parameters<IdArg>,
    ) -> Result<Json<FilesOut>, String> {
        if !self.session.has_job(id) {
            return Err("job gone".into());
        }
        let files = self.session.job_files(id).ok_or_else(|| {
            "no file list for this job (HTTP, debrid, or the magnet's metadata has not arrived)"
                .to_string()
        })?;
        Ok(Json(FilesOut {
            id: files.id,
            groups: files
                .groups
                .into_iter()
                .map(|group| FileGroupOut {
                    folder: group.folder,
                    files: group
                        .files
                        .into_iter()
                        .map(|file| FileOut {
                            id: file.id,
                            name: file.name,
                            size: file.size,
                            have: file.have,
                            selected: file.selected,
                        })
                        .collect(),
                })
                .collect(),
        }))
    }

    #[tool(
        description = "Choose which files of a torrent job download (ids from list_files). A running job applies it at once; a finished job starts again for newly chosen files; files on disk are never deleted. Returns the job id"
    )]
    fn set_files(
        &self,
        Parameters(SetFilesArg { id, ids }): Parameters<SetFilesArg>,
    ) -> Result<Json<QueueOut>, String> {
        self.session
            .set_files(id, &ids)
            .map(|id| Json(QueueOut { id }))
    }

    #[tool(
        description = "Restart a job (leech, same dest, same route; never removes anything from a debrid account)"
    )]
    fn restart(
        &self,
        Parameters(IdArg { id }): Parameters<IdArg>,
    ) -> Result<Json<QueueOut>, String> {
        let job = self.session.job(id).ok_or_else(|| "job gone".to_string())?;
        self.session
            .restart(id, false, job.dest)
            .map(|id| Json(QueueOut { id }))
    }
}

/// The account id for an optional `debrid` argument (a name, or a provider id
/// when that provider has one account).
fn debrid_account(debrid: Option<&str>) -> Result<Option<String>, String> {
    let Some(query) = debrid.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    torrenter::credentials::resolve(query)
        .filter(|account| Service::offered().contains(&account.service))
        .map(|account| Some(account.id))
        .ok_or_else(|| match Service::parse(query) {
            // A provider with no account: the user adds one.
            Some(service)
                if !torrenter::credentials::accounts()
                    .iter()
                    .any(|account| account.service == service) =>
            {
                format!("add a {} API key in settings first", service.label())
            }
            _ => format!("no single debrid account matches {query:?}; see debrid_services"),
        })
}

fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(path)),
        None => std::path::PathBuf::from(path),
    }
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("torrenter", VERSION))
            .with_instructions(
                "Magnet, .torrent file (queue_torrent_file, by local path) and HTTP downloads. A magnet or .torrent can go through the user's own debrid service (`debrid`) only when they ask; keys are configured by the user, never through MCP. Poll jobs. Do not run the GUI at the same time.",
            )
    }
}

pub async fn run() -> Result<()> {
    let mcp = Mcp::open()?;
    let session = Arc::clone(&mcp.session);
    let notices = Arc::clone(&mcp.notices);
    let (stop_tx, stop_rx) = watch::channel(false);
    let ticker = tokio::spawn(tick_loop(Arc::clone(&session), notices, stop_rx));
    let running = mcp.serve(stdio()).await?;
    running.waiting().await?;
    let _ = stop_tx.send(true);
    let _ = ticker.await;
    session.persist(true);
    Ok(())
}

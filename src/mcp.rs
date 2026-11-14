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
    /// Optional debrid service id (see `debrid_services`). Omit to download
    /// from peers. Uses the user's own configured key; never ask for one.
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
    /// Debrid route only: service id, provider fetch progress (0–100) and file i/n.
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

fn mutex_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn job_view(job: &Job) -> JobToolView {
    let error = job.snapshot.error();
    let debrid = job
        .debrid
        .as_ref()
        .map(|debrid| (debrid.service, debrid.view()));
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
        debrid: debrid.as_ref().map(|(service, _)| service.id().to_string()),
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
        description = "Queue a magnet download (leech). Pass `debrid` (a service id from debrid_services) only when the user asks to download through their debrid service."
    )]
    fn queue_magnet(
        &self,
        Parameters(MagnetArg { uri, debrid }): Parameters<MagnetArg>,
    ) -> Result<Json<QueueOut>, String> {
        if !is_valid_magnet_url(&uri) {
            return Err("Invalid magnet URL!".into());
        }
        let via = match debrid.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
            None => None,
            Some(id) => Some(
                Service::parse(id)
                    .filter(|service| Service::shipped().contains(service))
                    .ok_or_else(|| format!("unknown debrid service {id:?}; see debrid_services"))?,
            ),
        };
        self.session
            .queue_with(JobKind::Magnet, &uri, via)
            .map(|id| Json(QueueOut { id }))
    }

    #[tool(
        description = "Debrid services this build supports and whether the user configured a key for each (keys are never shown)"
    )]
    fn debrid_services(&self) -> Json<DebridServicesOut> {
        Json(DebridServicesOut {
            services: Service::shipped()
                .iter()
                .map(|service| DebridServiceView {
                    id: service.id().into(),
                    label: service.label().into(),
                    configured: self.session.has_debrid_key(*service),
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
            jobs: self.session.jobs().iter().map(job_view).collect(),
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

    #[tool(description = "Remove a job from the list")]
    fn remove(&self, Parameters(IdArg { id }): Parameters<IdArg>) -> Result<String, String> {
        if !self.session.has_job(id) {
            return Err("job gone".into());
        }
        self.session.remove(id);
        Ok("ok".into())
    }

    #[tool(description = "Restart a job (leech, same dest, same route)")]
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

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("torrenter", VERSION))
            .with_instructions(
                "Magnet and HTTP downloads. A magnet can go through the user's own debrid service (queue_magnet `debrid`) only when they ask; keys are configured by the user, never through MCP. Poll jobs. Do not run the GUI at the same time.",
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

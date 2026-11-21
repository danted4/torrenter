use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

use crate::debrid::run::{start_debrid_reported, DebridRun, DebridState, SharedDebridState};
use crate::debrid::{Secret, Service};
use crate::http::{
    filename_from_url, http_payload_present, http_resume_present, start_url_download_reported,
    PART_COUNT,
};
use crate::jobs::{self, JobKind, LoadJobs, PersistedJob, RestoreAction};
use crate::paths::plain_path;
use crate::progress::{is_stopped, until_stop, JobSnapshot};
use crate::torrent::{
    magnet_display_name, magnet_info_hash_hex, magnet_payload_present, start_download_reported,
    OnMetadata,
};
use crate::torrent_file;
use crate::validate::{is_valid_download_url, is_valid_magnet_url};

pub const MAX_ACTIVE_DOWNLOADS: usize = 4;
const PERSIST_EVERY: Duration = Duration::from_secs(2);

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub source: String,
    pub snapshot: Arc<JobSnapshot>,
    pub added: String,
    pub last_bytes: u64,
    pub last_uploaded: u64,
    pub last_check: Instant,
    pub speed: f64,
    pub up_speed: f64,
    pub seed: bool,
    pub dest: PathBuf,
    /// HTTP parts per file, fixed when the job was created (resume needs it).
    pub part_count: usize,
    /// Debrid route: provider plus shared provider-side progress.
    pub debrid: Option<JobDebrid>,
    /// From a private `.torrent`: only ever started from its stored copy, never
    /// the magnet (the DHT must not see it) and never through debrid.
    pub private_torrent: bool,
    /// Fields a newer build wrote on this record, written back on every save.
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone)]
pub struct JobDebrid {
    pub service: Service,
    /// The account (named key) the job runs through; its torrent lives there.
    pub account: String,
    pub state: SharedDebridState,
}

impl JobDebrid {
    pub fn view(&self) -> DebridState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// How a restart routes a magnet: as before, from peers, or via a debrid
/// account (by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Keep,
    Peers,
    Via(String),
}

pub fn persist_record(job: &Job) -> PersistedJob {
    let debrid = job.debrid.as_ref().map(JobDebrid::view);
    let files = match job.kind {
        JobKind::Magnet => {
            let files = job.snapshot.files();
            (files.only().cloned(), files.have.clone())
        }
        _ => (None, Vec::new()),
    };
    let files = PersistedSelection {
        only: files.0,
        have: files.1,
    };
    PersistedJob {
        id: job.id,
        kind: job.kind.as_str().to_string(),
        source: job.source.clone(),
        dest: job.dest.clone(),
        seed: job.seed,
        added: job.added.clone(),
        title: job.snapshot.title(),
        downloaded: job.snapshot.downloaded.load(Ordering::Relaxed),
        total: job.snapshot.total.load(Ordering::Relaxed),
        status: job.snapshot.status(),
        paused: job.snapshot.is_paused(),
        error: job.snapshot.error(),
        part_count: (job.kind != JobKind::Magnet)
            .then(|| u8::try_from(job.part_count).unwrap_or(u8::MAX)),
        service: job.debrid.as_ref().map(|d| d.service.id().to_string()),
        account: job.debrid.as_ref().map(|d| d.account.clone()),
        remote_id: debrid.as_ref().and_then(|d| d.remote_id.clone()),
        added_by_us: debrid.as_ref().is_some_and(|d| d.added_by_us),
        files: debrid.map(|d| d.files).unwrap_or_default(),
        private_torrent: job.private_torrent,
        only_files: files.only,
        file_have: files.have,
        extra: job.extra.clone(),
    }
}

struct PersistedSelection {
    only: Option<Vec<usize>>,
    have: Vec<u64>,
}

fn persist_fingerprint(jobs: &[Job]) -> String {
    jobs.iter()
        .map(|job| {
            let record = persist_record(job);
            format!(
                "{}:{}:{}:{}:{}:{}:{:?}:{}:{:?}:{:?}:{}:{:?}:{}:{}",
                record.id,
                record.kind,
                record.downloaded,
                record.total,
                record.status,
                record.paused,
                record.error,
                record.title,
                record.remote_id,
                record.part_count,
                record.files.iter().filter(|file| file.done).count(),
                record.only_files,
                record.file_have.iter().sum::<u64>(),
                record.private_torrent
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

enum EngineCmd {
    Download {
        id: u64,
        kind: JobKind,
        source: String,
        snapshot: Arc<JobSnapshot>,
        seed: bool,
        dest: PathBuf,
        part_count: usize,
        debrid: Option<DebridLaunch>,
        /// Where a `.torrent` copy for this magnet may be kept.
        torrent_store: PathBuf,
        /// Run only from that copy (a private torrent).
        private_torrent: bool,
    },
    Probe(Vec<ProbeSpec>),
    DeleteRemote {
        service: Service,
        key: Secret,
        remote_id: String,
        base: Option<Url>,
    },
    /// A one-shot notice for every front (sent back as a job-less failure).
    Notice(String),
}

struct DebridLaunch {
    service: Service,
    key: Secret,
    state: SharedDebridState,
    remove_after: bool,
    base: Option<Url>,
}

struct ProbeSpec {
    id: u64,
    kind: JobKind,
    source: String,
    dest: PathBuf,
    filename: String,
}

pub enum SessionEvent {
    DownloadFailed { id: u64, error: String },
    RestoredMissing(usize),
}

enum EngineEvent {
    DownloadFailed {
        id: u64,
        error: String,
    },
    /// A magnet's metadata arrived: its rebuilt `.torrent`, to keep a copy.
    Resolved(Vec<u8>),
    Probed(Vec<(u64, bool, bool)>),
}

pub enum Hydrate {
    Empty,
    Loaded,
    Corrupt(String),
}

struct Inner {
    dest: PathBuf,
    jobs_path: PathBuf,
    seed: bool,
    leech_only: bool,
    jobs: Vec<Job>,
    /// Records of kinds this build does not know, written back untouched.
    foreign: Vec<serde_json::Value>,
    next_id: u64,
    persist_at: Instant,
    saved_fingerprint: String,
    /// Connections setting for new HTTP and debrid jobs.
    part_count: usize,
    debrid_remove_after: bool,
    /// Keys handed in at runtime (mobile keychains, tests). Desktop also reads
    /// `credentials.json` / env when no key is held here.
    /// Mobile accounts held in memory: id → provider and key.
    keys: HashMap<String, (Service, Secret)>,
    desktop_keys: bool,
    debrid_bases: HashMap<Service, Url>,
}

pub struct Session {
    inner: Arc<Mutex<Inner>>,
    engine: Sender<EngineCmd>,
    events: Mutex<Receiver<EngineEvent>>,
}

impl Session {
    pub fn open(dest: PathBuf, jobs_path: PathBuf, seed: bool) -> Self {
        Self::open_inner(dest, jobs_path, seed, false)
    }

    /// MCP: never upload, including jobs restored from a GUI `jobs.json`.
    pub fn open_leech(dest: PathBuf, jobs_path: PathBuf) -> Self {
        Self::open_inner(dest, jobs_path, false, true)
    }

    fn open_inner(dest: PathBuf, jobs_path: PathBuf, seed: bool, leech_only: bool) -> Self {
        let (engine_tx, engine_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || engine_loop(engine_rx, event_tx));
        Self::with_channels(dest, jobs_path, seed, leech_only, engine_tx, event_rx)
    }

    fn with_channels(
        dest: PathBuf,
        jobs_path: PathBuf,
        seed: bool,
        leech_only: bool,
        engine_tx: Sender<EngineCmd>,
        event_rx: Receiver<EngineEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                dest: plain_path(dest),
                jobs_path,
                seed: seed && !leech_only,
                leech_only,
                jobs: Vec::new(),
                foreign: Vec::new(),
                next_id: 1,
                persist_at: Instant::now(),
                saved_fingerprint: String::new(),
                part_count: PART_COUNT,
                debrid_remove_after: false,
                keys: HashMap::new(),
                desktop_keys: true,
                debrid_bases: HashMap::new(),
            })),
            engine: engine_tx,
            events: Mutex::new(event_rx),
        }
    }

    pub fn dest(&self) -> PathBuf {
        mutex_lock(&self.inner).dest.clone()
    }

    pub fn set_dest(&self, dest: PathBuf) {
        mutex_lock(&self.inner).dest = plain_path(dest);
    }

    pub fn seed(&self) -> bool {
        mutex_lock(&self.inner).seed
    }

    pub fn set_seed(&self, seed: bool) {
        let mut inner = mutex_lock(&self.inner);
        if inner.leech_only {
            inner.seed = false;
            return;
        }
        inner.seed = seed;
    }

    /// Connections per download for new jobs (`None` = Auto = 4).
    pub fn set_connections(&self, parts: Option<u8>) {
        mutex_lock(&self.inner).part_count = parts
            .map(|parts| usize::from(parts.clamp(1, crate::update::MAX_CONNECTIONS)))
            .unwrap_or(PART_COUNT);
    }

    pub fn connections(&self) -> usize {
        mutex_lock(&self.inner).part_count
    }

    pub fn set_debrid_remove_after(&self, enabled: bool) {
        mutex_lock(&self.inner).debrid_remove_after = enabled;
    }

    /// Mobile: keys live only in memory (the app's keychain hands them over);
    /// never read `credentials.json`.
    pub fn use_memory_keys(&self) {
        mutex_lock(&self.inner).desktop_keys = false;
    }

    /// Mobile: hold an account's key in memory (`None` forgets it).
    pub fn set_debrid_account(&self, id: &str, service: Service, key: Option<Secret>) {
        let mut inner = mutex_lock(&self.inner);
        match key {
            Some(key) => {
                inner.keys.insert(id.to_string(), (service, key));
            }
            None => {
                inner.keys.remove(id);
            }
        }
    }

    /// One account per provider, its id the provider id (today's phones).
    pub fn set_debrid_key(&self, service: Service, key: Option<Secret>) {
        self.set_debrid_account(service.id(), service, key);
    }

    pub fn has_account(&self, id: &str) -> bool {
        resolve_account(&mutex_lock(&self.inner), id).is_some()
    }

    #[doc(hidden)]
    /// Tests: point a provider at a fake server.
    pub fn set_debrid_base(&self, service: Service, base: Url) {
        mutex_lock(&self.inner).debrid_bases.insert(service, base);
    }

    pub fn hydrate(&self) -> Hydrate {
        let mut inner = mutex_lock(&self.inner);
        match jobs::load_jobs_from(&inner.jobs_path) {
            LoadJobs::Empty => Hydrate::Empty,
            LoadJobs::Corrupt(error) => Hydrate::Corrupt(error),
            LoadJobs::Loaded(list) => {
                let leech_only = inner.leech_only;
                let mut probes = Vec::new();
                let mut cleaned = false;
                inner.next_id = inner.next_id.max(list.max_id() + 1);
                inner.foreign = list.foreign;
                for record in list.jobs {
                    let Some(kind) = record.kind_id() else {
                        continue;
                    };
                    let snapshot = Arc::new(JobSnapshot::default());
                    record.apply_to(&snapshot);
                    if kind == JobKind::Magnet {
                        load_file_list(&inner, &record.source, &snapshot);
                    }
                    // Folders saved verbatim by an earlier Windows build
                    // (\\?\E:\...) come back plain everywhere they show.
                    let dest = plain_path(record.dest.clone());
                    cleaned |= dest != record.dest;
                    let wants_engine = record.wants_engine();
                    if record.id >= inner.next_id {
                        inner.next_id = record.id + 1;
                    }
                    if wants_engine {
                        probes.push(ProbeSpec {
                            id: record.id,
                            kind,
                            source: record.source.clone(),
                            dest: dest.clone(),
                            filename: record.title.clone(),
                        });
                    }
                    let debrid = (kind == JobKind::Debrid)
                        .then(|| record.service.as_deref().and_then(Service::parse))
                        .flatten()
                        .map(|service| JobDebrid {
                            service,
                            // Jobs saved before accounts used the provider id.
                            account: record
                                .account
                                .clone()
                                .unwrap_or_else(|| service.id().to_string()),
                            state: Arc::new(Mutex::new(DebridState {
                                remote_id: record.remote_id.clone(),
                                added_by_us: record.added_by_us,
                                files: record.files.clone(),
                                ..DebridState::default()
                            })),
                        });
                    inner.jobs.push(Job {
                        id: record.id,
                        kind,
                        source: record.source,
                        snapshot,
                        added: record.added,
                        last_bytes: record.downloaded,
                        last_uploaded: 0,
                        last_check: Instant::now(),
                        speed: 0.0,
                        up_speed: 0.0,
                        seed: record.seed && !leech_only && kind == JobKind::Magnet,
                        dest,
                        // Jobs saved before the setting existed used 4 parts.
                        part_count: record.part_count.map(usize::from).unwrap_or(PART_COUNT),
                        debrid,
                        private_torrent: record.private_torrent,
                        extra: record.extra,
                    });
                }
                // Rewrite now when a folder was cleaned, so jobs.json holds the
                // plain form too (the fingerprint does not include folders).
                if inner.leech_only || cleaned {
                    inner.saved_fingerprint.clear();
                    persist_inner(&mut inner, true);
                } else {
                    inner.saved_fingerprint = persist_fingerprint(&inner.jobs);
                }
                prune_torrents(&inner);
                drop(inner);
                if !probes.is_empty() {
                    let _ = self.engine.send(EngineCmd::Probe(probes));
                }
                Hydrate::Loaded
            }
        }
    }

    pub fn persist(&self, force: bool) {
        persist_inner(&mut mutex_lock(&self.inner), force);
    }

    pub fn drain_events(&self) -> Vec<SessionEvent> {
        let mut incoming = Vec::new();
        {
            let rx = mutex_lock(&self.events);
            while let Ok(event) = rx.try_recv() {
                incoming.push(event);
            }
        }
        let mut out = Vec::new();
        for event in incoming {
            match event {
                EngineEvent::DownloadFailed { id, error } => {
                    {
                        let inner = mutex_lock(&self.inner);
                        if let Some(job) = inner.jobs.iter().find(|job| job.id == id) {
                            if !job.snapshot.is_stopped() {
                                job.snapshot.set_error(error.clone());
                            }
                        }
                    }
                    self.persist(true);
                    out.push(SessionEvent::DownloadFailed { id, error });
                }
                EngineEvent::Resolved(bytes) => self.keep_resolved(&bytes),
                EngineEvent::Probed(results) => {
                    let missing = self.apply_probes(results);
                    if missing > 0 {
                        out.push(SessionEvent::RestoredMissing(missing));
                    }
                }
            }
        }
        out
    }

    fn apply_probes(&self, results: Vec<(u64, bool, bool)>) -> usize {
        let mut inner = mutex_lock(&self.inner);
        let mut missing = 0usize;
        let mut starts = Vec::new();
        for (id, present, resumable) in results {
            let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
                continue;
            };
            let record = persist_record(&inner.jobs[index]);
            match record.after_probe(present, resumable) {
                RestoreAction::Start => starts.push(index),
                RestoreAction::Missing => {
                    missing += 1;
                    inner.jobs[index]
                        .snapshot
                        .mark_stopped_with(record.missing_reason());
                }
                RestoreAction::Idle => {}
            }
        }
        let mut cmds = Vec::new();
        for index in starts {
            let job = inner.jobs[index].clone();
            match launch(&inner, &job) {
                Ok(cmd) => cmds.push(cmd),
                Err(error) => job.snapshot.set_error(error),
            }
        }
        persist_inner(&mut inner, true);
        drop(inner);
        for cmd in cmds {
            let _ = self.engine.send(cmd);
        }
        missing
    }

    pub fn duplicate_active_note(&self, kind: JobKind, source: &str) -> Option<String> {
        duplicate_note(&mutex_lock(&self.inner).jobs, kind, source)
    }

    pub fn queue(&self, kind: JobKind, source: &str) -> Result<u64, String> {
        self.queue_with(kind, source, None)
    }

    /// Queue a `.torrent` file: keep a copy beside `jobs.json` and queue the
    /// magnet built from it (through `via` like any magnet). The engine starts
    /// from the copy, so there is no metadata wait.
    ///
    /// A private torrent (its tracker forbids DHT and peer exchange) gets a
    /// magnet without trackers (their URLs carry the user's passkey), runs only
    /// from the copy, and cannot go through debrid.
    pub fn queue_torrent(&self, bytes: &[u8], via: Option<&str>) -> Result<u64, String> {
        let imported = torrent_file::parse(bytes).map_err(|error| format!("{error:#}"))?;
        if imported.private && via.is_some() {
            return Err(PRIVATE_NO_DEBRID.into());
        }
        self.queue_inner(
            JobKind::Magnet,
            &imported.magnet,
            via,
            Some((&imported, bytes)),
        )
    }

    /// Queue a job; a magnet with `via` (an account id) goes through that
    /// debrid account.
    pub fn queue_with(
        &self,
        kind: JobKind,
        source: &str,
        via: Option<&str>,
    ) -> Result<u64, String> {
        self.queue_inner(kind, source, via, None)
    }

    /// `torrent`: a `.torrent` the magnet was built from. Its copy is saved
    /// under the session lock after the duplicate check, so a refused file
    /// never overwrites a running job's copy and no prune can race the save.
    fn queue_inner(
        &self,
        kind: JobKind,
        source: &str,
        via: Option<&str>,
        torrent: Option<(&torrent_file::Imported, &[u8])>,
    ) -> Result<u64, String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("paste a magnet or HTTP URL first".into());
        }
        let kind = match (kind, via) {
            (JobKind::Magnet | JobKind::Debrid, Some(_)) => JobKind::Debrid,
            (JobKind::Debrid, None) => return Err("pick a debrid service".into()),
            (JobKind::Http, Some(_)) => return Err("debrid applies to magnet links".into()),
            (kind, None) => kind,
        };
        let valid = match kind {
            JobKind::Magnet | JobKind::Debrid => is_valid_magnet_url(source),
            JobKind::Http => is_valid_download_url(source),
        };
        if !valid {
            let shown = if kind.takes_magnet() {
                "magnet"
            } else {
                "http"
            };
            return Err(format!("invalid {shown} URL"));
        }
        let mut inner = mutex_lock(&self.inner);
        let debrid = match via {
            Some(account) => Some(JobDebrid {
                service: account_service(&inner, account)?,
                account: account.to_string(),
                state: SharedDebridState::default(),
            }),
            None => None,
        };
        if let Some(note) = duplicate_note(&inner.jobs, kind, source) {
            return Err(note);
        }
        if let Some((imported, bytes)) = torrent {
            torrent_file::save(&torrent_file::store_dir(&inner.jobs_path), imported, bytes)
                .map_err(|error| format!("{error:#}"))?;
        }
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        match kind {
            JobKind::Magnet | JobKind::Debrid => {
                if let Some(name) = magnet_display_name(source) {
                    snapshot.set_title(name);
                }
            }
            JobKind::Http => snapshot.set_title(filename_from_url(source, None)),
        }
        let id = inner.next_id;
        let seed = kind == JobKind::Magnet && inner.seed && !inner.leech_only;
        let job = Job {
            id,
            kind,
            source: source.to_string(),
            snapshot: Arc::clone(&snapshot),
            added: today(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed,
            dest: inner.dest.clone(),
            part_count: inner.part_count,
            debrid,
            private_torrent: torrent.is_some_and(|(imported, _)| imported.private),
            extra: Default::default(),
        };
        if let (JobKind::Magnet, Some((_, bytes))) = (kind, torrent) {
            if let Ok(list) = torrent_file::files(bytes) {
                job.snapshot.files().set_list(Arc::new(list));
            }
        }
        let cmd = match launch(&inner, &job) {
            Ok(cmd) => cmd,
            Err(error) => {
                prune_torrents(&inner);
                return Err(error);
            }
        };
        inner.next_id += 1;
        let _ = self.engine.send(cmd);
        inner.jobs.push(job);
        persist_inner(&mut inner, true);
        Ok(id)
    }

    pub fn restart(&self, id: u64, seed: bool, dest: PathBuf) -> Result<u64, String> {
        self.restart_with(id, Route::Keep, seed, dest, false)
    }

    /// True when this re-route should ask whether to remove the job's torrent
    /// from the service it leaves: torrenter added it, it has a remote id, the
    /// job is not done and `route` leaves that service. Same wording as clear.
    pub fn reroute_asks_about_remote(&self, id: u64, route: &Route) -> bool {
        let inner = mutex_lock(&self.inner);
        inner
            .jobs
            .iter()
            .find(|job| job.id == id)
            .and_then(|job| left_remote(job, route))
            .is_some()
    }

    /// Restart a job, optionally re-routing a magnet (peers ⇄ a provider).
    /// Re-using the same provider keeps its torrent id and finished files, so
    /// the download resumes instead of starting over. Leaving a service never
    /// deletes on its own: `remove_from_service` is the user's answer to
    /// [`Self::reroute_asks_about_remote`] (best effort, like clear).
    pub fn restart_with(
        &self,
        id: u64,
        route: Route,
        seed: bool,
        dest: PathBuf,
        remove_from_service: bool,
    ) -> Result<u64, String> {
        let dest = plain_path(dest);
        let mut inner = mutex_lock(&self.inner);
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            return Err("job gone".into());
        };
        self.relaunch(
            &mut inner,
            index,
            route,
            seed,
            dest,
            remove_from_service,
            false,
        )
    }

    /// Restart (a new id, added today) or, with `reopen`, start a done job
    /// again under its own id and date (a file was ticked on it: no task of
    /// the old run is left to send late events). Magnet jobs keep their file
    /// selection either way.
    #[allow(clippy::too_many_arguments)]
    fn relaunch(
        &self,
        inner: &mut Inner,
        index: usize,
        route: Route,
        seed: bool,
        dest: PathBuf,
        remove_from_service: bool,
        reopen: bool,
    ) -> Result<u64, String> {
        let old = inner.jobs[index].clone();
        let source = old.source.clone();
        if old.private_torrent && matches!(route, Route::Via(_)) {
            return Err(PRIVATE_NO_DEBRID.into());
        }
        let debrid = match (old.kind, &route) {
            (JobKind::Http, Route::Via(_)) => return Err("debrid applies to magnet links".into()),
            (_, Route::Peers) => None,
            (_, Route::Via(account)) => Some(match &old.debrid {
                // Same account: its torrent and finished files carry over.
                Some(existing) if &existing.account == account => JobDebrid {
                    service: existing.service,
                    account: account.clone(),
                    state: Arc::new(Mutex::new(existing.view())),
                },
                _ => JobDebrid {
                    service: account_service(inner, account)?,
                    account: account.clone(),
                    state: SharedDebridState::default(),
                },
            }),
            (_, Route::Keep) => old.debrid.as_ref().map(|existing| JobDebrid {
                service: existing.service,
                account: existing.account.clone(),
                state: Arc::new(Mutex::new(existing.view())),
            }),
        };
        let kind = match (old.kind, &debrid) {
            (JobKind::Http, _) => JobKind::Http,
            (_, Some(_)) => JobKind::Debrid,
            (_, None) => JobKind::Magnet,
        };
        let others: Vec<Job> = inner
            .jobs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, job)| job.clone())
            .collect();
        if let Some(note) = duplicate_note(&others, kind, &source) {
            return Err(note);
        }
        let title = inner.jobs[index].snapshot.title();
        inner.jobs[index].snapshot.mark_stopped();
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        if !title.is_empty() {
            snapshot.set_title(title);
        }
        if kind == JobKind::Magnet {
            *snapshot.files() = old.snapshot.files().clone();
        }
        let (new_id, added) = if reopen {
            snapshot.downloaded.store(
                old.snapshot.downloaded.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            snapshot.total.store(
                old.snapshot.total.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            (old.id, old.added.clone())
        } else {
            // Claim the id now: a failed launch still stores the job under it.
            let new_id = inner.next_id;
            inner.next_id += 1;
            (new_id, today())
        };
        let leave_remote = left_remote(&old, &route)
            .filter(|_| remove_from_service)
            .map(|left| delete_remote_cmd(inner, left));
        if let Some(cmd) = leave_remote {
            let _ = self.engine.send(cmd);
        }
        let seed = kind == JobKind::Magnet && seed && !inner.leech_only;
        // A fresh HTTP split starts from scratch anyway; keep the job's own
        // count for debrid so its parts resume.
        let part_count = match kind {
            JobKind::Debrid if old.kind == JobKind::Debrid => old.part_count,
            _ => inner.part_count,
        };
        let job = Job {
            id: new_id,
            kind,
            source,
            snapshot,
            added,
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed,
            dest,
            part_count,
            debrid,
            private_torrent: old.private_torrent,
            extra: old.extra.clone(),
        };
        let cmd = match launch(inner, &job) {
            Ok(cmd) => cmd,
            Err(error) => {
                job.snapshot.set_error(error.clone());
                inner.jobs[index] = job;
                persist_inner(inner, true);
                return Err(error);
            }
        };
        let _ = self.engine.send(cmd);
        inner.jobs[index] = job;
        persist_inner(inner, true);
        Ok(new_id)
    }

    /// A torrent job's files for the files screen (`None` for HTTP and
    /// debrid jobs, and while a magnet's file list is not known yet).
    pub fn job_files(&self, id: u64) -> Option<JobFiles> {
        let inner = mutex_lock(&self.inner);
        let job = inner
            .jobs
            .iter()
            .find(|job| job.id == id && job.kind == JobKind::Magnet)?;
        let files = job.snapshot.files();
        let list = files.list()?.clone();
        Some(JobFiles {
            id,
            groups: group_files(&list, &files),
        })
    }

    /// Choose a torrent job's files (`ids` = file ids from [`Self::job_files`]).
    /// A running job applies it at once, a stopped or failed one keeps it for
    /// its restart, and a done job starts again under its own id. Returns the
    /// job's id.
    pub fn set_files(&self, id: u64, ids: &[u32]) -> Result<u64, String> {
        let mut inner = mutex_lock(&self.inner);
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            return Err("job gone".into());
        };
        let job = inner.jobs[index].clone();
        if job.kind != JobKind::Magnet {
            return Err("only torrent jobs have files to choose".into());
        }
        let reopen = {
            let mut files = job.snapshot.files();
            let Some(list) = files.list().cloned() else {
                return Err("this torrent's file list is not known yet".into());
            };
            let only = normalize_selection(&list, ids)?;
            if files.only() == only.as_ref() {
                return Ok(job.id);
            }
            let adds = adds_files(files.only(), only.as_ref());
            files.set_only(only);
            files.generation += 1;
            // Checked under the files lock: the engine finishes a job only
            // under it, after checking the generation it applied. A done job
            // starts again only for a newly ticked file; an untick just stays
            // stored (nothing to download).
            adds && job.snapshot.is_done()
        };
        if reopen {
            let (seed, dest) = (job.seed, job.dest.clone());
            return self.relaunch(&mut inner, index, Route::Keep, seed, dest, false, true);
        }
        persist_inner(&mut inner, true);
        Ok(job.id)
    }

    /// Keep the `.torrent` librqbit rebuilt for a magnet (once, and only while
    /// a job still refers to its hash), so its files and restart need no
    /// metadata wait. A private one is marked so it never leaves its tracker.
    fn keep_resolved(&self, bytes: &[u8]) {
        let Ok(imported) = torrent_file::parse(bytes) else {
            return;
        };
        let mut inner = mutex_lock(&self.inner);
        let matching: Vec<usize> = inner
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| {
                job.kind == JobKind::Magnet
                    && magnet_info_hash_hex(&job.source).as_deref()
                        == Some(imported.hash_hex.as_str())
            })
            .map(|(index, _)| index)
            .collect();
        if matching.is_empty() {
            return;
        }
        let store = torrent_file::store_dir(&inner.jobs_path);
        if torrent_file::load(&store, &imported.hash_hex).is_none()
            && torrent_file::save(&store, &imported, bytes).is_err()
        {
            return;
        }
        let list = torrent_file::files(bytes).ok().map(Arc::new);
        for index in matching {
            let job = &mut inner.jobs[index];
            if imported.private {
                job.private_torrent = true;
            }
            let mut files = job.snapshot.files();
            if let (None, Some(list)) = (files.list(), &list) {
                files.set_list(Arc::clone(list));
            }
        }
        persist_inner(&mut inner, true);
    }

    pub fn toggle_pause(&self, id: u64) {
        let inner = mutex_lock(&self.inner);
        if let Some(job) = inner.jobs.iter().find(|job| job.id == id) {
            job.snapshot.toggle_pause();
        }
        drop(inner);
        self.persist(true);
    }

    pub fn stop(&self, id: u64) {
        let inner = mutex_lock(&self.inner);
        if let Some(job) = inner.jobs.iter().find(|job| job.id == id) {
            job.snapshot.mark_stopped();
        }
        drop(inner);
        self.persist(true);
    }

    pub fn remove(&self, id: u64) {
        let mut inner = mutex_lock(&self.inner);
        if let Some(index) = inner.jobs.iter().position(|job| job.id == id) {
            inner.jobs[index].snapshot.mark_stopped();
            inner.jobs.remove(index);
            persist_inner(&mut inner, true);
            prune_torrents(&inner);
        }
    }

    /// True when clearing this job should ask whether to delete it from the
    /// provider too: a debrid job torrenter added that did not finish.
    pub fn clear_asks_about_remote(&self, id: u64) -> bool {
        let inner = mutex_lock(&self.inner);
        inner.jobs.iter().any(|job| {
            job.id == id
                && !job.snapshot.is_done()
                && job.debrid.as_ref().is_some_and(|debrid| {
                    let state = debrid.view();
                    state.added_by_us && state.remote_id.is_some()
                })
        })
    }

    /// Clear a job and delete its torrent from the provider (only one
    /// torrenter added). Best effort: failures arrive as a notice.
    pub fn remove_with_remote(&self, id: u64) {
        let delete = {
            let inner = mutex_lock(&self.inner);
            inner.jobs.iter().find(|job| job.id == id).and_then(|job| {
                let debrid = job.debrid.as_ref()?;
                let state = debrid.view();
                if !state.added_by_us {
                    return None;
                }
                Some(delete_remote_cmd(
                    &inner,
                    LeftRemote {
                        account: debrid.account.clone(),
                        service: debrid.service,
                        remote_id: state.remote_id?,
                    },
                ))
            })
        };
        self.remove(id);
        if let Some(cmd) = delete {
            let _ = self.engine.send(cmd);
        }
    }

    pub fn job(&self, id: u64) -> Option<Job> {
        mutex_lock(&self.inner)
            .jobs
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }

    pub fn jobs(&self) -> Vec<Job> {
        let mut inner = mutex_lock(&self.inner);
        for job in &mut inner.jobs {
            tick_job_rates(job);
        }
        inner.jobs.clone()
    }

    pub fn is_empty(&self) -> bool {
        mutex_lock(&self.inner).jobs.is_empty()
    }

    pub fn has_job(&self, id: u64) -> bool {
        mutex_lock(&self.inner).jobs.iter().any(|job| job.id == id)
    }

    pub fn with_jobs_mut<R>(&self, f: impl FnOnce(&mut Vec<Job>) -> R) -> R {
        f(&mut mutex_lock(&self.inner).jobs)
    }

    pub fn any_active(&self) -> bool {
        mutex_lock(&self.inner)
            .jobs
            .iter()
            .any(|job| job.snapshot.is_active())
    }
}

pub(crate) fn tick_job_rates(job: &mut Job) {
    let now = Instant::now();
    let downloaded = job.snapshot.downloaded.load(Ordering::Relaxed);
    let uploaded = job.snapshot.uploaded.load(Ordering::Relaxed);
    let engine_up = job.snapshot.upload_bps.load(Ordering::Relaxed) as f64;
    // Magnets: librqbit's smoothed live speed. Their `downloaded` grows only
    // by whole verified pieces, so a byte delta reads 0, then a burst. A
    // stopped or paused job no longer updates it, so it reads 0 then.
    let engine_down =
        (job.kind == JobKind::Magnet && job.snapshot.is_active() && !job.snapshot.is_paused())
            .then(|| job.snapshot.download_bps.load(Ordering::Relaxed) as f64);
    if let Some(speed) = engine_down {
        job.speed = speed;
    }
    let elapsed = now.saturating_duration_since(job.last_check).as_secs_f64();
    if elapsed >= 0.4 {
        if job.kind != JobKind::Magnet {
            job.speed = (downloaded.saturating_sub(job.last_bytes) as f64) / elapsed;
        } else if engine_down.is_none() {
            job.speed = 0.0;
        }
        job.up_speed = if engine_up > 0.0 {
            engine_up
        } else {
            (uploaded.saturating_sub(job.last_uploaded) as f64) / elapsed
        };
        job.last_bytes = downloaded;
        job.last_uploaded = uploaded;
        job.last_check = now;
    } else if engine_up > 0.0 {
        job.up_speed = engine_up;
    }
}

/// One torrent is one job whichever route it takes (peers or a provider), since
/// both write the same folder.
fn duplicate_note(jobs: &[Job], kind: JobKind, source: &str) -> Option<String> {
    let conflict = jobs.iter().any(|job| {
        if !job.snapshot.is_active() || job.kind.takes_magnet() != kind.takes_magnet() {
            return false;
        }
        if kind.takes_magnet() {
            magnet_info_hash_hex(source).is_some_and(|hash| {
                magnet_info_hash_hex(&job.source).as_deref() == Some(hash.as_str())
            })
        } else {
            job.source == source
        }
    });
    conflict.then(|| {
        if kind.takes_magnet() {
            "that magnet is already downloading".into()
        } else {
            "that URL is already downloading".into()
        }
    })
}

/// A provider torrent a job leaves behind.
struct LeftRemote {
    account: String,
    service: Service,
    remote_id: String,
}

/// The provider torrent a re-route would leave behind and that is worth asking
/// about: torrenter added it, the job is not done, and `route` leaves its
/// account (another account of the same provider counts as leaving).
fn left_remote(job: &Job, route: &Route) -> Option<LeftRemote> {
    let existing = job.debrid.as_ref()?;
    let staying = match route {
        Route::Keep => true,
        Route::Via(account) => *account == existing.account,
        Route::Peers => false,
    };
    if staying || job.snapshot.is_done() {
        return None;
    }
    let state = existing.view();
    if !state.added_by_us {
        return None;
    }
    Some(LeftRemote {
        account: existing.account.clone(),
        service: existing.service,
        remote_id: state.remote_id?,
    })
}

/// Delete a torrent from a provider the user said yes to. Without the
/// account's key (it was removed since) nothing can be deleted, so say so
/// instead of dropping it.
fn delete_remote_cmd(inner: &Inner, left: LeftRemote) -> EngineCmd {
    // Exactly the account the torrent lives on: never the provider fallback,
    // whose key belongs to a different account and torrent list.
    match exact_account(inner, &left.account) {
        Some((service, key)) => EngineCmd::DeleteRemote {
            service,
            key,
            remote_id: left.remote_id,
            base: inner.debrid_bases.get(&service).cloned(),
        },
        None => EngineCmd::Notice(format!(
            "could not remove it from {}: no API key for that account any more; it is still on the account",
            left.service.label()
        )),
    }
}

/// An account's provider and key for running a job: the exact account, or
/// for an id saved before accounts its provider's only account.
fn resolve_account(inner: &Inner, id: &str) -> Option<(Service, Secret)> {
    exact_account(inner, id).or_else(|| legacy_account(inner, id))
}

/// Only the account with this id: memory first (mobile keychains, tests),
/// then on desktop `credentials.json` and the env accounts.
fn exact_account(inner: &Inner, id: &str) -> Option<(Service, Secret)> {
    if let Some((service, key)) = inner.keys.get(id) {
        return Some((*service, key.clone()));
    }
    if inner.desktop_keys {
        return crate::credentials::account_key(id);
    }
    None
}

/// Jobs and routes saved before accounts name the provider (`torbox`). With
/// no account of that id, use the provider's only account (saved, env or in
/// memory), as the CLI and MCP resolve a provider name.
fn legacy_account(inner: &Inner, id: &str) -> Option<(Service, Secret)> {
    let service = Service::parse(id).filter(|service| service.id() == id)?;
    let mut memory = inner.keys.values().filter(|(each, _)| *each == service);
    if let Some((_, key)) = memory.next() {
        return memory.next().is_none().then(|| (service, key.clone()));
    }
    if !inner.desktop_keys {
        return None;
    }
    let accounts: Vec<_> = crate::credentials::accounts()
        .into_iter()
        .filter(|account| account.service == service)
        .collect();
    match accounts.as_slice() {
        [only] => crate::credentials::account_key(&only.id),
        _ => None,
    }
}

/// The provider of an account a new route names, or why it cannot be used.
fn account_service(inner: &Inner, id: &str) -> Result<Service, String> {
    if let Some((service, _)) = resolve_account(inner, id) {
        return offered(service).map(|()| service);
    }
    Err(match Service::parse(id) {
        Some(service) => format!("add a {} API key in settings first", service.label()),
        None => "that debrid account's key is not available (removed, or not readable right now); check it in settings".into(),
    })
}

/// The engine command for a job. Debrid jobs need a key for their provider.
/// Only providers this build ships run, whatever a saved job, env key or
/// hand-edited account names.
fn offered(service: Service) -> Result<(), String> {
    if Service::offered().contains(&service) {
        Ok(())
    } else {
        Err(format!(
            "{} is not available in this build",
            service.label()
        ))
    }
}

/// A torrent job's files, grouped by parent folder (root files first, then
/// folders in torrent order). Padding files are left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFiles {
    pub id: u64,
    pub groups: Vec<FileGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    /// Parent path inside the torrent (`/`-separated); empty for root files.
    pub folder: String,
    pub files: Vec<FileRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// The id `set_files` takes.
    pub id: u32,
    pub name: String,
    pub size: u64,
    /// Bytes on disk (live, or as last saved).
    pub have: u64,
    pub selected: bool,
}

fn group_files(
    list: &[torrent_file::FileEntry],
    state: &crate::progress::FileState,
) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = vec![FileGroup {
        folder: String::new(),
        files: Vec::new(),
    }];
    let mut by_folder: HashMap<String, usize> = HashMap::from([(String::new(), 0)]);
    for entry in list.iter().filter(|entry| !entry.padding) {
        let (folder, name) = match entry.path.rsplit_once('/') {
            Some((folder, name)) => (folder.to_string(), name.to_string()),
            None => (String::new(), entry.path.clone()),
        };
        let row = FileRow {
            id: u32::try_from(entry.index).unwrap_or(u32::MAX),
            name,
            size: entry.len,
            have: state
                .have
                .get(entry.index)
                .copied()
                .unwrap_or(0)
                .min(entry.len),
            selected: state.is_selected(entry.index),
        };
        match by_folder.get(&folder) {
            Some(&index) => groups[index].files.push(row),
            None => {
                by_folder.insert(folder.clone(), groups.len());
                groups.push(FileGroup {
                    folder,
                    files: vec![row],
                });
            }
        }
    }
    groups.retain(|group| !group.files.is_empty());
    groups
}

/// Check `ids` against the torrent's real files; all of them = `None`.
fn normalize_selection(
    list: &[torrent_file::FileEntry],
    ids: &[u32],
) -> Result<Option<Vec<usize>>, String> {
    let real: HashSet<usize> = list
        .iter()
        .filter(|entry| !entry.padding)
        .map(|entry| entry.index)
        .collect();
    let mut chosen = std::collections::BTreeSet::new();
    for &id in ids {
        let index = id as usize;
        if !real.contains(&index) {
            return Err(format!("this torrent has no file {id}"));
        }
        chosen.insert(index);
    }
    Ok((chosen.len() != real.len()).then(|| chosen.into_iter().collect()))
}

/// Whether `new` selects a file `old` did not (both sorted; `None` = all).
fn adds_files(old: Option<&Vec<usize>>, new: Option<&Vec<usize>>) -> bool {
    match (old, new) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(old), Some(new)) => new.iter().any(|index| old.binary_search(index).is_err()),
    }
}

/// Fill a restored magnet job's file list from its stored `.torrent` copy.
fn load_file_list(inner: &Inner, source: &str, snapshot: &JobSnapshot) {
    let store = torrent_file::store_dir(&inner.jobs_path);
    let Some(bytes) =
        magnet_info_hash_hex(source).and_then(|hash| torrent_file::load(&store, &hash))
    else {
        return;
    };
    if let Ok(list) = torrent_file::files(&bytes) {
        snapshot.files().set_list(Arc::new(list));
    }
}

const PRIVATE_COPY_MISSING: &str =
    "this private torrent's .torrent copy is missing; add the file again";

/// The `.torrent` bytes a magnet job starts from: its stored copy when there
/// is one. A public torrent falls back to the magnet (`None`); a private one
/// never does, since resolving it from the magnet would ask the DHT.
fn torrent_for_launch(
    store: &std::path::Path,
    source: &str,
    private_torrent: bool,
) -> Result<Option<Vec<u8>>, &'static str> {
    let torrent = magnet_info_hash_hex(source).and_then(|hash| torrent_file::load(store, &hash));
    if private_torrent && torrent.is_none() {
        return Err(PRIVATE_COPY_MISSING);
    }
    Ok(torrent)
}

const PRIVATE_NO_DEBRID: &str =
    "a private torrent downloads only from its own tracker's peers, not through a debrid service";

fn launch(inner: &Inner, job: &Job) -> Result<EngineCmd, String> {
    let debrid = match &job.debrid {
        Some(debrid) => {
            offered(debrid.service)?;
            let (_, key) = resolve_account(inner, &debrid.account).ok_or_else(|| {
                if debrid.account == debrid.service.id() {
                    format!("add a {} API key in settings first", debrid.service.label())
                } else {
                    format!(
                        "this job's {} account key is not available (removed, or not readable right now); check it in settings or download it as a torrent",
                        debrid.service.label()
                    )
                }
            })?;
            Some(DebridLaunch {
                service: debrid.service,
                key,
                state: Arc::clone(&debrid.state),
                remove_after: inner.debrid_remove_after,
                base: inner.debrid_bases.get(&debrid.service).cloned(),
            })
        }
        None => None,
    };
    Ok(EngineCmd::Download {
        id: job.id,
        kind: job.kind,
        source: job.source.clone(),
        snapshot: Arc::clone(&job.snapshot),
        seed: job.seed && !inner.leech_only,
        dest: job.dest.clone(),
        part_count: job.part_count,
        debrid,
        torrent_store: torrent_file::store_dir(&inner.jobs_path),
        private_torrent: job.private_torrent,
    })
}

/// Info hashes (hex) of every magnet job, saved or foreign: the `.torrent`
/// copies to keep.
fn torrent_keep(inner: &Inner) -> HashSet<String> {
    let foreign = inner
        .foreign
        .iter()
        .filter_map(|record| record.get("source")?.as_str());
    inner
        .jobs
        .iter()
        .filter(|job| job.kind.takes_magnet())
        .map(|job| job.source.as_str())
        .chain(foreign)
        .filter_map(magnet_info_hash_hex)
        .collect()
}

/// Drop `.torrent` copies no job refers to any more.
fn prune_torrents(inner: &Inner) {
    torrent_file::prune(
        &torrent_file::store_dir(&inner.jobs_path),
        &torrent_keep(inner),
    );
}

fn persist_inner(inner: &mut Inner, force: bool) {
    let fingerprint = persist_fingerprint(&inner.jobs);
    if fingerprint == inner.saved_fingerprint {
        return;
    }
    if !force && inner.persist_at.elapsed() < PERSIST_EVERY {
        return;
    }
    let records: Vec<_> = inner.jobs.iter().map(persist_record).collect();
    if jobs::save_job_list_to(&inner.jobs_path, &records, &inner.foreign).is_ok() {
        inner.saved_fingerprint = fingerprint;
        inner.persist_at = Instant::now();
    }
}

fn engine_loop(rx: Receiver<EngineCmd>, tx: Sender<EngineEvent>) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let limiter = Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_DOWNLOADS));
    let _keep_alive = thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });
    while let Ok(cmd) = rx.recv() {
        let tx = tx.clone();
        let limiter = Arc::clone(&limiter);
        handle.spawn(async move {
            match cmd {
                EngineCmd::Download {
                    id,
                    kind,
                    source,
                    snapshot,
                    seed,
                    dest,
                    part_count,
                    debrid,
                    torrent_store,
                    private_torrent,
                } => {
                    if snapshot.should_stop() {
                        snapshot.mark_stopped();
                        return;
                    }
                    if !snapshot.is_paused() {
                        snapshot.set_status("queued");
                    }
                    let permit = tokio::select! {
                        permit = limiter.acquire_owned() => permit.ok(),
                        _ = until_stop(Some(snapshot.as_ref())) => {
                            snapshot.mark_stopped();
                            return;
                        }
                    };
                    let Some(_permit) = permit else {
                        return;
                    };
                    if snapshot.should_stop() {
                        snapshot.mark_stopped();
                        return;
                    }
                    if !snapshot.is_paused() {
                        snapshot.set_status("starting");
                    }
                    let result = match (kind, debrid) {
                        (JobKind::Debrid, Some(launch)) => {
                            let run = DebridRun {
                                service: launch.service,
                                key: launch.key,
                                magnet: source,
                                dest,
                                part_count,
                                remove_after: launch.remove_after,
                                state: launch.state,
                                base: launch.base,
                                stall_timeout: crate::debrid::run::STALL_TIMEOUT,
                            };
                            start_debrid_reported(run, snapshot).await
                        }
                        (JobKind::Debrid, None) => {
                            Err(anyhow::anyhow!("debrid job without a provider"))
                        }
                        (JobKind::Magnet, _) => {
                            match torrent_for_launch(&torrent_store, &source, private_torrent) {
                                Ok(torrent) => {
                                    let resolved = tx.clone();
                                    let on_metadata: OnMetadata = Box::new(move |bytes| {
                                        let _ = resolved.send(EngineEvent::Resolved(bytes));
                                    });
                                    start_download_reported(
                                        &source,
                                        torrent,
                                        snapshot,
                                        seed,
                                        dest,
                                        Some(on_metadata),
                                    )
                                    .await
                                }
                                Err(error) => {
                                    snapshot.set_error(error);
                                    Err(anyhow::anyhow!(error))
                                }
                            }
                        }
                        (JobKind::Http, _) => {
                            start_url_download_reported(&source, snapshot, dest, part_count).await
                        }
                    };
                    if let Err(error) = result {
                        if !is_stopped(&error) {
                            let _ = tx.send(EngineEvent::DownloadFailed {
                                id,
                                error: format!("{error:#}"),
                            });
                        }
                    }
                }
                EngineCmd::DeleteRemote {
                    service,
                    key,
                    remote_id,
                    base,
                } => {
                    if let Err(error) =
                        crate::debrid::delete_remote(service, &key, &remote_id, base).await
                    {
                        let _ = tx.send(EngineEvent::DownloadFailed {
                            id: 0,
                            error: crate::debrid::redact(
                                &format!("could not remove it from {}: {error}", service.label()),
                                Some(&key),
                            ),
                        });
                    }
                }
                EngineCmd::Notice(error) => {
                    let _ = tx.send(EngineEvent::DownloadFailed { id: 0, error });
                }
                EngineCmd::Probe(specs) => {
                    let results = specs
                        .into_iter()
                        .map(|spec| {
                            let (present, resumable) = match spec.kind {
                                JobKind::Magnet => {
                                    (magnet_payload_present(&spec.dest, &spec.source), false)
                                }
                                // Restored as Start; the runner re-checks every file.
                                JobKind::Debrid => (false, false),
                                JobKind::Http => (
                                    http_payload_present(&spec.dest, &spec.source, &spec.filename),
                                    http_resume_present(&spec.dest, &spec.source),
                                ),
                            };
                            (spec.id, present, resumable)
                        })
                        .collect();
                    let _ = tx.send(EngineEvent::Probed(results));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::HTTP_NOT_RESUMABLE;
    use tempfile::tempdir;

    fn sample_job(title: &str, source: &str, dest: &str) -> Job {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title(title);
        Job {
            id: 1,
            kind: JobKind::Http,
            source: source.into(),
            snapshot,
            added: "2026-10-27".into(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed: false,
            dest: PathBuf::from(dest),
            part_count: 4,
            debrid: None,
            private_torrent: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn tick_job_rates_from_byte_delta() {
        let mut job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        job.last_check = Instant::now() - Duration::from_secs(1);
        job.snapshot.downloaded.store(1024, Ordering::Relaxed);
        job.snapshot.uploaded.store(512, Ordering::Relaxed);
        tick_job_rates(&mut job);
        assert!((job.speed - 1024.0).abs() < 1.0, "{}", job.speed);
        assert!((job.up_speed - 512.0).abs() < 1.0, "{}", job.up_speed);
    }

    #[test]
    fn magnet_speed_follows_the_engine_not_piece_bursts() {
        let mut job = sample_job("Sintel", "magnet:?xt=urn:btih:x", "/tmp/films");
        job.kind = JobKind::Magnet;
        job.snapshot.set_status("downloading");
        job.snapshot
            .download_bps
            .store(7_000_000, Ordering::Relaxed);
        // No verified piece landed this window: still the live speed, not 0.
        job.last_check = Instant::now() - Duration::from_secs(1);
        tick_job_rates(&mut job);
        assert!((job.speed - 7_000_000.0).abs() < 1.0, "{}", job.speed);
        // A whole piece landing does not spike it.
        job.snapshot
            .downloaded
            .store(16 * 1024 * 1024, Ordering::Relaxed);
        job.last_check = Instant::now() - Duration::from_millis(400);
        tick_job_rates(&mut job);
        assert!((job.speed - 7_000_000.0).abs() < 1.0, "{}", job.speed);
        // Paused or stopped: the engine no longer updates it, so 0.
        job.snapshot.toggle_pause();
        job.last_check = Instant::now() - Duration::from_secs(1);
        tick_job_rates(&mut job);
        assert_eq!(job.speed, 0.0);
    }

    #[test]
    fn caps_active_downloads() {
        assert_eq!(MAX_ACTIVE_DOWNLOADS, 4);
    }

    #[test]
    fn persist_record_roundtrips_paused_and_dest() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        job.snapshot.set_paused(true);
        job.snapshot.downloaded.store(40, Ordering::Relaxed);
        let record = persist_record(&job);
        assert_eq!(record.kind, "http");
        assert_eq!(record.dest, PathBuf::from("/tmp/films"));
        assert!(record.paused);
        assert_eq!(record.downloaded, 40);
        assert_eq!(record.after_probe(true, false), RestoreAction::Missing);
        assert_eq!(record.after_probe(true, true), RestoreAction::Start);
        let snapshot = JobSnapshot::default();
        record.apply_to(&snapshot);
        assert!(snapshot.is_paused());
        assert!(snapshot.is_active());
    }

    #[test]
    fn missing_probe_does_not_want_engine() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        job.snapshot.mark_stopped_with(HTTP_NOT_RESUMABLE);
        let record = persist_record(&job);
        assert_eq!(record.after_probe(false, false), RestoreAction::Idle);
        assert!(!record.wants_engine());
    }

    #[test]
    fn duplicate_http_url_is_refused() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        job.snapshot.set_status("queued");
        let note = duplicate_note(&[job], JobKind::Http, "https://example.com/sintel.mkv");
        assert_eq!(note.as_deref(), Some("that URL is already downloading"));
    }

    #[test]
    fn inactive_duplicate_http_is_allowed() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        job.snapshot.mark_finished();
        assert!(duplicate_note(&[job], JobKind::Http, "https://example.com/sintel.mkv").is_none());
    }

    #[test]
    fn hydrate_done_job_from_disk() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let jobs_path = dir.path().join("jobs.json");
        let record = persist_record(&sample_job(
            "Sintel",
            "https://example.com/sintel.mkv",
            dest.to_str().unwrap(),
        ));
        let mut record = record;
        record.status = "done".into();
        record.id = 7;
        jobs::save_jobs_to(&jobs_path, &[record]).unwrap();
        let session = Session::open(dest, jobs_path, true);
        match session.hydrate() {
            Hydrate::Loaded => {}
            Hydrate::Empty | Hydrate::Corrupt(_) => panic!("expected loaded"),
        }
        let jobs = session.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, 7);
        assert!(jobs[0].snapshot.is_done());
    }

    #[test]
    fn queue_rejects_empty_and_invalid() {
        let dir = tempdir().unwrap();
        let session = Session::open(dir.path().to_path_buf(), dir.path().join("jobs.json"), true);
        assert!(session.queue(JobKind::Http, "  ").is_err());
        assert!(session.queue(JobKind::Http, "not-a-url").is_err());
        assert!(session
            .queue(JobKind::Magnet, "magnet:?xt=urn:btih:zz")
            .is_err());
    }

    #[test]
    fn leech_hydrate_strips_share_seed_and_persists() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let jobs_path = dir.path().join("jobs.json");
        let mut job = sample_job(
            "Sintel",
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10",
            dest.to_str().unwrap(),
        );
        job.kind = JobKind::Magnet;
        job.seed = true;
        job.snapshot.set_status("seeding");
        let mut record = persist_record(&job);
        record.id = 3;
        jobs::save_jobs_to(&jobs_path, &[record]).unwrap();
        let session = Session::open_leech(dest, jobs_path.clone());
        match session.hydrate() {
            Hydrate::Loaded => {}
            Hydrate::Empty | Hydrate::Corrupt(_) => panic!("expected loaded"),
        }
        let jobs = session.jobs();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].seed, "leech session must not keep GUI share seed");
        match jobs::load_jobs_from(&jobs_path) {
            jobs::LoadJobs::Loaded(list) => {
                assert_eq!(list.jobs.len(), 1);
                assert!(!list.jobs[0].seed);
            }
            jobs::LoadJobs::Empty | jobs::LoadJobs::Corrupt(_) => panic!("expected rewritten jobs"),
        }
    }

    #[test]
    fn restart_marks_previous_snapshot_stopped() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let jobs_path = dir.path().join("jobs.json");
        let job = sample_job(
            "Sintel",
            "https://127.0.0.1:1/sintel.mkv",
            dest.to_str().unwrap(),
        );
        job.snapshot.set_status("queued");
        let mut record = persist_record(&job);
        record.id = 1;
        jobs::save_jobs_to(&jobs_path, &[record]).unwrap();
        let session = Session::open(dest.clone(), jobs_path, false);
        match session.hydrate() {
            Hydrate::Loaded => {}
            Hydrate::Empty | Hydrate::Corrupt(_) => panic!("expected loaded"),
        }
        let old = Arc::clone(&session.jobs()[0].snapshot);
        assert!(old.is_active());
        session.restart(1, false, dest).expect("restart");
        assert!(old.is_stopped());
        assert_eq!(session.jobs()[0].id, 2);
        assert!(session.jobs()[0].snapshot.is_active());
    }

    #[test]
    fn remove_marks_snapshot_stopped() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let jobs_path = dir.path().join("jobs.json");
        let job = sample_job(
            "Sintel",
            "https://127.0.0.1:1/sintel.mkv",
            dest.to_str().unwrap(),
        );
        job.snapshot.set_status("queued");
        let mut record = persist_record(&job);
        record.id = 9;
        jobs::save_jobs_to(&jobs_path, &[record]).unwrap();
        let session = Session::open(dest, jobs_path, false);
        match session.hydrate() {
            Hydrate::Loaded => {}
            Hydrate::Empty | Hydrate::Corrupt(_) => panic!("expected loaded"),
        }
        let snap = Arc::clone(&session.jobs()[0].snapshot);
        session.remove(9);
        assert!(session.jobs().is_empty());
        assert!(snap.is_stopped());
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting");
    }

    /// A magnet of its own per test (`tag` repeated as the hex info hash).
    /// The folder lock is process-wide per info hash, and a job whose test
    /// has ended keeps retrying its dropped fake server for ~45 s while it
    /// holds that lock, so tests that share a hash stall each other (seen as
    /// "timed out waiting" in restart_keeps_the_remote_or_switches_to_peers).
    fn own_magnet(tag: u8) -> String {
        format!(
            "magnet:?xt=urn:btih:{}&dn=Album",
            format!("{tag:02x}").repeat(20)
        )
    }

    async fn torbox_session(
        dir: &std::path::Path,
        fetch_polls: u32,
    ) -> (
        Session,
        crate::debrid::fake::FakeServer,
        Arc<Mutex<crate::debrid::torbox::tests::State>>,
    ) {
        use crate::debrid::torbox::tests::{torbox_fake, State, KEY};
        let fake = Arc::new(Mutex::new(State {
            fetch_polls,
            ..State::default()
        }));
        let (server, base) = torbox_fake(Arc::clone(&fake)).await;
        let session = Session::open(dir.join("dl"), dir.join("jobs.json"), true);
        session.use_memory_keys();
        session.set_debrid_base(Service::TorBox, base);
        session.set_debrid_key(Service::TorBox, Secret::new(KEY));
        (session, server, fake)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn debrid_job_completes_and_persists_ids_not_secrets() {
        use crate::debrid::torbox::tests::KEY;
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 0).await;
        session.set_connections(Some(8));
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa1), Some("torbox"))
            .unwrap();
        let job = session.job(id).unwrap();
        assert_eq!(job.kind, JobKind::Debrid);
        assert!(!job.seed, "debrid jobs never seed");
        assert_eq!(job.part_count, 8);
        tokio::task::spawn_blocking(move || {
            wait_for(|| session.job(id).unwrap().snapshot.is_done());
            session.persist(true);
            let job = session.job(id).unwrap();
            let state = job.debrid.as_ref().unwrap().view();
            assert_eq!(state.files.len(), 2);
            assert!(state.files.iter().all(|file| file.done));
        })
        .await
        .unwrap();
        let text = std::fs::read_to_string(dir.path().join("jobs.json")).unwrap();
        assert!(text.contains("\"kind\": \"debrid\""), "{text}");
        assert!(text.contains("\"service\": \"torbox\""));
        assert!(text.contains("\"remoteId\": \"1\""));
        assert!(text.contains("\"partCount\": 8"));
        assert!(
            !text.contains(KEY) && !text.contains("/dld/"),
            "no key or link persisted"
        );
        assert!(dir.path().join("dl/Album/CD1/01.bin").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn torrent_files_queue_as_their_magnet_and_keep_a_copy() {
        use crate::torrent_file::tests::torrent_bytes;
        let dir = tempdir().unwrap();
        // Through the fake TorBox so nothing reaches the real swarm; it keeps
        // the job fetching, so the duplicate check sees it active.
        let (session, _server, _fake) = torbox_session(dir.path(), 10_000).await;
        let bytes = torrent_bytes("Sintel 4K.mkv", &["udp://tracker.example:1337/announce"]);
        let imported = torrent_file::parse(&bytes).unwrap();
        let store = dir.path().join("torrents");
        let copy = store.join(format!("{}.torrent", imported.hash_hex));

        assert_eq!(
            session.queue_torrent(b"not a torrent", None).unwrap_err(),
            "not a valid .torrent file"
        );
        let id = session.queue_torrent(&bytes, Some("torbox")).unwrap();
        let job = session.job(id).unwrap();
        assert_eq!(job.source, imported.magnet);
        assert_eq!(job.snapshot.title(), "Sintel 4K.mkv");
        assert_eq!(std::fs::read(&copy).unwrap(), bytes);
        // The engine is told where the copy lives.
        match launch(&mutex_lock(&session.inner), &job) {
            Ok(EngineCmd::Download { torrent_store, .. }) => assert_eq!(torrent_store, store),
            _ => panic!("expected a download command"),
        }
        // The same file again, or its magnet pasted: one job per torrent, and
        // the copy stays for the job already running.
        assert_eq!(
            session.queue_torrent(&bytes, None).unwrap_err(),
            "that magnet is already downloading"
        );
        assert_eq!(
            session
                .queue(JobKind::Magnet, &imported.magnet)
                .unwrap_err(),
            "that magnet is already downloading"
        );
        assert!(copy.exists());
        // jobs.json keeps the built magnet, not the file.
        session.persist(true);
        let saved = std::fs::read_to_string(dir.path().join("jobs.json")).unwrap();
        assert!(saved.contains(&imported.hash_hex), "{saved}");
        // Clearing the job drops the copy.
        session.stop(id);
        session.remove(id);
        assert!(!copy.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn private_torrents_never_leave_their_tracker() {
        use crate::torrent_file::tests::{private_torrent_bytes, torrent_bytes};
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 10_000).await;
        let store = dir.path().join("torrents");
        let bytes = private_torrent_bytes("Private.mkv", &["https://t.example/a?passkey=S"]);
        let imported = torrent_file::parse(&bytes).unwrap();
        // Not through debrid, and nothing kept for a refused file.
        assert_eq!(
            session.queue_torrent(&bytes, Some("torbox")).unwrap_err(),
            PRIVATE_NO_DEBRID
        );
        assert!(!store
            .join(format!("{}.torrent", imported.hash_hex))
            .exists());

        // The engine starts from the copy; without it a public torrent falls
        // back to its magnet, a private one refuses (no DHT lookup).
        let public = torrent_file::parse(&torrent_bytes("Public.mkv", &[])).unwrap();
        assert_eq!(torrent_for_launch(&store, &public.magnet, false), Ok(None));
        assert_eq!(
            torrent_for_launch(&store, &imported.magnet, true),
            Err(PRIVATE_COPY_MISSING)
        );
        torrent_file::save(&store, &imported, &bytes).unwrap();
        assert_eq!(
            torrent_for_launch(&store, &imported.magnet, true),
            Ok(Some(bytes.clone()))
        );

        // A private job keeps its flag in jobs.json and cannot be re-routed
        // to a debrid account.
        let mut job = sample_job("Private.mkv", &imported.magnet, "/tmp/films");
        job.id = 77;
        job.kind = JobKind::Magnet;
        job.private_torrent = true;
        assert!(persist_record(&job).private_torrent);
        let json = serde_json::to_string(&persist_record(&job)).unwrap();
        assert!(json.contains("\"privateTorrent\":true"), "{json}");
        mutex_lock(&session.inner).jobs.push(job);
        assert_eq!(
            session
                .restart_with(
                    77,
                    Route::Via("torbox".into()),
                    false,
                    PathBuf::from("/tmp/films"),
                    false
                )
                .unwrap_err(),
            PRIVATE_NO_DEBRID
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn debrid_routes_share_duplicate_checks_and_need_a_key() {
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa4), Some("torbox"))
            .unwrap();
        // Same torrent (base32 spelling) as a plain magnet: one job per torrent.
        let b32 = "usskjjfeusskjjfeusskjjfeusskjjfe";
        assert_eq!(crate::torrent::normalize_info_hash(b32), "a4".repeat(20));
        let plain = format!("magnet:?xt=urn:btih:{b32}");
        assert_eq!(
            session.queue(JobKind::Magnet, &plain).unwrap_err(),
            "that magnet is already downloading"
        );
        session.set_debrid_key(Service::TorBox, None);
        let other = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111";
        assert_eq!(
            session
                .queue_with(JobKind::Magnet, other, Some("torbox"))
                .unwrap_err(),
            "add a TorBox API key in settings first"
        );
        assert!(session
            .queue_with(JobKind::Http, "https://example.com/a", Some("torbox"))
            .is_err());
        session.stop(id);
        assert!(session.job(id).unwrap().snapshot.is_stopped());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keeps_the_remote_or_switches_to_peers() {
        use crate::debrid::torbox::tests::KEY;
        let dir = tempdir().unwrap();
        let (session, _server, fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa5), Some("torbox"))
            .unwrap();
        let session = tokio::task::spawn_blocking(move || {
            wait_for(|| {
                session
                    .job(id)
                    .unwrap()
                    .debrid
                    .unwrap()
                    .view()
                    .remote_id
                    .is_some()
            });
            session
        })
        .await
        .unwrap();
        session.stop(id);
        let kept = session.restart(id, false, dir.path().join("dl")).unwrap();
        let job = session.job(kept).unwrap();
        assert_eq!(job.kind, JobKind::Debrid);
        assert_eq!(
            job.debrid.as_ref().unwrap().view().remote_id.as_deref(),
            Some("1"),
            "same provider resumes the same remote torrent"
        );
        session.stop(kept);
        // A restart that cannot launch (key removed) still uses up its id, so
        // the next job never shares it.
        session.set_debrid_key(Service::TorBox, None);
        assert!(session.restart(kept, false, dir.path().join("dl")).is_err());
        let failed = session.jobs()[0].id;
        assert_ne!(failed, kept);
        session.set_debrid_key(Service::TorBox, Secret::new(KEY));
        let other_magnet = "magnet:?xt=urn:btih:2222222222222222222222222222222222222222";
        let other = session
            .queue_with(JobKind::Magnet, other_magnet, Some("torbox"))
            .unwrap();
        assert_ne!(other, failed, "ids stay unique after a failed restart");
        let dest = dir.path().join("dl");
        // Nothing to ask while staying on the service.
        assert!(session.reroute_asks_about_remote(failed, &Route::Peers));
        assert!(!session.reroute_asks_about_remote(failed, &Route::Keep));
        assert!(!session.reroute_asks_about_remote(failed, &Route::Via("torbox".into())));
        // "as torrent", answer no: same job from peers (seed stays off in
        // tests); the torrent stays on the account.
        let peers = session
            .restart_with(failed, Route::Peers, false, dest.clone(), false)
            .unwrap();
        let job = session.job(peers).unwrap();
        assert_eq!(job.kind, JobKind::Magnet);
        assert!(job.debrid.is_none());
        session.stop(peers);
        assert!(
            !session.reroute_asks_about_remote(peers, &Route::Peers),
            "a peers job has nothing on a service"
        );
        // "as torrent", answer yes: the torrent torrenter added is removed.
        let session = tokio::task::spawn_blocking(move || {
            wait_for(|| {
                session
                    .job(other)
                    .unwrap()
                    .debrid
                    .unwrap()
                    .view()
                    .remote_id
                    .is_some()
            });
            session
        })
        .await
        .unwrap();
        session.stop(other);
        assert!(session.reroute_asks_about_remote(other, &Route::Peers));
        let other_peers = session
            .restart_with(other, Route::Peers, false, dest, true)
            .unwrap();
        session.stop(other_peers);
        assert_eq!(session.jobs().len(), 2);
        tokio::task::spawn_blocking(move || {
            wait_for(|| !fake.lock().unwrap().torrents.contains_key(&2));
            assert!(
                fake.lock().unwrap().torrents.contains_key(&1),
                "the job answered no keeps its torrent"
            );
        })
        .await
        .unwrap();
    }

    #[test]
    fn verbatim_windows_folders_come_back_plain() {
        let dir = tempdir().unwrap();
        let jobs_path = dir.path().join("jobs.json");
        std::fs::write(
            &jobs_path,
            r#"{"jobs":[{"id":3,"kind":"magnet","source":"magnet:?xt=urn:btih:4444444444444444444444444444444444444444","dest":"\\\\?\\E:\\my\\folder","status":"stopped"}]}"#,
        )
        .unwrap();
        let session = Session::open(PathBuf::from(r"\\?\E:\dl"), jobs_path.clone(), false);
        assert_eq!(session.dest(), PathBuf::from(r"E:\dl"));
        session.hydrate();
        let job = session.job(3).unwrap();
        assert_eq!(
            job.dest,
            PathBuf::from(r"E:\my\folder"),
            "restart shows the plain folder"
        );
        session.set_dest(PathBuf::from(r"\\?\UNC\nas\share"));
        assert_eq!(session.dest(), PathBuf::from(r"\\nas\share"));
        // Loading rewrote jobs.json with the plain folder.
        let saved = std::fs::read_to_string(&jobs_path).unwrap();
        assert!(!saved.contains(r"\\\\?"), "{saved}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removal_never_borrows_another_accounts_key() {
        use crate::debrid::torbox::tests::KEY;
        let dir = tempdir().unwrap();
        let (session, _server, fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa6), Some("torbox"))
            .unwrap();
        tokio::task::spawn_blocking(move || {
            wait_for(|| {
                session
                    .job(id)
                    .unwrap()
                    .debrid
                    .unwrap()
                    .view()
                    .remote_id
                    .is_some()
            });
            session.stop(id);
            // The job's own account goes; another TorBox account remains. It
            // could run a legacy job, but must never delete for this one.
            session.set_debrid_key(Service::TorBox, None);
            session.set_debrid_account("torbox-other", Service::TorBox, Secret::new(KEY));
            assert!(session.clear_asks_about_remote(id));
            session.remove_with_remote(id);
            let mut notices = Vec::new();
            wait_for(|| {
                notices.extend(session.drain_events().into_iter().filter_map(
                    |event| match event {
                        SessionEvent::DownloadFailed { id: 0, error } => Some(error),
                        _ => None,
                    },
                ));
                !notices.is_empty()
            });
            assert!(
                notices[0].contains("no API key for that account"),
                "{notices:?}"
            );
            assert!(
                fake.lock().unwrap().torrents.contains_key(&1),
                "no delete reached the provider"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removal_without_a_key_says_it_was_kept() {
        let dir = tempdir().unwrap();
        let (session, _server, fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa7), Some("torbox"))
            .unwrap();
        tokio::task::spawn_blocking(move || {
            wait_for(|| {
                session
                    .job(id)
                    .unwrap()
                    .debrid
                    .unwrap()
                    .view()
                    .remote_id
                    .is_some()
            });
            session.stop(id);
            session.set_debrid_key(Service::TorBox, None);
            assert!(session.reroute_asks_about_remote(id, &Route::Peers));
            let peers = session
                .restart_with(id, Route::Peers, false, dir.path().join("dl"), true)
                .unwrap();
            session.stop(peers);
            let mut notices = Vec::new();
            wait_for(|| {
                notices.extend(session.drain_events().into_iter().filter_map(
                    |event| match event {
                        SessionEvent::DownloadFailed { id: 0, error } => Some(error),
                        _ => None,
                    },
                ));
                !notices.is_empty()
            });
            assert!(
                notices[0].contains("no API key for that account"),
                "{notices:?}"
            );
            assert!(fake.lock().unwrap().torrents.contains_key(&1));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_an_unfinished_job_can_delete_it_from_the_provider() {
        let dir = tempdir().unwrap();
        let (session, _server, fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &own_magnet(0xa8), Some("torbox"))
            .unwrap();
        let session = tokio::task::spawn_blocking(move || {
            wait_for(|| {
                session
                    .job(id)
                    .unwrap()
                    .debrid
                    .unwrap()
                    .view()
                    .remote_id
                    .is_some()
            });
            session
        })
        .await
        .unwrap();
        session.stop(id);
        assert!(session.clear_asks_about_remote(id));
        session.remove_with_remote(id);
        assert!(session.jobs().is_empty());
        let fake = Arc::clone(&fake);
        tokio::task::spawn_blocking(move || {
            wait_for(|| fake.lock().unwrap().torrents.is_empty());
        })
        .await
        .unwrap();
    }

    #[test]
    fn restored_debrid_jobs_keep_their_provider_state() {
        let dir = tempdir().unwrap();
        let jobs_path = dir.path().join("jobs.json");
        let record = PersistedJob {
            id: 4,
            kind: "debrid".into(),
            source: "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056".into(),
            dest: dir.path().to_path_buf(),
            status: "stopped".into(),
            part_count: Some(8),
            service: Some("torbox".into()),
            remote_id: Some("77".into()),
            added_by_us: true,
            files: vec![jobs::PersistedFile {
                id: "0".into(),
                path: "a.bin".into(),
                size: 5,
                done: true,
            }],
            ..PersistedJob::default()
        };
        jobs::save_jobs_to(&jobs_path, &[record]).unwrap();
        let session = Session::open(dir.path().to_path_buf(), jobs_path.clone(), true);
        session.use_memory_keys();
        assert!(matches!(session.hydrate(), Hydrate::Loaded));
        let job = session.job(4).unwrap();
        assert_eq!(job.kind, JobKind::Debrid);
        assert_eq!(job.part_count, 8);
        let state = job.debrid.as_ref().unwrap().view();
        assert_eq!(state.remote_id.as_deref(), Some("77"));
        assert!(state.added_by_us && state.files[0].done);
        session.persist(true);
        match jobs::load_jobs_from(&jobs_path) {
            jobs::LoadJobs::Loaded(list) => {
                assert_eq!(list.jobs[0].remote_id.as_deref(), Some("77"));
                assert_eq!(list.jobs[0].files.len(), 1);
            }
            _ => panic!("expected jobs"),
        }
    }

    /// A session whose engine is the test: commands land in the receiver and
    /// events come from the sender (no librqbit, no network).
    fn detached(dir: &std::path::Path) -> (Session, Receiver<EngineCmd>, Sender<EngineEvent>) {
        let (engine_tx, engine_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let dest = dir.join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let session = Session::with_channels(
            dest,
            dir.join("jobs.json"),
            false,
            false,
            engine_tx,
            event_rx,
        );
        (session, engine_rx, event_tx)
    }

    fn season_pack() -> Vec<u8> {
        crate::torrent_file::tests::multi_torrent_bytes(
            "Show",
            &[
                ("readme.nfo", 3, false),
                ("Season 1/E01.mkv", 10, false),
                (".pad/6", 6, true),
                ("Season 1/E02.mkv", 12, false),
                ("Season 2/E01.mkv", 8, false),
            ],
        )
    }

    /// One restored magnet job (id 5) for `bytes`, its copy stored.
    fn restored_pack(
        dir: &std::path::Path,
        bytes: &[u8],
        status: &str,
    ) -> (Session, Receiver<EngineCmd>, Sender<EngineEvent>) {
        let imported = torrent_file::parse(bytes).unwrap();
        torrent_file::save(&dir.join("torrents"), &imported, bytes).unwrap();
        let mut record = PersistedJob {
            id: 5,
            kind: "magnet".into(),
            source: imported.magnet.clone(),
            dest: dir.join("dl"),
            added: "2026-11-01".into(),
            title: "Show".into(),
            status: status.into(),
            ..Default::default()
        };
        record
            .extra
            .insert("futureKey".into(), serde_json::json!({"kept": true}));
        jobs::save_jobs_to(&dir.join("jobs.json"), &[record]).unwrap();
        let (session, rx, tx) = detached(dir);
        assert!(matches!(session.hydrate(), Hydrate::Loaded));
        (session, rx, tx)
    }

    fn downloads(rx: &Receiver<EngineCmd>) -> usize {
        rx.try_iter()
            .filter(|cmd| matches!(cmd, EngineCmd::Download { .. }))
            .count()
    }

    #[test]
    fn files_are_grouped_and_a_stopped_job_keeps_its_choice() {
        let dir = tempdir().unwrap();
        let (session, rx, _tx) = restored_pack(dir.path(), &season_pack(), "stopped");
        let files = session.job_files(5).expect("list from the stored copy");
        let shape: Vec<(String, Vec<(u32, String)>)> = files
            .groups
            .iter()
            .map(|group| {
                (
                    group.folder.clone(),
                    group
                        .files
                        .iter()
                        .map(|file| (file.id, file.name.clone()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (String::new(), vec![(0, "readme.nfo".to_string())]),
                (
                    "Season 1".to_string(),
                    vec![(1, "E01.mkv".to_string()), (3, "E02.mkv".to_string())]
                ),
                ("Season 2".to_string(), vec![(4, "E01.mkv".to_string())]),
            ]
        );
        assert!(files
            .groups
            .iter()
            .flat_map(|g| &g.files)
            .all(|f| f.selected));
        assert_eq!(session.jobs()[0].snapshot.file_counts(), (4, 4));

        // Padding and unknown ids are refused; all files is "no selection".
        assert!(session.set_files(5, &[1, 2]).is_err());
        assert!(session.set_files(5, &[9]).is_err());
        assert_eq!(session.set_files(5, &[4, 3, 1, 0]), Ok(5));
        assert_eq!(session.jobs()[0].snapshot.files().only(), None);

        // A stopped job only stores the choice (no launch) and saves it,
        // keeping what a newer build wrote on the record.
        assert_eq!(session.set_files(5, &[3, 1, 1]), Ok(5));
        assert_eq!(downloads(&rx), 0);
        assert_eq!(session.jobs()[0].snapshot.files().only(), Some(&vec![1, 3]));
        assert_eq!(session.jobs()[0].snapshot.file_counts(), (4, 2));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("jobs.json")).unwrap())
                .unwrap();
        assert_eq!(saved["jobs"][0]["onlyFiles"], serde_json::json!([1, 3]));
        assert_eq!(
            saved["jobs"][0]["futureKey"],
            serde_json::json!({"kept": true})
        );
        let row = &session.job_files(5).unwrap().groups[1].files[0];
        assert!(row.selected);
        assert!(!session.job_files(5).unwrap().groups[0].files[0].selected);

        // Restart keeps the selection (and the newer build's field).
        let id = session.restart(5, false, dir.path().join("dl")).unwrap();
        assert_ne!(id, 5);
        assert_eq!(downloads(&rx), 1);
        let job = &session.jobs()[0];
        assert_eq!(job.snapshot.files().only(), Some(&vec![1, 3]));
        assert!(persist_record(job).extra.contains_key("futureKey"));
    }

    #[test]
    fn ticking_a_file_on_a_done_job_reopens_it_in_place() {
        let dir = tempdir().unwrap();
        let (session, rx, _tx) = restored_pack(dir.path(), &season_pack(), "done");
        assert!(session.jobs()[0].snapshot.is_done());
        // Unticking on a done job only stores it: nothing to download.
        assert_eq!(session.set_files(5, &[0]), Ok(5));
        assert!(session.jobs()[0].snapshot.is_done());
        assert_eq!(downloads(&rx), 0);
        assert_eq!(session.set_files(5, &[]), Ok(5));
        assert!(session.jobs()[0].snapshot.is_done());
        assert_eq!(downloads(&rx), 0);
        // Ticking a file starts it again under the same id and date.
        assert_eq!(session.set_files(5, &[3]), Ok(5));
        let job = session.jobs()[0].clone();
        assert_eq!((job.id, job.added.as_str()), (5, "2026-11-01"));
        assert!(job.snapshot.is_active());
        assert_eq!(job.snapshot.files().only(), Some(&vec![3]));
        assert_eq!(downloads(&rx), 1);
        // The reopened job is running: more ticks are applied by its engine.
        let generation = session.jobs()[0].snapshot.files().generation;
        assert_eq!(session.set_files(5, &[0, 3]), Ok(5));
        assert_eq!(downloads(&rx), 0);
        assert_eq!(
            session.jobs()[0].snapshot.files().generation,
            generation + 1
        );
        // Unchanged selection: nothing to do.
        assert_eq!(session.set_files(5, &[3, 0]), Ok(5));
        assert_eq!(
            session.jobs()[0].snapshot.files().generation,
            generation + 1
        );
    }

    #[test]
    fn only_a_newly_ticked_file_counts_as_adding() {
        assert!(!adds_files(None, Some(&vec![1])));
        assert!(adds_files(Some(&vec![1]), None));
        assert!(adds_files(Some(&vec![1, 3]), Some(&vec![1, 2])));
        assert!(!adds_files(Some(&vec![1, 3]), Some(&vec![3])));
        assert!(!adds_files(Some(&vec![1, 3]), Some(&vec![])));
    }

    #[test]
    fn only_torrent_jobs_choose_files() {
        let dir = tempdir().unwrap();
        let (session, _rx, _tx) = detached(dir.path());
        assert_eq!(session.set_files(1, &[0]).unwrap_err(), "job gone");
        let mut job = sample_job("Sintel", "https://example.com/s.mkv", "/tmp/films");
        job.id = 3;
        mutex_lock(&session.inner).jobs.push(job);
        assert!(session.set_files(3, &[0]).is_err());
        assert!(session.job_files(3).is_none());
        // A magnet whose metadata has not arrived has no list yet.
        let mut magnet = sample_job(
            "M",
            "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056",
            "/tmp/films",
        );
        magnet.id = 4;
        magnet.kind = JobKind::Magnet;
        mutex_lock(&session.inner).jobs.push(magnet);
        assert!(session.job_files(4).is_none());
        assert!(session.set_files(4, &[0]).is_err());
    }

    #[test]
    fn a_resolved_magnet_keeps_one_copy() {
        use crate::torrent_file::tests::private_torrent_bytes;
        let dir = tempdir().unwrap();
        let (session, _rx, tx) = detached(dir.path());
        let store = dir.path().join("torrents");
        let bytes = season_pack();
        let imported = torrent_file::parse(&bytes).unwrap();
        let copy = store.join(format!("{}.torrent", imported.hash_hex));

        // No job for that hash: nothing is kept.
        tx.send(EngineEvent::Resolved(bytes.clone())).unwrap();
        session.drain_events();
        assert!(!copy.exists());

        let mut job = sample_job("Show", &imported.magnet, "/tmp/films");
        job.id = 2;
        job.kind = JobKind::Magnet;
        mutex_lock(&session.inner).jobs.push(job);
        tx.send(EngineEvent::Resolved(bytes.clone())).unwrap();
        session.drain_events();
        assert_eq!(std::fs::read(&copy).unwrap(), bytes);
        assert_eq!(session.job_files(2).unwrap().groups.len(), 3);
        assert!(!session.jobs()[0].private_torrent);
        // The copy is kept for that job.
        prune_torrents(&mutex_lock(&session.inner));
        assert!(copy.exists());

        // An existing copy is never replaced.
        std::fs::write(&copy, b"placeholder that does not parse").unwrap();
        torrent_file::save(&store, &imported, &bytes).unwrap();
        let first = std::fs::read(&copy).unwrap();
        tx.send(EngineEvent::Resolved(bytes.clone())).unwrap();
        session.drain_events();
        assert_eq!(std::fs::read(&copy).unwrap(), first);

        // A magnet that turns out private is marked so.
        let private = private_torrent_bytes("Private.mkv", &[]);
        let hidden = torrent_file::parse(&private).unwrap();
        let mut job = sample_job("P", &hidden.magnet, "/tmp/films");
        job.id = 3;
        job.kind = JobKind::Magnet;
        mutex_lock(&session.inner).jobs.push(job);
        tx.send(EngineEvent::Resolved(private)).unwrap();
        session.drain_events();
        assert!(session
            .jobs()
            .iter()
            .any(|job| job.id == 3 && job.private_torrent));
    }
}

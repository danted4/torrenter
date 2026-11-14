use std::collections::HashMap;
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
use crate::progress::{is_stopped, until_stop, JobSnapshot};
use crate::torrent::{
    magnet_display_name, magnet_info_hash_hex, magnet_payload_present, start_download_reported,
};
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
}

#[derive(Clone)]
pub struct JobDebrid {
    pub service: Service,
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

/// How a restart routes a magnet: as before, from peers, or via a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Keep,
    Peers,
    Via(Service),
}

pub fn persist_record(job: &Job) -> PersistedJob {
    let debrid = job.debrid.as_ref().map(JobDebrid::view);
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
        remote_id: debrid.as_ref().and_then(|d| d.remote_id.clone()),
        added_by_us: debrid.as_ref().is_some_and(|d| d.added_by_us),
        files: debrid.map(|d| d.files).unwrap_or_default(),
        ..Default::default()
    }
}

fn persist_fingerprint(jobs: &[Job]) -> String {
    jobs.iter()
        .map(|job| {
            let record = persist_record(job);
            format!(
                "{}:{}:{}:{}:{}:{}:{:?}:{}:{:?}:{:?}:{}",
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
                record.files.iter().filter(|file| file.done).count()
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
    },
    Probe(Vec<ProbeSpec>),
    DeleteRemote {
        service: Service,
        key: Secret,
        remote_id: String,
        base: Option<Url>,
    },
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
    DownloadFailed { id: u64, error: String },
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
    keys: HashMap<Service, Secret>,
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
        Self {
            inner: Arc::new(Mutex::new(Inner {
                dest,
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
        mutex_lock(&self.inner).dest = dest;
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

    pub fn set_debrid_key(&self, service: Service, key: Option<Secret>) {
        let mut inner = mutex_lock(&self.inner);
        match key {
            Some(key) => {
                inner.keys.insert(service, key);
            }
            None => {
                inner.keys.remove(&service);
            }
        }
    }

    pub fn has_debrid_key(&self, service: Service) -> bool {
        resolve_key(&mutex_lock(&self.inner), service).is_some()
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
                inner.next_id = inner.next_id.max(list.max_id() + 1);
                inner.foreign = list.foreign;
                for record in list.jobs {
                    let Some(kind) = record.kind_id() else {
                        continue;
                    };
                    let snapshot = Arc::new(JobSnapshot::default());
                    record.apply_to(&snapshot);
                    let wants_engine = record.wants_engine();
                    if record.id >= inner.next_id {
                        inner.next_id = record.id + 1;
                    }
                    if wants_engine {
                        probes.push(ProbeSpec {
                            id: record.id,
                            kind,
                            source: record.source.clone(),
                            dest: record.dest.clone(),
                            filename: record.title.clone(),
                        });
                    }
                    let debrid = (kind == JobKind::Debrid)
                        .then(|| record.service.as_deref().and_then(Service::parse))
                        .flatten()
                        .map(|service| JobDebrid {
                            service,
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
                        dest: record.dest,
                        // Jobs saved before the setting existed used 4 parts.
                        part_count: record.part_count.map(usize::from).unwrap_or(PART_COUNT),
                        debrid,
                    });
                }
                if inner.leech_only {
                    inner.saved_fingerprint.clear();
                    persist_inner(&mut inner, true);
                } else {
                    inner.saved_fingerprint = persist_fingerprint(&inner.jobs);
                }
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

    /// Queue a job; a magnet with `via` goes through that debrid provider.
    pub fn queue_with(
        &self,
        kind: JobKind,
        source: &str,
        via: Option<Service>,
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
        if let Some(note) = duplicate_note(&inner.jobs, kind, source) {
            return Err(note);
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
            debrid: via.map(|service| JobDebrid {
                service,
                state: SharedDebridState::default(),
            }),
        };
        let cmd = launch(&inner, &job)?;
        inner.next_id += 1;
        let _ = self.engine.send(cmd);
        inner.jobs.push(job);
        persist_inner(&mut inner, true);
        Ok(id)
    }

    pub fn restart(&self, id: u64, seed: bool, dest: PathBuf) -> Result<u64, String> {
        self.restart_with(id, Route::Keep, seed, dest)
    }

    /// Restart a job, optionally re-routing a magnet (peers ⇄ a provider).
    /// Re-using the same provider keeps its torrent id and finished files, so
    /// the download resumes instead of starting over.
    pub fn restart_with(
        &self,
        id: u64,
        route: Route,
        seed: bool,
        dest: PathBuf,
    ) -> Result<u64, String> {
        let mut inner = mutex_lock(&self.inner);
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            return Err("job gone".into());
        };
        let old = inner.jobs[index].clone();
        let source = old.source.clone();
        let debrid = match (old.kind, route) {
            (JobKind::Http, Route::Via(_)) => return Err("debrid applies to magnet links".into()),
            (_, Route::Peers) => None,
            (_, Route::Via(service)) => Some(match &old.debrid {
                Some(existing) if existing.service == service => JobDebrid {
                    service,
                    state: Arc::new(Mutex::new(existing.view())),
                },
                _ => JobDebrid {
                    service,
                    state: SharedDebridState::default(),
                },
            }),
            (_, Route::Keep) => old.debrid.as_ref().map(|existing| JobDebrid {
                service: existing.service,
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
        let new_id = inner.next_id;
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
            added: today(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed,
            dest,
            part_count,
            debrid,
        };
        let cmd = match launch(&inner, &job) {
            Ok(cmd) => cmd,
            Err(error) => {
                job.snapshot.set_error(error.clone());
                inner.jobs[index] = job;
                persist_inner(&mut inner, true);
                return Err(error);
            }
        };
        inner.next_id += 1;
        let _ = self.engine.send(cmd);
        inner.jobs[index] = job;
        persist_inner(&mut inner, true);
        Ok(new_id)
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
                Some(EngineCmd::DeleteRemote {
                    service: debrid.service,
                    key: resolve_key(&inner, debrid.service)?,
                    remote_id: state.remote_id?,
                    base: inner.debrid_bases.get(&debrid.service).cloned(),
                })
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
    let elapsed = now.saturating_duration_since(job.last_check).as_secs_f64();
    if elapsed >= 0.4 {
        job.speed = (downloaded.saturating_sub(job.last_bytes) as f64) / elapsed;
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

/// Memory first (mobile keychains, tests), then on desktop the env override and
/// `credentials.json`.
fn resolve_key(inner: &Inner, service: Service) -> Option<Secret> {
    if let Some(key) = inner.keys.get(&service) {
        return Some(key.clone());
    }
    if inner.desktop_keys {
        return crate::credentials::key_for(service).map(|(key, _)| key);
    }
    None
}

/// The engine command for a job. Debrid jobs need a key for their provider.
fn launch(inner: &Inner, job: &Job) -> Result<EngineCmd, String> {
    let debrid = match &job.debrid {
        Some(debrid) => {
            let key = resolve_key(inner, debrid.service).ok_or_else(|| {
                format!("add a {} API key in settings first", debrid.service.label())
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
    })
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
                            };
                            start_debrid_reported(run, snapshot).await
                        }
                        (JobKind::Debrid, None) => {
                            Err(anyhow::anyhow!("debrid job without a provider"))
                        }
                        (JobKind::Magnet, _) => {
                            start_download_reported(&source, snapshot, seed, dest).await
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
                            error: format!("could not remove it from {}: {error}", service.label()),
                        });
                    }
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
        use crate::debrid::torbox::tests::{magnet, KEY};
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 0).await;
        session.set_connections(Some(8));
        let id = session
            .queue_with(JobKind::Magnet, &magnet(), Some(Service::TorBox))
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
    async fn debrid_routes_share_duplicate_checks_and_need_a_key() {
        use crate::debrid::torbox::tests::{magnet, HASH};
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &magnet(), Some(Service::TorBox))
            .unwrap();
        // Same torrent (base32 spelling) as a plain magnet: one job per torrent.
        let b32 = "zhqvoy7xelzd5gfctxwn7lrudomnkmcw";
        assert_eq!(crate::torrent::normalize_info_hash(b32), HASH);
        let plain = format!("magnet:?xt=urn:btih:{b32}");
        assert_eq!(
            session.queue(JobKind::Magnet, &plain).unwrap_err(),
            "that magnet is already downloading"
        );
        session.set_debrid_key(Service::TorBox, None);
        let other = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111";
        assert_eq!(
            session
                .queue_with(JobKind::Magnet, other, Some(Service::TorBox))
                .unwrap_err(),
            "add a TorBox API key in settings first"
        );
        assert!(session
            .queue_with(
                JobKind::Http,
                "https://example.com/a",
                Some(Service::TorBox)
            )
            .is_err());
        session.stop(id);
        assert!(session.job(id).unwrap().snapshot.is_stopped());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keeps_the_remote_or_switches_to_peers() {
        use crate::debrid::torbox::tests::{magnet, KEY};
        let dir = tempdir().unwrap();
        let (session, _server, _fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &magnet(), Some(Service::TorBox))
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
        // "as torrent": same job, now from peers (seed stays off in tests).
        session.set_debrid_key(Service::TorBox, Secret::new(KEY));
        let peers = session
            .restart_with(kept, Route::Peers, false, dir.path().join("dl"))
            .unwrap();
        let job = session.job(peers).unwrap();
        assert_eq!(job.kind, JobKind::Magnet);
        assert!(job.debrid.is_none());
        session.stop(peers);
        assert_eq!(session.jobs().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_an_unfinished_job_can_delete_it_from_the_provider() {
        use crate::debrid::torbox::tests::magnet;
        let dir = tempdir().unwrap();
        let (session, _server, fake) = torbox_session(dir.path(), 10_000).await;
        let id = session
            .queue_with(JobKind::Magnet, &magnet(), Some(Service::TorBox))
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
}

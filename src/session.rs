use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::http::{
    filename_from_url, http_payload_present, http_resume_present, start_url_download_reported,
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
}

pub fn persist_record(job: &Job) -> PersistedJob {
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
        ..Default::default()
    }
}

fn persist_fingerprint(jobs: &[Job]) -> String {
    jobs.iter()
        .map(|job| {
            let record = persist_record(job);
            format!(
                "{}:{}:{}:{}:{}:{}:{:?}:{}",
                record.id,
                record.kind,
                record.downloaded,
                record.total,
                record.status,
                record.paused,
                record.error,
                record.title
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
    },
    Probe(Vec<ProbeSpec>),
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
                        seed: record.seed && !leech_only,
                        dest: record.dest,
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
        let leech_only = inner.leech_only;
        let cmds: Vec<_> = starts
            .into_iter()
            .filter_map(|index| {
                let job = inner.jobs.get(index)?;
                Some(EngineCmd::Download {
                    id: job.id,
                    kind: job.kind,
                    source: job.source.clone(),
                    snapshot: Arc::clone(&job.snapshot),
                    seed: job.seed && !leech_only,
                    dest: job.dest.clone(),
                })
            })
            .collect();
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
        let source = source.trim();
        if source.is_empty() {
            return Err("paste a magnet or HTTP URL first".into());
        }
        let valid = match kind {
            JobKind::Magnet => is_valid_magnet_url(source),
            JobKind::Http => is_valid_download_url(source),
        };
        if !valid {
            return Err(format!("invalid {} URL", kind.as_str()));
        }
        let mut inner = mutex_lock(&self.inner);
        if let Some(note) = duplicate_note(&inner.jobs, kind, source) {
            return Err(note);
        }
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        match kind {
            JobKind::Magnet => {
                if let Some(name) = magnet_display_name(source) {
                    snapshot.set_title(name);
                }
            }
            JobKind::Http => snapshot.set_title(filename_from_url(source, None)),
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let seed = kind == JobKind::Magnet && inner.seed && !inner.leech_only;
        let dest = inner.dest.clone();
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
            dest: dest.clone(),
        };
        let _ = self.engine.send(EngineCmd::Download {
            id,
            kind,
            source: source.to_string(),
            snapshot,
            seed,
            dest,
        });
        inner.jobs.push(job);
        persist_inner(&mut inner, true);
        Ok(id)
    }

    pub fn restart(&self, id: u64, seed: bool, dest: PathBuf) -> Result<u64, String> {
        let mut inner = mutex_lock(&self.inner);
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            return Err("job gone".into());
        };
        let kind = inner.jobs[index].kind;
        let source = inner.jobs[index].source.clone();
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
        inner.next_id += 1;
        let seed = kind == JobKind::Magnet && seed && !inner.leech_only;
        let _ = self.engine.send(EngineCmd::Download {
            id: new_id,
            kind,
            source: source.clone(),
            snapshot: Arc::clone(&snapshot),
            seed,
            dest: dest.clone(),
        });
        inner.jobs[index] = Job {
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
        };
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

fn duplicate_note(jobs: &[Job], kind: JobKind, source: &str) -> Option<String> {
    let conflict = jobs.iter().any(|job| {
        if !job.snapshot.is_active() || job.kind != kind {
            return false;
        }
        match kind {
            JobKind::Magnet => magnet_info_hash_hex(source).is_some_and(|hash| {
                magnet_info_hash_hex(&job.source).as_deref() == Some(hash.as_str())
            }),
            JobKind::Http => job.source == source,
        }
    });
    conflict.then(|| match kind {
        JobKind::Magnet => "that magnet is already downloading".into(),
        JobKind::Http => "that URL is already downloading".into(),
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
                    let result = match kind {
                        JobKind::Magnet => {
                            start_download_reported(&source, snapshot, seed, dest).await
                        }
                        JobKind::Http => start_url_download_reported(&source, snapshot, dest).await,
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
                EngineCmd::Probe(specs) => {
                    let results = specs
                        .into_iter()
                        .map(|spec| {
                            let (present, resumable) = match spec.kind {
                                JobKind::Magnet => {
                                    (magnet_payload_present(&spec.dest, &spec.source), false)
                                }
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
}

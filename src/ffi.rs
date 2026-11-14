use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::identity;
use crate::jobs::JobKind;
use crate::session::{self, Job, Session};

#[uniffi::export]
pub fn banner() -> String {
    identity::banner()
}

#[derive(Debug, Clone, uniffi::Error)]
pub enum EngineError {
    Failed { msg: String },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { msg } => f.write_str(msg),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<String> for EngineError {
    fn from(msg: String) -> Self {
        Self::Failed { msg }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Kind {
    Magnet,
    Http,
    /// A magnet routed through a debrid provider.
    Debrid,
}

impl From<JobKind> for Kind {
    fn from(kind: JobKind) -> Self {
        match kind {
            JobKind::Magnet => Self::Magnet,
            JobKind::Http => Self::Http,
            JobKind::Debrid => Self::Debrid,
        }
    }
}

impl From<Kind> for JobKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Magnet => Self::Magnet,
            Kind::Http => Self::Http,
            Kind::Debrid => Self::Debrid,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct JobView {
    pub id: u64,
    pub kind: Kind,
    pub source: String,
    pub title: String,
    pub status: String,
    pub downloaded: u64,
    pub total: u64,
    pub uploaded: u64,
    pub upload_bps: u64,
    pub peers: u64,
    pub speed: f64,
    pub up_speed: f64,
    pub seed: bool,
    pub dest: String,
    pub added: String,
    pub paused: bool,
    pub finished: bool,
    pub active: bool,
    pub error: Option<String>,
}

impl JobView {
    fn from_job(job: &Job) -> Self {
        let error = job.snapshot.error();
        Self {
            id: job.id,
            kind: job.kind.into(),
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
        }
    }
}

#[derive(uniffi::Object)]
pub struct Engine {
    session: Session,
}

#[uniffi::export]
impl Engine {
    #[uniffi::constructor]
    pub fn open(dest: String, jobs_path: String, seed: bool) -> Arc<Self> {
        Arc::new(Self {
            session: Session::open(PathBuf::from(dest), PathBuf::from(jobs_path), seed),
        })
    }

    pub fn hydrate(&self) -> Result<(), EngineError> {
        match self.session.hydrate() {
            session::Hydrate::Empty | session::Hydrate::Loaded => Ok(()),
            session::Hydrate::Corrupt(error) => Err(EngineError::Failed {
                msg: format!("could not read saved jobs: {error}"),
            }),
        }
    }

    pub fn persist(&self, force: bool) {
        self.session.persist(force);
    }

    pub fn drain_notices(&self) -> Vec<String> {
        self.session
            .drain_events()
            .into_iter()
            .map(|event| match event {
                session::SessionEvent::RestoredMissing(1) => {
                    "1 saved download is missing from its folder".into()
                }
                session::SessionEvent::RestoredMissing(n) => {
                    format!("{n} saved downloads are missing from their folders")
                }
                session::SessionEvent::DownloadFailed { error, .. } => error,
            })
            .collect()
    }

    pub fn jobs(&self) -> Vec<JobView> {
        self.session.jobs().iter().map(JobView::from_job).collect()
    }

    pub fn queue(&self, kind: Kind, source: String) -> Result<u64, EngineError> {
        self.session.queue(kind.into(), &source).map_err(Into::into)
    }

    pub fn restart(&self, id: u64, seed: bool, dest: String) -> Result<u64, EngineError> {
        self.session
            .restart(id, seed, PathBuf::from(dest))
            .map_err(Into::into)
    }

    pub fn toggle_pause(&self, id: u64) {
        self.session.toggle_pause(id);
    }

    pub fn stop(&self, id: u64) {
        self.session.stop(id);
    }

    pub fn remove(&self, id: u64) {
        self.session.remove(id);
    }

    pub fn set_seed(&self, seed: bool) {
        self.session.set_seed(seed);
    }

    pub fn seed(&self) -> bool {
        self.session.seed()
    }

    pub fn dest(&self) -> String {
        self.session.dest().display().to_string()
    }

    pub fn any_active(&self) -> bool {
        self.session.any_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::JobSnapshot;
    use std::time::Instant;

    fn sample() -> Job {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title("Sintel");
        snapshot.set_status("queued");
        snapshot.downloaded.store(10, Ordering::Relaxed);
        Job {
            id: 3,
            kind: JobKind::Http,
            source: "https://example.com/sintel.mkv".into(),
            snapshot,
            added: "2026-10-27".into(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 1.5,
            up_speed: 0.0,
            seed: false,
            dest: PathBuf::from("/tmp/films"),
            part_count: 4,
            debrid: None,
        }
    }

    #[test]
    fn job_view_copies_snapshot_fields() {
        let view = JobView::from_job(&sample());
        assert_eq!(view.id, 3);
        assert_eq!(view.kind, Kind::Http);
        assert_eq!(view.title, "Sintel");
        assert_eq!(view.status, "queued");
        assert_eq!(view.downloaded, 10);
        assert_eq!(view.dest, "/tmp/films");
        assert!(view.active);
        assert!(!view.paused);
        assert!(view.error.is_none());
    }

    #[test]
    fn engine_rejects_empty_queue() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            dir.path().display().to_string(),
            dir.path().join("jobs.json").display().to_string(),
            true,
        );
        assert!(engine.queue(Kind::Http, "  ".into()).is_err());
        assert!(engine.jobs().is_empty());
        engine.persist(false);
        engine.persist(true);
        assert!(!engine.any_active());
    }

    #[test]
    fn banner_matches_identity() {
        assert_eq!(banner(), identity::banner());
        assert!(banner().starts_with("danted4/torrenter v"));
    }
}

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::debrid::{check_account, Secret, Service};
use crate::identity;
use crate::jobs::JobKind;
use crate::session::{self, Job, Route, Session};

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
    /// Debrid route only: service id (`torbox`, …).
    pub service: Option<String>,
    /// Provider fetch progress, 0–1000, while the service fetches.
    pub remote_permille: u16,
    /// 1-based file being downloaded and the file count (0 before downloading).
    pub file_index: u32,
    pub file_count: u32,
}

/// A debrid service this build offers.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ServiceView {
    pub id: String,
    pub label: String,
}

/// Result of a settings "Test": never contains the key.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AccountView {
    pub premium: bool,
    pub expires: Option<String>,
}

fn service_from(id: &str) -> Result<Service, EngineError> {
    Service::parse(id)
        .filter(|service| Service::shipped().contains(service))
        .ok_or_else(|| EngineError::Failed {
            msg: format!("unknown debrid service {id}"),
        })
}

#[uniffi::export]
pub fn debrid_services() -> Vec<ServiceView> {
    Service::shipped()
        .iter()
        .map(|service| ServiceView {
            id: service.id().into(),
            label: service.label().into(),
        })
        .collect()
}

impl JobView {
    fn from_job(job: &Job) -> Self {
        let error = job.snapshot.error();
        let debrid = job.debrid.as_ref().map(|d| d.view());
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
            service: job.debrid.as_ref().map(|d| d.service.id().to_string()),
            remote_permille: debrid.as_ref().map(|d| d.remote_permille).unwrap_or(0),
            file_index: debrid
                .as_ref()
                .map(|d| u32::try_from(d.file_index).unwrap_or(u32::MAX))
                .unwrap_or(0),
            file_count: debrid
                .as_ref()
                .map(|d| u32::try_from(d.files.len()).unwrap_or(u32::MAX))
                .unwrap_or(0),
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
        let session = Session::open(PathBuf::from(dest), PathBuf::from(jobs_path), seed);
        // Apps keep keys in the Keystore / Keychain and hand them over.
        session.use_memory_keys();
        Arc::new(Self { session })
    }

    /// Hand a key from the platform keychain to the engine (memory only), or
    /// `None` to forget it. Call before `hydrate` so restored jobs can start.
    pub fn set_debrid_key(&self, service: String, key: Option<String>) -> Result<(), EngineError> {
        let service = service_from(&service)?;
        self.session
            .set_debrid_key(service, key.and_then(Secret::new));
        Ok(())
    }

    pub fn has_debrid_key(&self, service: String) -> bool {
        service_from(&service).is_ok_and(|service| self.session.has_debrid_key(service))
    }

    /// Settings "Test": blocking network call — run it off the UI thread.
    pub fn debrid_account(&self, service: String, key: String) -> Result<AccountView, EngineError> {
        let service = service_from(&service)?;
        let key = Secret::new(key).ok_or_else(|| EngineError::Failed {
            msg: "empty key".into(),
        })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| EngineError::Failed {
                msg: error.to_string(),
            })?;
        runtime
            .block_on(check_account(service, &key))
            .map(|account| AccountView {
                premium: account.premium,
                expires: account.expires,
            })
            .map_err(|error| EngineError::Failed {
                msg: error.to_string(),
            })
    }

    /// Queue a magnet through a debrid service.
    pub fn queue_debrid(&self, source: String, service: String) -> Result<u64, EngineError> {
        let service = service_from(&service)?;
        self.session
            .queue_with(JobKind::Magnet, &source, Some(service))
            .map_err(Into::into)
    }

    /// Restart re-routed: `service = None` downloads from peers ("as torrent").
    pub fn restart_route(
        &self,
        id: u64,
        service: Option<String>,
        seed: bool,
        dest: String,
    ) -> Result<u64, EngineError> {
        let route = match service {
            Some(id) => Route::Via(service_from(&id)?),
            None => Route::Peers,
        };
        self.session
            .restart_with(id, route, seed, PathBuf::from(dest))
            .map_err(Into::into)
    }

    /// Connections per download for new jobs (`None` = Auto = 4).
    pub fn set_connections(&self, parts: Option<u8>) {
        self.session.set_connections(parts);
    }

    pub fn set_debrid_remove_after(&self, enabled: bool) {
        self.session.set_debrid_remove_after(enabled);
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
        assert!(view.service.is_none());
        assert_eq!(view.file_count, 0);
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
    fn engine_keeps_keys_in_memory_only() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            dir.path().display().to_string(),
            dir.path().join("jobs.json").display().to_string(),
            false,
        );
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10".to_string();
        assert!(!engine.has_debrid_key("torbox".into()));
        let error = engine
            .queue_debrid(magnet.clone(), "torbox".into())
            .unwrap_err();
        assert!(
            error.to_string().contains("add a TorBox API key"),
            "{error}"
        );
        engine
            .set_debrid_key("torbox".into(), Some("k".into()))
            .unwrap();
        assert!(engine.has_debrid_key("torbox".into()));
        engine.set_debrid_key("torbox".into(), None).unwrap();
        assert!(!engine.has_debrid_key("torbox".into()));
        assert!(engine
            .set_debrid_key("nope".into(), Some("k".into()))
            .is_err());
        assert_eq!(debrid_services()[0].id, "torbox");
        assert!(
            !dir.path().join("credentials.json").exists(),
            "mobile never writes a credentials file"
        );
    }

    #[test]
    fn banner_matches_identity() {
        assert_eq!(banner(), identity::banner());
        assert!(banner().starts_with("danted4/torrenter v"));
    }
}

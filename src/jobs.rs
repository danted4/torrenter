use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsutil::{atomic_write, quarantine, recover_incomplete_save};
use crate::progress::JobSnapshot;
use crate::update::config_path;
use crate::validate::{is_valid_download_url, is_valid_magnet_url};

pub const MISSING_FILES: &str = "files missing from download folder";
pub const HTTP_NOT_RESUMABLE: &str = "HTTP download cannot resume; restart to start over";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Magnet,
    Http,
    /// A magnet routed through a debrid provider (downloads over HTTP).
    Debrid,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Magnet => "magnet",
            Self::Http => "http",
            Self::Debrid => "debrid",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "magnet" => Some(Self::Magnet),
            "http" => Some(Self::Http),
            "debrid" => Some(Self::Debrid),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Magnet => "MAGNET",
            Self::Http => "HTTP",
            Self::Debrid => "DEBRID",
        }
    }

    /// Magnet input (plain or via a debrid provider).
    pub fn takes_magnet(self) -> bool {
        matches!(self, Self::Magnet | Self::Debrid)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedJob {
    pub id: u64,
    pub kind: String,
    pub source: String,
    pub dest: PathBuf,
    #[serde(default)]
    pub seed: bool,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// HTTP parts this job was created with (resume needs the same split).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_count: Option<u8>,
    /// Debrid route: provider id (`torbox`, …). Never a key or a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Debrid route: the provider's id for this torrent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// Debrid route: torrenter added it, so "remove after download" may delete it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added_by_us: bool,
    /// Debrid route: files reported by the provider (ids and relative paths only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PersistedFile>,
    /// Fields written by newer builds, kept so a rewrite does not drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedFile {
    pub id: String,
    /// Relative path inside the job folder, already sanitized.
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobListFile {
    #[serde(default)]
    jobs: Vec<Value>,
}

/// Jobs this build understands, plus records of kinds it does not (written by a
/// newer build). Foreign records are written back untouched on every save.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct JobList {
    pub jobs: Vec<PersistedJob>,
    pub foreign: Vec<Value>,
}

impl JobList {
    /// Highest id across known and foreign records, so new ids never collide.
    pub fn max_id(&self) -> u64 {
        let foreign = self
            .foreign
            .iter()
            .filter_map(|value| value.get("id").and_then(Value::as_u64));
        self.jobs
            .iter()
            .map(|job| job.id)
            .chain(foreign)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    Start,
    Idle,
    Missing,
}

#[derive(Debug)]
pub enum LoadJobs {
    Loaded(JobList),
    Empty,
    Corrupt(String),
}

pub fn jobs_path() -> Result<PathBuf> {
    let config = config_path()?;
    Ok(config.with_file_name("jobs.json"))
}

pub fn load_jobs() -> LoadJobs {
    let Ok(path) = jobs_path() else {
        return LoadJobs::Empty;
    };
    load_jobs_from(&path)
}

pub fn save_jobs(jobs: &[PersistedJob]) -> Result<()> {
    let path = jobs_path()?;
    save_jobs_to(&path, jobs)
}

pub fn load_jobs_from(path: &Path) -> LoadJobs {
    recover_incomplete_save(path);
    if !path.exists() {
        return LoadJobs::Empty;
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            quarantine(path);
            return LoadJobs::Corrupt(error.to_string());
        }
    };
    match serde_json::from_slice::<JobListFile>(&bytes) {
        Ok(file) => LoadJobs::Loaded(split_records(file.jobs)),
        Err(error) => {
            quarantine(path);
            LoadJobs::Corrupt(error.to_string())
        }
    }
}

/// Known kinds must parse and validate or they are dropped (one bad record no
/// longer quarantines the file). Unknown kinds are kept as-is for newer builds.
fn split_records(records: Vec<Value>) -> JobList {
    let mut list = JobList::default();
    for record in records {
        let known = record
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| JobKind::parse(kind).is_some());
        if !known {
            if record.get("id").and_then(Value::as_u64).is_some() {
                list.foreign.push(record);
            }
            continue;
        }
        if let Ok(job) = serde_json::from_value::<PersistedJob>(record) {
            if job.is_valid() {
                list.jobs.push(job);
            }
        }
    }
    list
}

pub fn save_jobs_to(path: &Path, jobs: &[PersistedJob]) -> Result<()> {
    save_job_list_to(path, jobs, &[])
}

pub fn save_job_list_to(path: &Path, jobs: &[PersistedJob], foreign: &[Value]) -> Result<()> {
    let mut records = Vec::with_capacity(jobs.len() + foreign.len());
    for job in jobs {
        records.push(serde_json::to_value(job).context("failed to serialize jobs")?);
    }
    records.extend(foreign.iter().cloned());
    let file = JobListFile { jobs: records };
    let bytes = serde_json::to_vec_pretty(&file).context("failed to serialize jobs")?;
    atomic_write(path, &bytes, false)
}

impl PersistedJob {
    pub fn kind_id(&self) -> Option<JobKind> {
        JobKind::parse(&self.kind)
    }

    pub fn is_valid(&self) -> bool {
        if self.id == 0 || self.source.trim().is_empty() || self.dest.as_os_str().is_empty() {
            return false;
        }
        match self.kind_id() {
            Some(JobKind::Magnet) => is_valid_magnet_url(&self.source),
            Some(JobKind::Http) => is_valid_download_url(&self.source),
            Some(JobKind::Debrid) => {
                is_valid_magnet_url(&self.source)
                    && self
                        .service
                        .as_deref()
                        .and_then(crate::debrid::Service::parse)
                        .is_some()
            }
            None => false,
        }
    }

    pub fn wants_engine(&self) -> bool {
        if !self.is_valid() {
            return false;
        }
        if self.error.is_some() && self.status == "failed" {
            return false;
        }
        !matches!(self.status.as_str(), "done" | "stopped" | "failed")
    }

    pub fn after_probe(&self, present: bool, http_resumable: bool) -> RestoreAction {
        if !self.wants_engine() {
            return RestoreAction::Idle;
        }
        match self.kind_id() {
            Some(JobKind::Http) => {
                if self.downloaded > 0 && !http_resumable {
                    RestoreAction::Missing
                } else {
                    RestoreAction::Start
                }
            }
            // The runner re-checks every file and can fetch again from the provider.
            Some(JobKind::Debrid) => RestoreAction::Start,
            Some(JobKind::Magnet) => {
                if self.downloaded > 0 && !present {
                    RestoreAction::Missing
                } else {
                    RestoreAction::Start
                }
            }
            None => RestoreAction::Idle,
        }
    }

    pub fn apply_to(&self, snapshot: &JobSnapshot) {
        snapshot
            .downloaded
            .store(self.downloaded, std::sync::atomic::Ordering::Relaxed);
        snapshot
            .total
            .store(self.total, std::sync::atomic::Ordering::Relaxed);
        if !self.title.is_empty() {
            snapshot.set_title(&self.title);
        }
        if self.status == "failed" {
            snapshot.set_error(self.error.clone().unwrap_or_else(|| "failed".into()));
            return;
        }
        if self.status == "stopped" {
            if let Some(error) = &self.error {
                snapshot.mark_stopped_with(error);
            } else {
                snapshot.mark_stopped();
            }
            return;
        }
        if self.status == "done" {
            snapshot.mark_finished();
            return;
        }
        let status = if self.paused {
            "paused"
        } else {
            self.status.as_str()
        };
        if !status.is_empty() {
            snapshot.set_status(status);
        }
        if self.paused {
            snapshot.set_paused(true);
        }
    }

    pub fn missing_reason(&self) -> &'static str {
        if self.kind_id() == Some(JobKind::Http) {
            HTTP_NOT_RESUMABLE
        } else {
            MISSING_FILES
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::JobSnapshot;

    fn sample(kind: &str, status: &str, downloaded: u64) -> PersistedJob {
        PersistedJob {
            id: 1,
            kind: kind.into(),
            source: if kind == "magnet" {
                "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10".into()
            } else {
                "https://example.com/file.bin".into()
            },
            dest: PathBuf::from("/tmp/torrenter-out"),
            seed: kind == "magnet",
            added: "2026-11-05".into(),
            title: "file".into(),
            downloaded,
            total: 100,
            status: status.into(),
            paused: status == "paused",
            error: None,
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let job = sample("magnet", "downloading", 10);
        save_jobs_to(&path, std::slice::from_ref(&job)).unwrap();
        match load_jobs_from(&path) {
            LoadJobs::Loaded(list) => assert_eq!(list.jobs, vec![job]),
            other => panic!("expected loaded, got {other:?}"),
        }
        let parsed: PersistedJob = serde_json::from_str(
            r#"{"id":2,"kind":"http","source":"https://example.com/a","dest":"/tmp/a","extra":true}"#,
        )
        .unwrap();
        assert_eq!(parsed.id, 2);
        assert!(!parsed.paused);
        assert_eq!(parsed.downloaded, 0);
        // Unknown fields survive a rewrite.
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["extra"], Value::Bool(true));
    }

    #[test]
    fn unknown_kinds_survive_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::write(
            &path,
            r#"{"jobs":[
                {"id":7,"kind":"future","source":"x","dest":"/tmp/a","newField":{"a":1}},
                {"id":3,"kind":"http","source":"https://example.com/a","dest":"/tmp/a"},
                {"id":4,"kind":"http","source":"https://example.com/b","dest":"/tmp/a","downloaded":"oops"}
            ]}"#,
        )
        .unwrap();
        let list = match load_jobs_from(&path) {
            LoadJobs::Loaded(list) => list,
            other => panic!("expected loaded, got {other:?}"),
        };
        assert_eq!(
            list.jobs.len(),
            1,
            "malformed known record is dropped alone"
        );
        assert_eq!(list.foreign.len(), 1);
        assert_eq!(list.max_id(), 7);
        save_job_list_to(&path, &list.jobs, &list.foreign).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let kinds: Vec<_> = raw["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["http", "future"]);
        assert_eq!(raw["jobs"][1]["newField"]["a"], 1);
    }

    #[test]
    fn debrid_fields_roundtrip_without_links_or_keys() {
        let job = PersistedJob {
            part_count: Some(8),
            service: Some("torbox".into()),
            remote_id: Some("123".into()),
            added_by_us: true,
            files: vec![PersistedFile {
                id: "0".into(),
                path: "Album/01.flac".into(),
                size: 10,
                done: false,
            }],
            ..sample("magnet", "downloading", 0)
        };
        let text = serde_json::to_string(&job).unwrap();
        assert!(text.contains("\"remoteId\":\"123\""));
        assert!(text.contains("\"addedByUs\":true"));
        let back: PersistedJob = serde_json::from_str(&text).unwrap();
        assert_eq!(back, job);
        let plain = serde_json::to_string(&sample("http", "queued", 0)).unwrap();
        assert!(
            !plain.contains("service") && !plain.contains("addedByUs") && !plain.contains("files")
        );
    }

    #[test]
    fn replace_keeps_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let first = sample("magnet", "downloading", 10);
        save_jobs_to(&path, std::slice::from_ref(&first)).unwrap();
        assert!(path.exists());
        let second = sample("http", "queued", 0);
        save_jobs_to(&path, std::slice::from_ref(&second)).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
        match load_jobs_from(&path) {
            LoadJobs::Loaded(list) => assert_eq!(list.jobs, vec![second]),
            other => panic!("expected loaded, got {other:?}"),
        }
    }

    #[test]
    fn load_recovers_tmp_when_live_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let job = sample("magnet", "paused", 10);
        save_jobs_to(&path, std::slice::from_ref(&job)).unwrap();
        std::fs::rename(&path, path.with_extension("json.tmp")).unwrap();
        assert!(!path.exists());
        match load_jobs_from(&path) {
            LoadJobs::Loaded(list) => assert_eq!(list.jobs, vec![job]),
            other => panic!("expected recovered, got {other:?}"),
        }
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn corrupt_file_moves_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::write(&path, "not-json").unwrap();
        match load_jobs_from(&path) {
            LoadJobs::Corrupt(_) => {}
            other => panic!("expected corrupt, got {other:?}"),
        }
        assert!(!path.exists());
        assert!(path.with_extension("json.bak").exists());
    }

    #[test]
    fn skips_invalid_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::write(
            &path,
            r#"{"jobs":[{"id":1,"kind":"magnet","source":"https://nope","dest":"/tmp/a"}]}"#,
        )
        .unwrap();
        match load_jobs_from(&path) {
            LoadJobs::Loaded(list) => assert!(list.jobs.is_empty()),
            other => panic!("expected loaded empty, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        match load_jobs_from(&dir.path().join("jobs.json")) {
            LoadJobs::Empty => {}
            other => panic!("expected empty, got {other:?}"),
        }
    }

    #[test]
    fn probe_rules() {
        let magnet_live = sample("magnet", "downloading", 50);
        assert_eq!(
            magnet_live.after_probe(false, false),
            RestoreAction::Missing
        );
        assert_eq!(magnet_live.after_probe(true, false), RestoreAction::Start);
        let magnet_zero = sample("magnet", "queued", 0);
        assert_eq!(magnet_zero.after_probe(false, false), RestoreAction::Start);
        let http_live = sample("http", "downloading", 20);
        assert_eq!(http_live.after_probe(true, false), RestoreAction::Missing);
        assert_eq!(http_live.after_probe(false, true), RestoreAction::Start);
        let done = sample("magnet", "done", 100);
        assert_eq!(done.after_probe(true, false), RestoreAction::Idle);
        let paused = sample("magnet", "paused", 10);
        assert!(paused.wants_engine());
        assert_eq!(paused.after_probe(true, false), RestoreAction::Start);
        let stopped = sample("http", "stopped", 10);
        assert_eq!(stopped.after_probe(false, true), RestoreAction::Idle);
    }

    #[test]
    fn apply_paused_keeps_engine_active() {
        let snapshot = JobSnapshot::default();
        sample("magnet", "paused", 10).apply_to(&snapshot);
        assert!(snapshot.is_paused());
        assert!(snapshot.is_active());
        assert_eq!(snapshot.status(), "paused");
        assert_eq!(
            snapshot
                .downloaded
                .load(std::sync::atomic::Ordering::Relaxed),
            10
        );
    }

    #[test]
    fn apply_missing_is_stopped_not_failed() {
        let snapshot = JobSnapshot::default();
        let mut job = sample("magnet", "stopped", 40);
        job.error = Some(MISSING_FILES.into());
        job.apply_to(&snapshot);
        assert!(snapshot.is_stopped());
        assert!(!snapshot.is_active());
        assert_eq!(snapshot.error().as_deref(), Some(MISSING_FILES));
        assert_eq!(snapshot.status(), "stopped");
    }
}

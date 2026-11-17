//! Debrid job runner: magnet → provider (fetch) → each file via the HTTP engine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use url::Url;

use super::{
    inside, redact, safe_relative_path, DebridError, Provider, RemoteFile, RemoteState, Secret,
    Service,
};
use crate::http::{download_job_file, range_client, DownloadLayout, LinkRejected};
use crate::jobs::PersistedFile;
use crate::progress::{
    is_stopped, until_stop, wait_while_paused, DownloadStopped, JobSnapshot, ProgressHandle,
};
use crate::torrent::{claim_magnet_sub_folder, magnet_folder_lock, magnet_info_hash_hex};
use crate::validate::is_valid_magnet_url;

const POLL_FIRST: Duration = Duration::from_secs(3);
const POLL_MAX: Duration = Duration::from_secs(30);
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
const NETWORK_RETRIES: u32 = 5;
/// Consecutive rate-limit / 503 waits before giving up (~10 minutes).
const RATE_LIMIT_RETRIES: u32 = 20;
/// Fail a job whose provider-side progress has not moved for this long, so a
/// dead torrent frees its download slot. Measured from the last progress
/// change, never counting time paused. 30 minutes (the user's call): a stuck
/// slot frees sooner, at the cost that a provider reporting whole percents
/// (Real-Debrid) can fail a big, very slow torrent between two steps, or one
/// waiting long in the provider's own queue; restart waits again.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Granularity of the stall clock and of noticing a resume while fetching.
const STALL_TICK: Duration = Duration::from_millis(250);

/// Provider-side progress and ids for one debrid job. Shared with the session,
/// which persists it and shows it; never holds a link or a key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebridState {
    pub remote_id: Option<String>,
    pub added_by_us: bool,
    pub files: Vec<PersistedFile>,
    /// 0–1000 while the provider fetches.
    pub remote_permille: u16,
    /// 1-based file being downloaded (0 before the download phase).
    pub file_index: usize,
}

pub type SharedDebridState = Arc<Mutex<DebridState>>;

fn lock(state: &SharedDebridState) -> MutexGuard<'_, DebridState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Everything one run needs. `key` is resolved by the caller (credentials file,
/// env, or the mobile keychain) at start.
pub struct DebridRun {
    pub service: Service,
    pub key: Secret,
    pub magnet: String,
    pub dest: PathBuf,
    /// Connections setting; capped by the provider.
    pub part_count: usize,
    pub remove_after: bool,
    pub state: SharedDebridState,
    /// Test hook: provider API base instead of the real host.
    pub base: Option<Url>,
    /// `STALL_TIMEOUT` outside tests.
    pub stall_timeout: Duration,
}

/// Run a debrid job to completion, stop, or failure. Errors are redacted (no
/// links, no key) because they end up in `jobs.json`, the UI and MCP output.
pub async fn start_debrid_reported(run: DebridRun, snapshot: Arc<JobSnapshot>) -> Result<()> {
    let key = run.key.clone();
    let hint = run.service.report_hint();
    match drive(run, Arc::clone(&snapshot)).await {
        Ok(()) => Ok(()),
        Err(error) if is_stopped(&error) => {
            snapshot.mark_stopped();
            Ok(())
        }
        Err(error) => {
            let mut text = redact(&format!("{error:#}"), Some(&key));
            if let Some(hint) = hint {
                text = format!("{text} ({hint})");
            }
            // Recorded here too, so the job is saved as failed (and not
            // restarted on the next launch) even before the session drains it.
            snapshot.set_error(text.clone());
            Err(anyhow!(text))
        }
    }
}

async fn drive(run: DebridRun, snapshot: Arc<JobSnapshot>) -> Result<()> {
    if !is_valid_magnet_url(&run.magnet) {
        bail!("Invalid magnet URL!");
    }
    let hash =
        magnet_info_hash_hex(&run.magnet).ok_or_else(|| anyhow!("magnet has no info hash"))?;
    // Same folder and lock as a plain magnet job, so "as torrent" continues in
    // place and the two routes never write one folder at once.
    let folder_lock = magnet_folder_lock(&hash);
    let _folder = tokio::select! {
        guard = folder_lock.lock() => guard,
        _ = until_stop(Some(snapshot.as_ref())) => bail!(DownloadStopped),
    };
    let sub_folder = claim_magnet_sub_folder(&run.dest, &run.magnet)?;
    let job_root = run.dest.join(sub_folder);
    let provider = Provider::new(run.service, run.key.clone(), run.base.clone())?;

    // Phase 1: the provider fetches the torrent.
    show_fetching(&snapshot);
    let remote_id = ensure_remote(&provider, &run, &hash, &snapshot).await?;
    let files = wait_ready(&provider, &run, &remote_id, &hash, &snapshot).await?;

    // Phase 2: download each file in order.
    let plan = plan_files(&files)?;
    sync_files(&run.state, &plan);
    let total: u64 = plan.iter().map(|item| item.file.size).sum();
    snapshot
        .total
        .store(total, std::sync::atomic::Ordering::Relaxed);
    if !snapshot.is_paused() {
        snapshot.set_status("downloading");
    }
    let parts = run.part_count.clamp(1, run.service.max_connections());
    let client = range_client(concat!("torrenter/", env!("CARGO_PKG_VERSION")))?;
    let mut done_bytes: u64 = 0;
    for (index, item) in plan.iter().enumerate() {
        let already = lock(&run.state)
            .files
            .iter()
            .any(|file| file.id == item.file.id && file.done);
        if !inside(&job_root, &item.relative) {
            bail!("refusing to write outside the download folder");
        }
        let output = job_root.join(&item.relative);
        let present = std::fs::metadata(&output)
            .map(|meta| meta.len() == item.file.size)
            .unwrap_or(false);
        if already && present {
            done_bytes += item.file.size;
            continue;
        }
        lock(&run.state).file_index = index + 1;
        let directory = output
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| job_root.clone());
        let filename = item
            .relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download.bin")
            .to_string();
        let layout = DownloadLayout::for_job_file(
            directory,
            job_root.clone(),
            format!("{}|{remote_id}|{}", run.service.id(), item.file.id),
            parts,
        );
        let progress =
            ProgressHandle::job_file(Arc::clone(&snapshot), done_bytes, total, item.file.size);
        // A rejected (expired) link is refreshed once per stretch without
        // progress: a second expiry after more bytes landed (another long
        // pause, an IP change) refreshes again; two in a row fail the job.
        let mut refreshed_at: Option<u64> = None;
        loop {
            wait_while_paused(Some(snapshot.as_ref())).await?;
            let link = retrying(&snapshot, || provider.link(&remote_id, &item.file)).await?;
            match download_job_file(
                &client,
                &link,
                &layout,
                &filename,
                item.file.size,
                &progress,
            )
            .await
            {
                Ok(()) => break,
                Err(error) if error.downcast_ref::<LinkRejected>().is_some() => {
                    let now = snapshot
                        .downloaded
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if refreshed_at.is_some_and(|at| now <= at) {
                        return Err(error);
                    }
                    refreshed_at = Some(now);
                }
                Err(error) => return Err(error),
            }
        }
        mark_done(&run.state, &item.file.id);
        done_bytes += item.file.size;
    }

    // Every part is merged; drop the job's now-empty resume folder (only
    // removed when empty).
    let _ = std::fs::remove_dir(job_root.join(".resume"));
    if run.remove_after && lock(&run.state).added_by_us {
        // Best effort: the files are already safe on disk.
        let _ = stoppable(&snapshot, provider.delete(&remote_id)).await;
    }
    snapshot
        .downloaded
        .store(total, std::sync::atomic::Ordering::Relaxed);
    snapshot
        .total
        .store(total, std::sync::atomic::Ordering::Relaxed);
    snapshot.mark_finished();
    Ok(())
}

async fn ensure_remote(
    provider: &Provider,
    run: &DebridRun,
    hash: &str,
    snapshot: &Arc<JobSnapshot>,
) -> Result<String> {
    if let Some(remote_id) = lock(&run.state).remote_id.clone() {
        return Ok(remote_id);
    }
    let added = retrying(snapshot, || provider.add(&run.magnet, hash)).await?;
    let mut state = lock(&run.state);
    state.remote_id = Some(added.remote_id.clone());
    state.added_by_us = added.added_by_us;
    Ok(added.remote_id)
}

/// Poll until the provider has the files. If the remote torrent vanished
/// (deleted on the website), add it again once.
async fn wait_ready(
    provider: &Provider,
    run: &DebridRun,
    remote_id: &str,
    hash: &str,
    snapshot: &Arc<JobSnapshot>,
) -> Result<Vec<RemoteFile>> {
    let mut remote_id = remote_id.to_string();
    let mut re_added = false;
    let mut interval = POLL_FIRST;
    // Unpaused time since provider progress last moved.
    let mut stalled = Duration::ZERO;
    let mut best: Option<u16> = None;
    loop {
        wait_while_paused(Some(snapshot.as_ref())).await?;
        show_fetching(snapshot);
        match retrying(snapshot, || provider.status(&remote_id)).await {
            Ok(remote) => match remote.state {
                RemoteState::Ready => {
                    lock(&run.state).remote_permille = 1000;
                    return Ok(remote.files);
                }
                RemoteState::Failed(reason) => bail!("{reason}"),
                RemoteState::Fetching { permille } => {
                    lock(&run.state).remote_permille = permille;
                    if best.is_none_or(|best| permille > best) {
                        best = Some(permille);
                        stalled = Duration::ZERO;
                    } else if stalled >= run.stall_timeout {
                        bail!(
                            "{} made no progress on this torrent for {}; restart to keep \
                             waiting, or download it as a torrent",
                            run.service.label(),
                            describe(run.stall_timeout)
                        );
                    }
                }
            },
            Err(error)
                if !re_added
                    && error
                        .downcast_ref::<DebridError>()
                        .is_some_and(|error| *error == DebridError::Gone) =>
            {
                re_added = true;
                lock(&run.state).remote_id = None;
                remote_id = ensure_remote(provider, run, hash, snapshot).await?;
                best = None;
                stalled = Duration::ZERO;
                continue;
            }
            Err(error) => return Err(error),
        }
        // Wake at the deadline rather than a full interval past it.
        let wait = interval.min(run.stall_timeout.saturating_sub(stalled).max(STALL_TICK));
        fetch_sleep(snapshot, wait, &mut stalled).await?;
        interval = (interval.mul_f32(1.5)).min(POLL_MAX);
    }
}

/// Status `fetching` while unpaused. Pause / resume rewrites the status, and a
/// restored job may start paused, so this is re-asserted while waiting.
fn show_fetching(snapshot: &JobSnapshot) {
    if !snapshot.is_paused() && snapshot.status() != "fetching" {
        snapshot.set_status("fetching");
    }
}

/// Sleep `wait` in short slices, adding only the unpaused slices to `stalled`.
async fn fetch_sleep(
    snapshot: &Arc<JobSnapshot>,
    wait: Duration,
    stalled: &mut Duration,
) -> Result<()> {
    let mut left = wait;
    while !left.is_zero() {
        let slice = left.min(STALL_TICK);
        sleep_or_stop(snapshot, slice).await?;
        left -= slice;
        if !snapshot.is_paused() {
            *stalled += slice;
            show_fetching(snapshot);
        }
    }
    Ok(())
}

fn describe(wait: Duration) -> String {
    let minutes = wait.as_secs() / 60;
    match minutes {
        0 => match wait.as_secs() {
            0 | 1 => "1 second".into(),
            seconds => format!("{seconds} seconds"),
        },
        1 => "1 minute".into(),
        _ => format!("{minutes} minutes"),
    }
}

/// Retry transient failures (rate limits, network) with backoff; give up on
/// anything else. Races stop so a waiting job stops at once.
async fn retrying<T, F, Fut>(snapshot: &Arc<JobSnapshot>, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, DebridError>>,
{
    let mut network_failures = 0;
    let mut rate_limited = 0;
    loop {
        match stoppable(snapshot, call()).await {
            Ok(value) => return Ok(value),
            Err(error) => match error.downcast_ref::<DebridError>() {
                Some(DebridError::RateLimited(_)) if rate_limited >= RATE_LIMIT_RETRIES => {
                    bail!("the service kept refusing requests (rate limited or unavailable)");
                }
                Some(DebridError::RateLimited(wait)) => {
                    rate_limited += 1;
                    sleep_or_stop(snapshot, wait.unwrap_or(RATE_LIMIT_WAIT)).await?;
                }
                Some(DebridError::Network(_)) if network_failures < NETWORK_RETRIES => {
                    network_failures += 1;
                    sleep_or_stop(snapshot, POLL_FIRST * network_failures).await?;
                }
                _ => return Err(error),
            },
        }
    }
}

async fn stoppable<T>(
    snapshot: &Arc<JobSnapshot>,
    call: impl std::future::Future<Output = std::result::Result<T, DebridError>>,
) -> Result<T> {
    tokio::select! {
        result = call => result.map_err(anyhow::Error::from),
        _ = until_stop(Some(snapshot.as_ref())) => bail!(DownloadStopped),
    }
}

async fn sleep_or_stop(snapshot: &Arc<JobSnapshot>, wait: Duration) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(wait) => Ok(()),
        _ = until_stop(Some(snapshot.as_ref())) => bail!(DownloadStopped),
    }
}

struct PlannedFile {
    file: RemoteFile,
    relative: PathBuf,
}

/// Safe relative paths for every file, with a folder shared by all of them
/// dropped (the job folder already carries the torrent name).
fn plan_files(files: &[RemoteFile]) -> Result<Vec<PlannedFile>> {
    let mut planned: Vec<PlannedFile> = files
        .iter()
        .filter_map(|file| {
            safe_relative_path(&file.path)
                .filter(|relative| inside(Path::new("/job"), relative))
                .map(|relative| PlannedFile {
                    file: file.clone(),
                    relative,
                })
        })
        .collect();
    if planned.is_empty() {
        bail!("the provider listed no files that can be saved safely");
    }
    let first_root = planned[0]
        .relative
        .components()
        .next()
        .map(|c| c.as_os_str().to_owned());
    let shared_root = planned.iter().all(|item| {
        item.relative.components().count() > 1
            && item
                .relative
                .components()
                .next()
                .map(|c| c.as_os_str().to_owned())
                == first_root
    });
    if shared_root {
        for item in &mut planned {
            item.relative = item.relative.components().skip(1).collect();
        }
    }
    Ok(planned)
}

fn sync_files(state: &SharedDebridState, plan: &[PlannedFile]) {
    let mut state = lock(state);
    let previous = std::mem::take(&mut state.files);
    state.files = plan
        .iter()
        .map(|item| PersistedFile {
            id: item.file.id.clone(),
            // Always "/"-separated, so jobs.json reads the same on every OS.
            path: item
                .relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            size: item.file.size,
            done: previous
                .iter()
                .any(|file| file.id == item.file.id && file.done),
        })
        .collect();
}

fn mark_done(state: &SharedDebridState, file_id: &str) {
    if let Some(file) = lock(state).files.iter_mut().find(|file| file.id == file_id) {
        file.done = true;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::debrid::torbox::tests::{magnet, torbox_fake, State, KEY};

    fn run_for(dest: &Path, base: Url, state: SharedDebridState) -> DebridRun {
        DebridRun {
            service: Service::TorBox,
            key: Secret::new(KEY).unwrap(),
            magnet: magnet(),
            dest: dest.to_path_buf(),
            part_count: 4,
            remove_after: true,
            state,
            base: Some(base),
            stall_timeout: crate::debrid::run::STALL_TIMEOUT,
        }
    }

    #[tokio::test]
    async fn torbox_job_downloads_every_file_into_the_magnet_folder() {
        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 0,
            expire_links: 1,
            ..State::default()
        }));
        let (server, base) = torbox_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        start_debrid_reported(
            run_for(dir.path(), base, Arc::clone(&state)),
            Arc::clone(&snapshot),
        )
        .await
        .unwrap();
        assert!(snapshot.is_done(), "status {}", snapshot.status());
        let job = dir.path().join("Album");
        let one = std::fs::read(job.join("CD1/01.bin")).unwrap();
        let two = std::fs::read(job.join("02.bin")).unwrap();
        assert_eq!(one.len(), 3000);
        assert_eq!(two.len(), 1500);
        assert_eq!(snapshot.downloaded.load(Ordering::Relaxed), 4500);
        let state = state.lock().unwrap().clone();
        assert_eq!(state.remote_id.as_deref(), Some("1"));
        assert!(state.added_by_us);
        assert!(state.files.iter().all(|file| file.done));
        assert_eq!(
            state.files[0].path, "CD1/01.bin",
            "shared torrent folder dropped"
        );
        // The expired first link was refreshed once; remove-after deleted it.
        assert!(server.count("requestdl") >= 3);
        assert!(fake.lock().unwrap().torrents.is_empty());
        assert!(!job.join(".resume").exists(), "empty resume folder removed");
    }

    #[tokio::test]
    async fn waiting_job_stops_promptly_and_errors_are_redacted() {
        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 1_000,
            ..State::default()
        }));
        let (_server, base) = torbox_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        let task = tokio::spawn(start_debrid_reported(
            run_for(dir.path(), base.clone(), Arc::clone(&state)),
            Arc::clone(&snapshot),
        ));
        for _ in 0..100 {
            if state.lock().unwrap().remote_permille > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(snapshot.status(), "fetching");
        assert_eq!(state.lock().unwrap().remote_permille, 450);
        snapshot.request_stop();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stop is prompt while polling")
            .unwrap()
            .unwrap();
        assert!(snapshot.is_stopped());

        // A bad key fails with a message that carries no key and no link.
        let snapshot = Arc::new(JobSnapshot::default());
        let mut run = run_for(dir.path(), base, SharedDebridState::default());
        run.key = Secret::new("wrong-key-123").unwrap();
        let error = start_debrid_reported(run, snapshot).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("API key rejected"), "{text}");
        assert!(
            !text.contains("wrong-key-123") && !text.contains("http"),
            "{text}"
        );
    }

    /// Its own info hash, so tests that hold a fetch open do not queue on
    /// another test's folder lock (the lock is process-wide, keyed by hash).
    fn own_magnet(last: char) -> String {
        let hash: String = crate::debrid::torbox::tests::HASH
            .chars()
            .take(39)
            .chain([last])
            .collect();
        format!("magnet:?xt=urn:btih:{hash}&dn=Album")
    }

    #[tokio::test]
    async fn experimental_provider_failure_asks_for_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = Arc::new(JobSnapshot::default());
        // Nothing listens on port 1: the provider call fails at once.
        let mut run = run_for(
            dir.path(),
            Url::parse("http://127.0.0.1:1/").unwrap(),
            SharedDebridState::default(),
        );
        run.service = Service::RealDebrid;
        run.magnet = own_magnet('2');
        let error = start_debrid_reported(run, Arc::clone(&snapshot))
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.ends_with(
                "(Real-Debrid support is experimental; if this looks wrong, please report it at github.com/danted4/torrenter/issues)"
            ),
            "{text}"
        );
        assert!(!text.contains(KEY), "{text}");
        assert_eq!(snapshot.error().as_deref(), Some(text.as_str()));
    }

    #[tokio::test]
    async fn stalled_fetch_fails_after_the_timeout_and_frees_the_slot() {
        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 1_000,
            ..State::default()
        }));
        let (_server, base) = torbox_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let snapshot = Arc::new(JobSnapshot::default());
        let mut run = run_for(dir.path(), base, SharedDebridState::default());
        run.magnet = own_magnet('1');
        run.stall_timeout = Duration::from_millis(600);
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            start_debrid_reported(run, Arc::clone(&snapshot)),
        )
        .await
        .expect("a stalled fetch gives up")
        .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(600));
        let text = format!("{error:#}");
        assert!(
            text.contains("TorBox made no progress on this torrent for 1 second;"),
            "{text}"
        );
        // Failed, not stopped: saved as failed, so it is not restarted on launch.
        assert_eq!(snapshot.status(), "failed");
        assert_eq!(snapshot.error().as_deref(), Some(text.as_str()));
        assert!(!snapshot.is_active());
    }

    #[tokio::test]
    async fn slow_but_moving_fetch_never_times_out() {
        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 12,
            advance: true,
            ..State::default()
        }));
        let (_server, base) = torbox_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let snapshot = Arc::new(JobSnapshot::default());
        let mut run = run_for(dir.path(), base, SharedDebridState::default());
        run.magnet = own_magnet('2');
        // Shorter than the whole fetch, longer than any one poll gap.
        run.stall_timeout = Duration::from_millis(400);
        start_debrid_reported(run, Arc::clone(&snapshot))
            .await
            .unwrap();
        assert!(snapshot.is_done(), "status {}", snapshot.status());
    }

    #[tokio::test]
    async fn paused_time_does_not_count_and_resume_shows_fetching() {
        let fake = Arc::new(Mutex::new(State {
            fetch_polls: 1_000,
            ..State::default()
        }));
        let (_server, base) = torbox_fake(Arc::clone(&fake)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = SharedDebridState::default();
        let snapshot = Arc::new(JobSnapshot::default());
        let mut run = run_for(dir.path(), base, Arc::clone(&state));
        run.magnet = own_magnet('3');
        run.stall_timeout = Duration::from_millis(800);
        let task = tokio::spawn(start_debrid_reported(run, Arc::clone(&snapshot)));
        for _ in 0..100 {
            if state.lock().unwrap().remote_permille > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        snapshot.toggle_pause();
        tokio::time::sleep(Duration::from_millis(2_000)).await;
        assert!(
            !task.is_finished(),
            "paused time counted toward the timeout"
        );
        assert_eq!(snapshot.status(), "paused");
        snapshot.toggle_pause();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            snapshot.status(),
            "fetching",
            "resume returns to the fetch view"
        );
        let error = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("times out once unpaused")
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("made no progress"));
    }

    #[test]
    fn stall_message_names_the_wait() {
        assert_eq!(describe(STALL_TIMEOUT), "30 minutes");
        assert_eq!(describe(Duration::from_secs(60)), "1 minute");
        assert_eq!(describe(Duration::from_millis(600)), "1 second");
        assert_eq!(describe(Duration::from_secs(30)), "30 seconds");
    }

    #[test]
    fn plan_keeps_safe_paths_and_drops_a_shared_root() {
        let files = vec![
            RemoteFile {
                id: "0".into(),
                path: "Album/a.bin".into(),
                size: 1,
            },
            RemoteFile {
                id: "1".into(),
                path: "Album/sub/b.bin".into(),
                size: 1,
            },
            RemoteFile {
                id: "2".into(),
                path: "../evil".into(),
                size: 1,
            },
        ];
        let plan = plan_files(&files).unwrap();
        let paths: Vec<_> = plan.iter().map(|item| item.relative.clone()).collect();
        assert_eq!(paths, [PathBuf::from("a.bin"), PathBuf::from("sub/b.bin")]);
        let single = plan_files(&[RemoteFile {
            id: "0".into(),
            path: "movie.mkv".into(),
            size: 1,
        }])
        .unwrap();
        assert_eq!(single[0].relative, PathBuf::from("movie.mkv"));
        assert!(plan_files(&[RemoteFile {
            id: "0".into(),
            path: "/etc/x".into(),
            size: 1
        }])
        .is_err());
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::slots::{SlotTicket, Slots};

#[derive(Debug)]
pub struct DownloadStopped;

impl std::fmt::Display for DownloadStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download stopped")
    }
}

impl std::error::Error for DownloadStopped {}

#[derive(Debug)]
pub struct DownloadPaused;

impl std::fmt::Display for DownloadPaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download paused")
    }
}

impl std::error::Error for DownloadPaused {}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// In-process lock per output key so a restarted job waits for the stopped
/// engine task that still owns the same files.
pub(crate) fn output_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = mutex_lock(LOCKS.get_or_init(|| Mutex::new(HashMap::new())));
    map.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = map.get(key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}

pub const BAR_TEMPLATE: &str =
    "[{bar:20}] {percent}% | Peers: {msg} | D: {have} @ {speed_txt} | T: {full_size} | ETA: {estimate}";

pub const SEED_TEMPLATE: &str = "[{bar:20}] {percent}% | Peers: {msg} | T: {full_size}";

fn bar_keys(style: ProgressStyle) -> ProgressStyle {
    style
        .progress_chars("█░")
        .with_key(
            "have",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_size(state.pos()));
            },
        )
        .with_key(
            "speed_txt",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_speed(state.per_sec()));
            },
        )
        .with_key(
            "full_size",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_size(state.len().unwrap_or(0)));
            },
        )
        .with_key(
            "estimate",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_eta(Some(state.eta())));
            },
        )
}

fn whole_bps(bps: f64) -> u64 {
    if bps.is_finite() && bps > 0.0 {
        bps.round() as u64
    } else {
        0
    }
}

pub fn progress_style() -> ProgressStyle {
    bar_keys(ProgressStyle::with_template(BAR_TEMPLATE).expect("valid progress template"))
}

pub fn seed_progress_style() -> ProgressStyle {
    bar_keys(ProgressStyle::with_template(SEED_TEMPLATE).expect("valid seed progress template"))
}

pub fn new_bar(total: u64) -> ProgressBar {
    styled_bar(ProgressBar::new(total.max(1)), total)
}

fn hidden_bar(total: u64) -> ProgressBar {
    styled_bar(ProgressBar::hidden(), total)
}

fn styled_bar(bar: ProgressBar, total: u64) -> ProgressBar {
    bar.set_length(total.max(1));
    bar.set_style(progress_style());
    bar.set_message("0");
    bar
}

/// Shared counters the GUI polls while the download engines run.
/// Magnet: the swarm is still sending the torrent's info (name, files).
pub const FETCHING_METADATA: &str = "fetching metadata";
/// Magnet: info known, peers found, no data yet.
pub const CONNECTING_TO_PEERS: &str = "connecting to peers";

/// Phases a job waits in before data flows; a resume returns to the same
/// one rather than claiming "downloading".
const WAITING_PHASES: [&str; 4] = [
    FETCHING_METADATA,
    CONNECTING_TO_PEERS,
    "verifying",
    "fetching",
];

/// File selection for a torrent job, shared by the fronts (which set the
/// selection) and the engine (which applies it and fills the list and `have`).
/// The selection is kept sorted and the counts cached, so per-frame and
/// per-poll reads stay cheap on torrents with many files.
#[derive(Debug, Default, Clone)]
pub struct FileState {
    /// Files to download (sorted librqbit indices); `None` = all.
    only: Option<Vec<usize>>,
    /// Bumped on every change, so the engine knows what it has applied.
    pub generation: u64,
    /// Bytes on disk per file (live, or the last saved values).
    pub have: Vec<u64>,
    /// The torrent's files, once the metadata is known.
    list: Option<Arc<Vec<crate::torrent_file::FileEntry>>>,
    /// (real files, selected real files), kept in step with `only` and `list`.
    counts: (u32, u32),
}

impl FileState {
    pub fn only(&self) -> Option<&Vec<usize>> {
        self.only.as_ref()
    }

    pub fn list(&self) -> Option<&Arc<Vec<crate::torrent_file::FileEntry>>> {
        self.list.as_ref()
    }

    /// Set the selection (sorted and deduplicated here). Does not bump
    /// `generation`: callers that want the engine to apply it do that.
    pub fn set_only(&mut self, only: Option<Vec<usize>>) {
        self.only = only.map(|mut only| {
            only.sort_unstable();
            only.dedup();
            only
        });
        self.recount();
    }

    pub fn set_list(&mut self, list: Arc<Vec<crate::torrent_file::FileEntry>>) {
        self.list = Some(list);
        self.recount();
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.only
            .as_ref()
            .is_none_or(|only| only.binary_search(&index).is_ok())
    }

    /// (real files, selected real files); (0, 0) while the list is unknown.
    pub fn counts(&self) -> (u32, u32) {
        self.counts
    }

    fn recount(&mut self) {
        let Some(list) = &self.list else {
            self.counts = (0, 0);
            return;
        };
        let mut real = 0u32;
        let mut selected = 0u32;
        for entry in list.iter().filter(|entry| !entry.padding) {
            real += 1;
            if self.is_selected(entry.index) {
                selected += 1;
            }
        }
        self.counts = (real, selected);
    }
}

#[derive(Debug)]
pub struct JobSnapshot {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub uploaded: AtomicU64,
    pub upload_bps: AtomicU64,
    /// Magnet jobs: librqbit's smoothed live download speed (bytes/s, raw
    /// bytes received). `downloaded` only grows by whole verified pieces, so
    /// a rate taken from it alone jumps between 0 and multiples of a piece.
    pub download_bps: AtomicU64,
    pub peers: AtomicU64,
    pub finished: AtomicBool,
    paused: AtomicBool,
    stop: AtomicBool,
    status: Mutex<String>,
    /// The waiting phase a pause interrupted, restored on resume.
    before_pause: Mutex<Option<String>>,
    title: Mutex<String>,
    error: Mutex<Option<String>>,
    /// Lock order: `files` before `status`.
    files: Mutex<FileState>,
    /// The job's place in the download-slot queue while its engine task
    /// runs (`None` for the CLI and the updater, which never wait).
    slot: Mutex<Option<SlotTicket>>,
}

impl Default for JobSnapshot {
    fn default() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            upload_bps: AtomicU64::new(0),
            download_bps: AtomicU64::new(0),
            peers: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            status: Mutex::new(String::from("queued")),
            before_pause: Mutex::new(None),
            title: Mutex::new(String::new()),
            error: Mutex::new(None),
            files: Mutex::new(FileState::default()),
            slot: Mutex::new(None),
        }
    }
}

impl JobSnapshot {
    pub fn status(&self) -> String {
        mutex_lock(&self.status).clone()
    }

    pub fn set_status(&self, value: impl Into<String>) {
        *mutex_lock(&self.status) = value.into();
    }

    pub fn error(&self) -> Option<String> {
        mutex_lock(&self.error).clone()
    }

    pub fn set_error(&self, value: impl Into<String>) {
        *mutex_lock(&self.error) = Some(value.into());
        self.finished.store(true, Ordering::Relaxed);
        self.set_status("failed");
    }

    pub fn mark_finished(&self) {
        self.finished.store(true, Ordering::Relaxed);
        self.set_status("done");
    }

    pub fn mark_stopped(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        self.finished.store(true, Ordering::Relaxed);
        self.set_status("stopped");
    }

    pub fn mark_stopped_with(&self, message: impl Into<String>) {
        *mutex_lock(&self.error) = Some(message.into());
        self.mark_stopped();
    }

    pub fn set_paused(&self, paused: bool) {
        if !self.is_active() {
            return;
        }
        self.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.set_status("paused");
        }
    }

    pub fn title(&self) -> String {
        mutex_lock(&self.title).clone()
    }

    pub fn set_title(&self, value: impl Into<String>) {
        *mutex_lock(&self.title) = value.into();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed) && !self.finished.load(Ordering::Relaxed)
    }

    pub fn is_active(&self) -> bool {
        !self.finished.load(Ordering::Relaxed) && self.error().is_none()
    }

    pub fn is_stopped(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
            && (self.stop.load(Ordering::Relaxed) || self.status() == "stopped")
    }

    pub fn is_done(&self) -> bool {
        self.status() == "done"
    }

    pub fn toggle_pause(&self) {
        if !self.is_active() {
            return;
        }
        let was_paused = self.paused.fetch_xor(true, Ordering::Relaxed);
        if !was_paused {
            let current = self.status();
            *mutex_lock(&self.before_pause) = WAITING_PHASES
                .contains(&current.as_str())
                .then_some(current);
            self.set_status("paused");
            return;
        }
        if let Some(waiting) = mutex_lock(&self.before_pause).take() {
            self.set_status(waiting);
            return;
        }
        let total = self.total.load(Ordering::Relaxed);
        let downloaded = self.downloaded.load(Ordering::Relaxed);
        self.set_status(if total > 0 && downloaded >= total {
            "seeding"
        } else {
            "downloading"
        });
    }

    pub fn request_stop(&self) {
        if !self.finished.load(Ordering::Relaxed) {
            self.stop.store(true, Ordering::Relaxed);
            self.paused.store(false, Ordering::Relaxed);
        }
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// The job's file selection (torrent jobs; empty for HTTP and debrid).
    pub fn files(&self) -> MutexGuard<'_, FileState> {
        mutex_lock(&self.files)
    }

    pub fn file_counts(&self) -> (u32, u32) {
        self.files().counts()
    }

    /// Gives the job a place in the slot queue (it holds no slot yet).
    pub fn attach_slot(&self, slots: Arc<Slots>, rank: u64) {
        *mutex_lock(&self.slot) = Some(SlotTicket::new(slots, rank));
    }

    /// Leaves the queue for good, giving back any slot held.
    pub fn detach_slot(&self) {
        let ticket = mutex_lock(&self.slot).take();
        drop(ticket);
    }

    /// Paused or seeding: give the slot to the next waiting job.
    pub fn release_slot(&self) {
        if let Some(ticket) = mutex_lock(&self.slot).as_mut() {
            ticket.release();
        }
    }

    /// True when the job may download now: it holds a slot (taking one if it
    /// is its turn) or it has no place in a queue at all.
    pub fn poll_slot(&self) -> bool {
        let paused = self.is_paused();
        match mutex_lock(&self.slot).as_mut() {
            Some(ticket) => ticket.poll(paused),
            None => true,
        }
    }

    fn slots(&self) -> Option<Arc<Slots>> {
        mutex_lock(&self.slot)
            .as_ref()
            .map(|ticket| Arc::clone(ticket.slots()))
    }

    /// Disk full: holds the queue. Call it while the job still holds its
    /// slot, so no waiting job starts before the hold is in place.
    pub fn hold_queue(&self) {
        if let Some(slots) = self.slots() {
            slots.hold();
        }
    }
}

pub fn is_stopped(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DownloadStopped>().is_some()
}

pub fn is_pause_abort(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DownloadPaused>().is_some()
}

pub fn check_stop(snapshot: Option<&JobSnapshot>) -> Result<()> {
    if snapshot.is_some_and(JobSnapshot::should_stop) {
        anyhow::bail!(DownloadStopped);
    }
    Ok(())
}

pub fn abort_if_paused(snapshot: Option<&JobSnapshot>) -> Result<()> {
    check_stop(snapshot)?;
    if snapshot.is_some_and(JobSnapshot::is_paused) {
        anyhow::bail!(DownloadPaused);
    }
    Ok(())
}

/// Waits out a pause, giving the job's download slot back meanwhile and
/// taking one again (in list order) before returning.
pub async fn wait_while_paused(snapshot: Option<&JobSnapshot>) -> Result<()> {
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    loop {
        if snapshot.should_stop() {
            anyhow::bail!(DownloadStopped);
        }
        if snapshot.is_paused() {
            snapshot.release_slot();
            tokio::time::sleep(Duration::from_millis(150)).await;
            continue;
        }
        return acquire_slot(snapshot).await;
    }
}

/// Waits for a download slot. Shows "queued" only while actually waiting
/// and puts the previous status back once the slot is taken; paused time is
/// spent outside the line. Fails with [`DownloadStopped`] on stop.
pub async fn acquire_slot(snapshot: &JobSnapshot) -> Result<()> {
    let mut before: Option<String> = None;
    let taken = |before: Option<String>| {
        if let Some(previous) = before {
            if snapshot.status() == QUEUED {
                snapshot.set_status(previous);
            }
        }
    };
    loop {
        if snapshot.should_stop() {
            anyhow::bail!(DownloadStopped);
        }
        // Cheap path first: this runs per merge chunk through honor_control.
        if snapshot.poll_slot() {
            taken(before);
            return Ok(());
        }
        // Armed, then checked again, so a slot freed in between wakes us.
        let slots = snapshot.slots();
        let wake = slots.as_ref().map(|slots| slots.notified());
        if snapshot.poll_slot() {
            taken(before);
            return Ok(());
        }
        if !snapshot.is_paused() {
            let status = snapshot.status();
            if status != QUEUED {
                before.get_or_insert(status);
                snapshot.set_status(QUEUED);
            }
        }
        let tick = tokio::time::sleep(Duration::from_millis(100));
        match wake {
            Some(wake) => {
                tokio::select! {
                    _ = wake => {}
                    _ = tick => {}
                }
            }
            None => tick.await,
        }
    }
}

/// A job waiting for a download slot.
pub const QUEUED: &str = "queued";

/// Text shown for a job that failed because the disk filled up.
pub const DISK_FULL: &str = "disk full: free some space, then restart";

/// The download failed because the disk (or the user's quota) is full.
/// librqbit hands its write errors over as text, so that is matched too.
pub fn is_disk_full(error: &anyhow::Error) -> bool {
    let io_full = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(io_error_is_disk_full)
    });
    io_full || text_is_disk_full(&format!("{error:#}"))
}

/// The text of a failed job says it ran out of disk space.
pub fn text_is_disk_full(text: &str) -> bool {
    if text.contains(DISK_FULL) {
        return true;
    }
    // The OS's own wording and error number only (librqbit passes write
    // errors on as text): the same numbers mean other things elsewhere
    // (Linux errno 112 is "host is down").
    let text = text.to_ascii_lowercase();
    #[cfg(not(windows))]
    let needles = ["no space left on device", "(os error 28)"];
    #[cfg(windows)]
    let needles = [
        "not enough space on the disk",
        "(os error 112)",
        "(os error 39)",
    ];
    needles.iter().any(|needle| text.contains(needle))
}

fn io_error_is_disk_full(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::StorageFull {
        return true;
    }
    match error.raw_os_error() {
        // ENOSPC everywhere but Windows (macOS, Linux, Android, iOS).
        #[cfg(not(windows))]
        Some(28) => true,
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL.
        #[cfg(windows)]
        Some(39 | 112) => true,
        _ => false,
    }
}

/// Resolves once the job is asked to stop or pause; never without a snapshot.
/// Race only network waits against this, never file I/O: a dropped tokio file
/// op keeps writing in the background.
pub async fn until_stop_or_pause(snapshot: Option<&JobSnapshot>) {
    let Some(snapshot) = snapshot else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if snapshot.should_stop() || snapshot.is_paused() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn until_stop(snapshot: Option<&JobSnapshot>) {
    let Some(snapshot) = snapshot else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if snapshot.should_stop() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn honor_control(snapshot: Option<&JobSnapshot>) -> Result<()> {
    wait_while_paused(snapshot).await
}

pub async fn watch_cli_controls(snapshot: Arc<JobSnapshot>) {
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        match stdin.read_line(&mut line).await {
            // No terminal (a script, cron, `</dev/null`): there will be no
            // keys, so keep the download going; only "2" stops it.
            Ok(0) | Err(_) => std::future::pending::<()>().await,
            Ok(_) => match line.trim() {
                "1" => {
                    snapshot.toggle_pause();
                    let state = if snapshot.is_paused() {
                        "paused"
                    } else {
                        "resumed"
                    };
                    eprintln!("\n{state}  [1] pause/resume  [2] stop");
                }
                "2" => {
                    snapshot.request_stop();
                    eprintln!("\nstopping…");
                    return;
                }
                _ => {}
            },
        }
    }
}

/// CLI progress bar plus optional GUI snapshot. Cloneable for parallel HTTP parts.
#[derive(Clone)]
pub struct ProgressHandle {
    bar: ProgressBar,
    snapshot: Option<Arc<JobSnapshot>>,
    announce: bool,
    /// Set for one file of a multi-file job: the bar counts this file, the
    /// snapshot shows `base + position` of `job_total`.
    file: Option<FileScope>,
}

#[derive(Clone, Copy)]
struct FileScope {
    base: u64,
    job_total: u64,
}

impl ProgressHandle {
    pub fn cli(total: u64) -> Self {
        let bar = if cfg!(test) {
            hidden_bar(total.max(1))
        } else {
            new_bar(total.max(1))
        };
        Self {
            bar,
            snapshot: Some(Arc::new(JobSnapshot::default())),
            announce: !cfg!(test),
            file: None,
        }
    }

    pub fn hidden() -> Self {
        Self {
            bar: hidden_bar(1),
            snapshot: None,
            announce: false,
            file: None,
        }
    }

    pub fn gui(total: u64, snapshot: Arc<JobSnapshot>) -> Self {
        let downloaded = snapshot.downloaded.load(Ordering::Relaxed);
        let snap_total = snapshot.total.load(Ordering::Relaxed);
        let length = snap_total.max(total).max(1);
        let bar = hidden_bar(length);
        bar.set_position(downloaded.min(length));
        if snap_total == 0 && total > 1 {
            snapshot.total.store(total, Ordering::Relaxed);
        }
        Self {
            bar,
            snapshot: Some(snapshot),
            announce: false,
            file: None,
        }
    }

    /// One file of a multi-file job. `base` is bytes already done in earlier
    /// files; the job's `downloaded`/`total` keep counting the whole job.
    pub fn job_file(snapshot: Arc<JobSnapshot>, base: u64, job_total: u64, size: u64) -> Self {
        let handle = Self {
            bar: hidden_bar(size.max(1)),
            snapshot: Some(snapshot),
            announce: false,
            file: Some(FileScope { base, job_total }),
        };
        handle.sync();
        handle
    }

    pub fn announce(&self) -> bool {
        self.announce
    }

    pub fn snapshot(&self) -> Option<&Arc<JobSnapshot>> {
        self.snapshot.as_ref()
    }

    pub fn set_title(&self, title: impl Into<String>) {
        if self.file.is_some() {
            return;
        }
        if let Some(snapshot) = &self.snapshot {
            snapshot.set_title(title);
        }
    }

    pub fn should_stop(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.should_stop())
    }

    pub fn is_paused(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.is_paused())
    }

    pub fn set_position(&self, n: u64) {
        self.bar.set_position(n);
        self.sync();
    }

    pub fn set_length(&self, n: u64) {
        self.bar.set_length(n.max(1));
        self.sync();
    }

    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
        self.sync();
    }

    pub fn set_status(&self, value: impl Into<String>) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.set_status(value);
        }
    }

    pub fn set_uploaded(&self, n: u64) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.uploaded.store(n, Ordering::Relaxed);
        }
    }

    pub fn set_upload_bps(&self, bps: f64) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.upload_bps.store(whole_bps(bps), Ordering::Relaxed);
        }
    }

    pub fn set_download_bps(&self, bps: f64) {
        if let Some(snapshot) = &self.snapshot {
            snapshot
                .download_bps
                .store(whole_bps(bps), Ordering::Relaxed);
        }
    }

    pub fn use_seed_style(&self) {
        self.bar.set_style(seed_progress_style());
    }

    /// Back to the download bar (a seeding torrent got a new file ticked).
    pub fn use_download_style(&self) {
        self.bar.set_style(progress_style());
    }

    pub fn set_seed_message(&self, peers: u64, uploaded: u64, up_speed: f64) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.peers.store(peers, Ordering::Relaxed);
        }
        self.bar.set_message(format!(
            "{peers} | U: {} @ {}",
            format_size(uploaded),
            format_speed(up_speed)
        ));
    }

    pub fn set_message(&self, message: impl Into<String>) {
        let message = message.into();
        if let Ok(peers) = message.parse::<u64>() {
            if let Some(snapshot) = &self.snapshot {
                snapshot.peers.store(peers, Ordering::Relaxed);
            }
        }
        self.bar.set_message(message);
    }

    pub fn length(&self) -> Option<u64> {
        self.bar.length()
    }

    pub fn finish(&self) {
        self.bar.finish();
        if let Some(snapshot) = &self.snapshot {
            if self.file.is_none() {
                snapshot.mark_finished();
            }
            self.sync();
        }
    }

    pub fn finish_and_clear(&self) {
        self.bar.finish_and_clear();
        self.sync();
    }

    fn sync(&self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if let Some(scope) = self.file {
            let position = scope.base + self.bar.position();
            snapshot.downloaded.store(position, Ordering::Relaxed);
            snapshot
                .total
                .store(scope.job_total.max(position), Ordering::Relaxed);
            return;
        }
        snapshot
            .downloaded
            .store(self.bar.position(), Ordering::Relaxed);
        snapshot
            .total
            .store(self.bar.length().unwrap_or(0), Ordering::Relaxed);
    }
}

pub fn format_eta(eta: Option<Duration>) -> String {
    let Some(eta) = eta else {
        return "N/A".to_string();
    };
    if eta == Duration::MAX {
        return "N/A".to_string();
    }

    let seconds = eta.as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let weeks = days / 7;
    let months = (weeks as f64 / 4.345) as u64;
    let years = months / 12;

    fn plural(n: u64, unit: &str) -> String {
        if n == 1 {
            format!("1 {unit}")
        } else {
            format!("{n} {unit}s")
        }
    }

    if years > 0 {
        plural(years, "year")
    } else if months > 0 {
        plural(months, "month")
    } else if weeks > 0 {
        plural(weeks, "week")
    } else if days > 0 {
        plural(days, "day")
    } else if hours > 0 {
        plural(hours, "hour")
    } else if minutes > 0 {
        plural(minutes, "minute")
    } else {
        plural(seconds, "second")
    }
}

pub fn format_speed(speed: f64) -> String {
    if !speed.is_finite() || speed <= 0.0 {
        return "0.00 B/s".to_string();
    }
    let level = (speed.log(1024.0).floor() as i32).clamp(0, 3);
    match level {
        0 => format!("{speed:.2} B/s"),
        1 => format!("{:.2} KB/s", speed / 1024.0),
        2 => format!("{:.2} MB/s", speed / (1024.0 * 1024.0)),
        _ => format!("{:.2} GB/s", speed / (1024.0 * 1024.0 * 1024.0)),
    }
}

pub fn format_size(size: u64) -> String {
    if size == 0 {
        return "0.00 B".to_string();
    }
    let size = size as f64;
    let level = (size.log(1024.0).floor() as i32).clamp(0, 3);
    match level {
        0 => format!("{size:.2} B"),
        1 => format!("{:.2} KB", size / 1024.0),
        2 => format!("{:.3} MB", size / (1024.0 * 1024.0)),
        _ => format!("{:.4} GB", size / (1024.0 * 1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resume_returns_to_the_waiting_phase_it_paused() {
        let snapshot = JobSnapshot::default();
        snapshot.set_status(FETCHING_METADATA);
        snapshot.toggle_pause();
        assert_eq!(snapshot.status(), "paused");
        snapshot.toggle_pause();
        assert_eq!(snapshot.status(), FETCHING_METADATA);
        // Once data flows, a pause and resume reads "downloading" as before.
        snapshot.set_status("downloading");
        snapshot.toggle_pause();
        snapshot.toggle_pause();
        assert_eq!(snapshot.status(), "downloading");
        snapshot.set_status(CONNECTING_TO_PEERS);
        snapshot.toggle_pause();
        snapshot.toggle_pause();
        assert_eq!(snapshot.status(), CONNECTING_TO_PEERS);
    }

    use super::*;
    use crate::slots::Slots;

    #[test]
    fn other_errors_with_the_same_numbers_are_not_disk_full() {
        // Linux errno 112 is EHOSTDOWN; Windows error 28 is unrelated too.
        #[cfg(not(windows))]
        let other = "error sending request: Host is down (os error 112)";
        #[cfg(windows)]
        let other = "The printer is out of paper. (os error 28)";
        assert!(!is_disk_full(&anyhow::anyhow!(other)));
    }

    #[test]
    fn disk_full_is_recognised_from_io_errors_and_librqbit_text() {
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert!(is_disk_full(
            &anyhow::Error::from(full).context("writing part 2")
        ));
        #[cfg(not(windows))]
        let raw = std::io::Error::from_raw_os_error(28);
        #[cfg(windows)]
        let raw = std::io::Error::from_raw_os_error(112);
        assert!(is_disk_full(&anyhow::Error::from(raw)));
        // librqbit hands write errors over as text.
        #[cfg(not(windows))]
        let text =
            "fatal error: error writing chunk to disk: No space left on device (os error 28)";
        #[cfg(windows)]
        let text = "fatal error: error writing chunk to disk: There is not enough space on the disk. (os error 112)";
        assert!(is_disk_full(&anyhow::anyhow!(text)));
        assert!(is_disk_full(&anyhow::anyhow!(DISK_FULL)));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_disk_full(&anyhow::Error::from(denied)));
        assert!(!is_disk_full(&anyhow::anyhow!("connection reset by peer")));
    }

    #[tokio::test]
    async fn a_pause_gives_the_slot_to_the_next_job_and_resume_waits_its_turn() {
        let slots = Slots::new(1);
        let running = Arc::new(JobSnapshot::default());
        running.attach_slot(Arc::clone(&slots), 1);
        running.set_status("downloading");
        acquire_slot(&running).await.unwrap();
        let next = Arc::new(JobSnapshot::default());
        next.attach_slot(Arc::clone(&slots), 2);
        assert!(!next.poll_slot(), "cap 1 is taken");

        running.toggle_pause();
        let waiter = Arc::clone(&running);
        let paused = tokio::spawn(async move { wait_while_paused(Some(&waiter)).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(next.poll_slot(), "the paused job gave its slot back");

        running.toggle_pause();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!paused.is_finished(), "resumed, but the slot is taken");
        assert_eq!(running.status(), QUEUED);
        next.detach_slot();
        tokio::time::timeout(Duration::from_secs(2), paused)
            .await
            .expect("takes the freed slot")
            .unwrap()
            .unwrap();
        assert_eq!(running.status(), "downloading");
        assert_eq!(slots.active(), 1);
    }

    #[tokio::test]
    async fn a_stop_while_waiting_for_a_slot_ends_the_wait() {
        let slots = Slots::new(1);
        let first = JobSnapshot::default();
        first.attach_slot(Arc::clone(&slots), 1);
        assert!(first.poll_slot());
        let second = Arc::new(JobSnapshot::default());
        second.attach_slot(Arc::clone(&slots), 2);
        let waiter = Arc::clone(&second);
        let wait = tokio::spawn(async move { acquire_slot(&waiter).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        second.request_stop();
        let result = tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err_and(|error| is_stopped(&error)));
    }

    #[test]
    fn a_disk_full_hold_lands_before_the_slot_is_given_back() {
        let slots = Slots::new(1);
        let failing = JobSnapshot::default();
        failing.attach_slot(Arc::clone(&slots), 1);
        assert!(failing.poll_slot());
        let waiting = JobSnapshot::default();
        waiting.attach_slot(Arc::clone(&slots), 2);
        assert!(!waiting.poll_slot());
        failing.hold_queue();
        failing.detach_slot();
        assert!(slots.held());
        assert!(!waiting.poll_slot(), "held: the freed slot is not taken");
        slots.lift();
        assert!(waiting.poll_slot());
    }

    #[test]
    fn formats_zero_and_known_sizes() {
        assert_eq!(format_size(0), "0.00 B");
        assert_eq!(format_size(512), "512.00 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.000 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0000 GB");
    }

    #[test]
    fn formats_speed_and_eta_units() {
        assert_eq!(format_speed(0.0), "0.00 B/s");
        assert_eq!(format_speed(f64::NAN), "0.00 B/s");
        assert_eq!(format_speed(1024.0), "1.00 KB/s");
        assert_eq!(format_eta(None), "N/A");
        assert_eq!(format_eta(Some(Duration::MAX)), "N/A");
        assert_eq!(format_eta(Some(Duration::from_secs(1))), "1 second");
        assert_eq!(format_eta(Some(Duration::from_secs(2))), "2 seconds");
        assert_eq!(format_eta(Some(Duration::from_secs(60))), "1 minute");
        assert_eq!(format_eta(Some(Duration::from_secs(3600))), "1 hour");
        assert_eq!(format_eta(Some(Duration::from_secs(86400))), "1 day");
    }

    #[test]
    fn progress_template_is_valid() {
        let bar = new_bar(100);
        bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        bar.set_position(50);
        bar.set_message("3");
        let _ = bar.elapsed();
        bar.finish_and_clear();
        let seed = ProgressBar::hidden();
        seed.set_style(seed_progress_style());
        seed.set_length(100);
        seed.set_position(100);
        seed.set_message("2 | U: 1.00 KB @ 1.00 KB/s");
        seed.finish_and_clear();
    }

    #[test]
    fn cli_init_message_does_not_replace_verifying_status() {
        let snapshot = Arc::new(JobSnapshot::default());
        let progress = ProgressHandle::gui(1, Arc::clone(&snapshot));
        progress.set_status("verifying");
        progress.set_message("Initializing torrent client & verifying data...");
        assert_eq!(snapshot.status(), "verifying");
        progress.set_message("0");
        assert_eq!(snapshot.status(), "verifying");
        assert_eq!(snapshot.peers.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pause_and_stop_flags() {
        let snapshot = JobSnapshot::default();
        snapshot.toggle_pause();
        assert!(snapshot.is_paused());
        snapshot.toggle_pause();
        assert!(!snapshot.is_paused());
        snapshot.request_stop();
        assert!(snapshot.should_stop());
        assert!(snapshot.is_active());
        snapshot.set_title("Sintel");
        assert_eq!(snapshot.title(), "Sintel");
    }

    #[test]
    fn stop_makes_incomplete_jobs_inactive() {
        let snapshot = JobSnapshot::default();
        snapshot.set_status("downloading");
        snapshot.mark_stopped();
        assert!(!snapshot.is_active());
        assert!(snapshot.is_stopped());
        assert!(!snapshot.is_done());
        assert_eq!(snapshot.status(), "stopped");
    }

    #[test]
    fn finished_jobs_are_inactive_and_not_stopped() {
        let snapshot = JobSnapshot::default();
        assert!(snapshot.is_active());
        snapshot.mark_finished();
        assert!(!snapshot.is_active());
        assert!(!snapshot.is_stopped());
        assert_eq!(snapshot.status(), "done");
        assert!(snapshot.is_done());
        snapshot.mark_stopped();
        assert!(!snapshot.is_done());
        assert!(snapshot.is_stopped());
    }

    #[test]
    fn set_paused_and_stopped_with_message() {
        let snapshot = JobSnapshot::default();
        snapshot.set_paused(true);
        assert!(snapshot.is_paused());
        assert_eq!(snapshot.status(), "paused");
        snapshot.mark_stopped_with("files missing from download folder");
        assert!(snapshot.is_stopped());
        assert!(!snapshot.is_paused());
        assert_eq!(
            snapshot.error().as_deref(),
            Some("files missing from download folder")
        );
        assert_eq!(snapshot.status(), "stopped");
    }

    #[test]
    fn gui_handle_keeps_restored_totals() {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot
            .downloaded
            .store(25, std::sync::atomic::Ordering::Relaxed);
        snapshot
            .total
            .store(100, std::sync::atomic::Ordering::Relaxed);
        let _handle = ProgressHandle::gui(1, Arc::clone(&snapshot));
        assert_eq!(
            snapshot
                .downloaded
                .load(std::sync::atomic::Ordering::Relaxed),
            25
        );
        assert_eq!(
            snapshot.total.load(std::sync::atomic::Ordering::Relaxed),
            100
        );
    }

    #[test]
    fn pause_abort_does_not_sleep() {
        let snapshot = JobSnapshot::default();
        snapshot.toggle_pause();
        let err = abort_if_paused(Some(&snapshot)).unwrap_err();
        assert!(is_pause_abort(&err));
    }
}

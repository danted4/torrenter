use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncBufReadExt, BufReader};

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
#[derive(Debug)]
pub struct JobSnapshot {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub uploaded: AtomicU64,
    pub upload_bps: AtomicU64,
    pub peers: AtomicU64,
    pub finished: AtomicBool,
    paused: AtomicBool,
    stop: AtomicBool,
    status: Mutex<String>,
    title: Mutex<String>,
    error: Mutex<Option<String>>,
}

impl Default for JobSnapshot {
    fn default() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            upload_bps: AtomicU64::new(0),
            peers: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            status: Mutex::new(String::from("queued")),
            title: Mutex::new(String::new()),
            error: Mutex::new(None),
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
        let total = self.total.load(Ordering::Relaxed);
        let downloaded = self.downloaded.load(Ordering::Relaxed);
        let next = if was_paused {
            if total > 0 && downloaded >= total {
                "seeding"
            } else {
                "downloading"
            }
        } else {
            "paused"
        };
        self.set_status(next);
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

pub async fn wait_while_paused(snapshot: Option<&JobSnapshot>) -> Result<()> {
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    loop {
        if snapshot.should_stop() {
            anyhow::bail!(DownloadStopped);
        }
        if snapshot.is_paused() {
            tokio::time::sleep(Duration::from_millis(150)).await;
            continue;
        }
        return Ok(());
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
            Ok(0) => return,
            Err(_) => return,
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
            let bps = if bps.is_finite() && bps > 0.0 {
                bps.round() as u64
            } else {
                0
            };
            snapshot.upload_bps.store(bps, Ordering::Relaxed);
        }
    }

    pub fn use_seed_style(&self) {
        self.bar.set_style(seed_progress_style());
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
    use super::*;

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

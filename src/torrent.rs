use std::time::Duration;

use anyhow::{bail, Context, Result};
use indicatif::ProgressBar;
use librqbit::{AddTorrent, Session};

use crate::http::DOWNLOAD_DIRECTORY;
use crate::progress::new_bar;
use crate::validate::is_valid_magnet_url;

pub async fn start_download(magnet_uri: &str) -> Result<()> {
    if !is_valid_magnet_url(magnet_uri) {
        bail!("Invalid magnet URL!");
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Initializing torrent client & verifying data...");
    spinner.enable_steady_tick(Duration::from_millis(80));

    let session = Session::new(DOWNLOAD_DIRECTORY.into())
        .await
        .context("failed to start torrent session")?;

    let handle = session
        .add_torrent(AddTorrent::from_url(magnet_uri), None)
        .await
        .context("failed to add magnet")?
        .into_handle()
        .context("torrent was added in list-only mode")?;

    spinner.finish_and_clear();
    println!("\nTorrent is downloading...\n");
    println!("Progress\n");

    let progress = new_bar(1);
    progress.set_message("0");

    loop {
        let stats = handle.stats();
        if let Some(error) = stats.error {
            progress.finish_and_clear();
            bail!("{error}");
        }

        let total = stats.total_bytes.max(1);
        progress.set_length(total);
        progress.set_position(stats.progress_bytes.min(total));

        let peers = stats
            .live
            .as_ref()
            .map(|live| live.snapshot.peer_stats.live)
            .unwrap_or(0);
        progress.set_message(format!("{peers}"));

        if stats.finished || (stats.progress_bytes > 0 && stats.progress_bytes == stats.total_bytes)
        {
            progress.set_position(total);
            progress.finish();
            println!("\nTorrent download finished!");
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_magnet() {
        let err = start_download("https://example.com/not-a-magnet")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid magnet URL"));
    }

    #[tokio::test]
    async fn torrent_session_starts() {
        let dir = tempfile::tempdir().unwrap();
        Session::new(dir.path().to_path_buf())
            .await
            .expect("librqbit session should start");
    }
}

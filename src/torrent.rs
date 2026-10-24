use std::time::Duration;

use anyhow::{bail, Context, Result};
use indicatif::ProgressBar;
use librqbit::{AddTorrent, DhtSessionConfig, Session, SessionOptions};

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

    let session = Session::new_with_opts(
        DOWNLOAD_DIRECTORY.into(),
        SessionOptions {
            dht: Some(DhtSessionConfig {
                persistence: None,
                ..Default::default()
            }),
            persistence: None,
            ..Default::default()
        },
    )
    .await
    .context("failed to start torrent session")?;

    let result = run_torrent(&session, magnet_uri, spinner).await;
    session.stop().await;
    result
}

async fn run_torrent(
    session: &std::sync::Arc<Session>,
    magnet_uri: &str,
    spinner: ProgressBar,
) -> Result<()> {
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

    let completed = handle.wait_until_completed();
    tokio::pin!(completed);

    loop {
        tokio::select! {
            result = &mut completed => {
                if let Err(error) = result {
                    progress.finish_and_clear();
                    return Err(error).context("torrent failed");
                }
                let stats = handle.stats();
                let total = stats.total_bytes.max(1);
                progress.set_length(total);
                progress.set_position(total);
                progress.finish();
                println!("\nTorrent download finished!");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                let stats = handle.stats();
                if let Some(error) = stats.error {
                    progress.finish_and_clear();
                    bail!("{error}");
                }
                if !stats.finished {
                    let total = stats.total_bytes.max(1);
                    progress.set_length(total);
                    progress.set_position(stats.progress_bytes.min(total));
                    let peers = stats
                        .live
                        .as_ref()
                        .map(|live| live.snapshot.peer_stats.live)
                        .unwrap_or(0);
                    progress.set_message(format!("{peers}"));
                }
            }
        }
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
    async fn torrent_session_starts_without_dht_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new_with_opts(
            dir.path().to_path_buf(),
            SessionOptions {
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .expect("librqbit session should start");
        session.stop().await;
    }
}

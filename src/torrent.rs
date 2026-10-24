use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, DhtSessionConfig, Session, SessionOptions};

use crate::http::DOWNLOAD_DIRECTORY;
use crate::progress::{honor_control, is_stopped, watch_cli_controls, JobSnapshot, ProgressHandle};
use crate::validate::is_valid_magnet_url;

pub fn magnet_display_name(magnet_uri: &str) -> Option<String> {
    magnet_uri.split(['?', '&']).find_map(|part| {
        let value = part.strip_prefix("dn=")?;
        let decoded = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .ok()?
            .into_owned();
        let trimmed = decoded.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub async fn start_download(magnet_uri: &str) -> Result<()> {
    start_download_with(magnet_uri, ProgressHandle::cli(1)).await
}

pub async fn start_download_reported(magnet_uri: &str, snapshot: Arc<JobSnapshot>) -> Result<()> {
    start_download_with(magnet_uri, ProgressHandle::gui(1, snapshot)).await
}

pub async fn start_download_with(magnet_uri: &str, progress: ProgressHandle) -> Result<()> {
    if !is_valid_magnet_url(magnet_uri) {
        bail!("Invalid magnet URL!");
    }

    if let Some(name) = magnet_display_name(magnet_uri) {
        progress.set_title(name);
    }
    progress.set_message("Initializing torrent client & verifying data...");

    // Compilation torrents open many files at once; macOS defaults are often 256.
    let _ = librqbit::try_increase_nofile_limit();

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

    let result = run_torrent(&session, magnet_uri, progress).await;
    session.stop().await;
    result
}

async fn run_torrent(
    session: &std::sync::Arc<Session>,
    magnet_uri: &str,
    progress: ProgressHandle,
) -> Result<()> {
    let handle = session
        .add_torrent(
            AddTorrent::from_url(magnet_uri),
            Some(AddTorrentOptions {
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .context("failed to add magnet")?
        .into_handle()
        .context("torrent was added in list-only mode")?;

    if let Some(name) = handle.name() {
        progress.set_title(name);
    }

    if progress.announce() {
        println!("\nTorrent is downloading...\n");
        if let Some(snapshot) = progress.snapshot() {
            let title = snapshot.title();
            if !title.is_empty() {
                println!("{title}\n");
            }
        }
        println!("Progress\n");
        println!("[1] pause/resume   [2] stop\n");
    }
    progress.set_message("0");

    let completed = handle.wait_until_completed();
    tokio::pin!(completed);
    let controls = async {
        if progress.announce() {
            if let Some(snapshot) = progress.snapshot() {
                watch_cli_controls(Arc::clone(snapshot)).await;
            } else {
                std::future::pending::<()>().await;
            }
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(controls);

    let mut engine_paused = false;
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
                if progress.announce() {
                    println!("\nTorrent download finished!");
                }
                return Ok(());
            }
            _ = &mut controls => {
                if let Some(snapshot) = progress.snapshot() {
                    snapshot.request_stop();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if let Err(error) = honor_control(progress.snapshot().map(Arc::as_ref)).await {
                    if is_stopped(&error) {
                        let _ = session.pause(&handle).await;
                        if let Some(snapshot) = progress.snapshot() {
                            snapshot.mark_stopped();
                        }
                        progress.finish_and_clear();
                        if progress.announce() {
                            println!("\nTorrent download stopped.");
                        }
                        return Ok(());
                    }
                    return Err(error);
                }
                if let Some(name) = handle.name() {
                    progress.set_title(name);
                }
                let want_pause = progress.is_paused();
                if want_pause != engine_paused {
                    if want_pause {
                        session.pause(&handle).await.context("failed to pause torrent")?;
                    } else {
                        session.unpause(&handle).await.context("failed to resume torrent")?;
                    }
                    engine_paused = want_pause;
                }
                let stats = handle.stats();
                if let Some(error) = stats.error {
                    progress.finish_and_clear();
                    bail!("{error}");
                }
                if !stats.finished && !want_pause {
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

    #[test]
    fn reads_magnet_display_name() {
        let uri = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel";
        assert_eq!(magnet_display_name(uri).as_deref(), Some("Sintel"));
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

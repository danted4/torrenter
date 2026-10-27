use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use librqbit::{
    AddTorrent, AddTorrentOptions, DhtSessionConfig, ListenerOptions, Session, SessionOptions,
};

use crate::progress::{honor_control, is_stopped, watch_cli_controls, JobSnapshot, ProgressHandle};
use crate::validate::is_valid_magnet_url;

fn magnet_folder_lock(hash: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = map.get(hash).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(hash.to_string(), Arc::downgrade(&lock));
    lock
}

pub fn magnet_info_hash(magnet_uri: &str) -> Option<String> {
    magnet_uri.split(['?', '&']).find_map(|part| {
        let value = part.strip_prefix("xt=")?;
        let hash = value.rsplit(':').next()?;
        let trimmed = hash.trim();
        if trimmed.len() == 32 || trimmed.len() == 40 {
            Some(trimmed.to_ascii_lowercase())
        } else {
            None
        }
    })
}

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

const HASH_MARKER: &str = ".torrenter-hash";
const FOLDER_NAME_MAX: usize = 80;

fn sanitize_folder_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_whitespace() {
                '_'
            } else {
                match ch {
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' => '_',
                    ch if ch.is_control() => '_',
                    ch => ch,
                }
            }
        })
        .collect();
    let trimmed = cleaned
        .trim_matches(|ch: char| ch == '_' || ch == '.' || ch.is_whitespace())
        .chars()
        .take(FOLDER_NAME_MAX)
        .collect::<String>()
        .trim_end_matches('_')
        .to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let reserved = matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "LPT1"
            | "LPT2"
            | "LPT3"
    );
    if reserved {
        None
    } else {
        Some(trimmed)
    }
}

fn short_hash(hash: &str) -> &str {
    let len = hash.len().min(8);
    &hash[..len]
}

fn folder_marker(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path.join(HASH_MARKER)).ok()?;
    let value = String::from_utf8(bytes).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn write_folder_marker(path: &Path, hash: &str) -> Result<()> {
    std::fs::write(path.join(HASH_MARKER), hash)
        .with_context(|| format!("failed to mark download folder {}", path.display()))
}

fn folder_belongs_to(path: &Path, hash: &str) -> bool {
    path.is_dir() && folder_marker(path).as_deref() == Some(hash)
}

fn folder_candidates(base: &str, hash: &str) -> Vec<String> {
    let mut candidates = vec![base.to_string()];
    if base != hash {
        candidates.push(format!("{base}_{}", short_hash(hash)));
        candidates.push(format!("{base}_{hash}"));
    }
    candidates.push(hash.to_string());
    candidates.dedup();
    candidates
}

fn magnet_folder_base(magnet_uri: &str) -> Option<(String, String)> {
    let hash = magnet_info_hash(magnet_uri)?;
    let base = magnet_display_name(magnet_uri)
        .and_then(|name| sanitize_folder_name(&name))
        .unwrap_or_else(|| hash.clone());
    Some((base, hash))
}

pub fn magnet_sub_folder(dest: &Path, magnet_uri: &str) -> Option<String> {
    let (base, hash) = magnet_folder_base(magnet_uri)?;
    folder_candidates(&base, &hash)
        .into_iter()
        .find(|name| folder_belongs_to(&dest.join(name), &hash))
}

pub fn claim_magnet_sub_folder(dest: &Path, magnet_uri: &str) -> Result<String> {
    let (base, hash) = magnet_folder_base(magnet_uri).context("magnet has no info hash")?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create download folder {}", dest.display()))?;
    for name in folder_candidates(&base, &hash) {
        let path = dest.join(&name);
        if folder_belongs_to(&path, &hash) {
            return Ok(name);
        }
        match std::fs::create_dir(&path) {
            Ok(()) => {
                write_folder_marker(&path, &hash)?;
                return Ok(name);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if folder_belongs_to(&path, &hash) {
                    return Ok(name);
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create torrent folder {}", path.display())
                });
            }
        }
    }
    bail!(
        "could not reserve a torrent folder under {}",
        dest.display()
    );
}

pub async fn start_download(magnet_uri: &str, seed: bool, dest: PathBuf) -> Result<()> {
    start_download_with(magnet_uri, ProgressHandle::cli(1), seed, dest).await
}

pub async fn start_download_reported(
    magnet_uri: &str,
    snapshot: Arc<JobSnapshot>,
    seed: bool,
    dest: PathBuf,
) -> Result<()> {
    start_download_with(magnet_uri, ProgressHandle::gui(1, snapshot), seed, dest).await
}

pub async fn start_download_with(
    magnet_uri: &str,
    progress: ProgressHandle,
    seed: bool,
    dest: PathBuf,
) -> Result<()> {
    if !is_valid_magnet_url(magnet_uri) {
        bail!("Invalid magnet URL!");
    }

    if let Some(name) = magnet_display_name(magnet_uri) {
        progress.set_title(name);
    }
    let info_hash = magnet_info_hash(magnet_uri).context("magnet has no info hash")?;
    let folder_lock = magnet_folder_lock(&info_hash);
    let _folder = folder_lock.lock().await;
    progress.set_message("Initializing torrent client & verifying data...");

    // Compilation torrents open many files at once; macOS defaults are often 256.
    let _ = librqbit::try_increase_nofile_limit();

    let sub_folder = claim_magnet_sub_folder(&dest, magnet_uri)?;
    let session = Session::new_with_opts(
        dest.clone(),
        SessionOptions {
            dht: Some(DhtSessionConfig {
                persistence: None,
                ..Default::default()
            }),
            persistence: None,
            listen: seed.then(share_listen),
            disable_upload: !seed,
            ..Default::default()
        },
    )
    .await
    .context("failed to start torrent session")?;

    let result = run_torrent(&session, magnet_uri, progress, seed, sub_folder).await;
    session.stop().await;
    result
}

fn share_listen() -> ListenerOptions {
    ListenerOptions {
        listen_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        ..Default::default()
    }
}

fn live_peers_and_upload(stats: &librqbit::TorrentStats) -> (u64, u64, f64) {
    let peers = stats
        .live
        .as_ref()
        .map(|live| live.snapshot.peer_stats.live as u64)
        .unwrap_or(0);
    let uploaded = stats
        .live
        .as_ref()
        .map(|live| live.snapshot.uploaded_bytes.max(stats.uploaded_bytes))
        .unwrap_or(stats.uploaded_bytes);
    let up_speed = stats
        .live
        .as_ref()
        .map(|live| live.upload_speed.mbps * 1024.0 * 1024.0)
        .unwrap_or(0.0);
    (peers, uploaded, up_speed)
}

async fn run_torrent(
    session: &std::sync::Arc<Session>,
    magnet_uri: &str,
    progress: ProgressHandle,
    seed: bool,
    sub_folder: String,
) -> Result<()> {
    let handle = session
        .add_torrent(
            AddTorrent::from_url(magnet_uri),
            Some(AddTorrentOptions {
                overwrite: true,
                sub_folder: Some(sub_folder),
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
    progress.set_status("downloading");

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
    let mut download_done = false;
    loop {
        tokio::select! {
            result = &mut completed, if !download_done => {
                if let Err(error) = result {
                    progress.finish_and_clear();
                    return Err(error).context("torrent failed");
                }
                download_done = true;
                let stats = handle.stats();
                let total = stats.total_bytes.max(1);
                progress.set_length(total);
                progress.set_position(total);
                let (peers, uploaded, up_speed) = live_peers_and_upload(&stats);
                progress.set_uploaded(uploaded);
                progress.set_upload_bps(up_speed);
                if !seed {
                    progress.finish();
                    if progress.announce() {
                        println!("\nTorrent download finished!");
                    }
                    return Ok(());
                }
                progress.set_status("seeding");
                progress.use_seed_style();
                progress.set_seed_message(peers, uploaded, up_speed);
                if progress.announce() {
                    println!("\nTorrent download finished — seeding until you press 2.");
                }
            }
            _ = &mut controls => {
                if let Some(snapshot) = progress.snapshot() {
                    snapshot.request_stop();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if let Err(error) = honor_control(progress.snapshot().map(Arc::as_ref)).await {
                    if is_stopped(&error) {
                        if let Some(snapshot) = progress.snapshot() {
                            snapshot.mark_stopped();
                        }
                        let _ = session.pause(&handle).await;
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
                if !want_pause && (!stats.finished || seed) {
                    let total = stats.total_bytes.max(1);
                    let position = if stats.finished {
                        total
                    } else {
                        stats.progress_bytes.min(total)
                    };
                    progress.set_length(total);
                    progress.set_position(position);
                    let (peers, uploaded, up_speed) = live_peers_and_upload(&stats);
                    progress.set_uploaded(uploaded);
                    progress.set_upload_bps(up_speed);
                    if download_done && seed {
                        progress.set_status("seeding");
                        progress.set_seed_message(peers, uploaded, up_speed);
                    } else {
                        progress.set_status("downloading");
                        progress.set_message(format!("{peers}"));
                    }
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
        let err = start_download(
            "https://example.com/not-a-magnet",
            false,
            PathBuf::from("."),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Invalid magnet URL"));
    }

    #[test]
    fn reads_magnet_display_name() {
        let uri = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel";
        assert_eq!(magnet_display_name(uri).as_deref(), Some("Sintel"));
    }

    #[test]
    fn reads_magnet_info_hash() {
        assert_eq!(
            magnet_info_hash(
                "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel"
            )
            .as_deref(),
            Some("08ada5a7a6183aae1e09d831df6748d566095a10")
        );
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

    #[tokio::test]
    async fn torrent_session_starts_with_incoming_listen() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new_with_opts(
            dir.path().to_path_buf(),
            SessionOptions {
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                persistence: None,
                listen: Some(share_listen()),
                ..Default::default()
            },
        )
        .await
        .expect("librqbit session should start with a listen socket");
        assert!(session.listen_addr().is_some());
        session.stop().await;
    }

    #[tokio::test]
    async fn torrent_session_starts_with_upload_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new_with_opts(
            dir.path().to_path_buf(),
            SessionOptions {
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                persistence: None,
                disable_upload: true,
                ..Default::default()
            },
        )
        .await
        .expect("librqbit session should start with upload disabled");
        session.stop().await;
    }

    #[test]
    fn folder_name_replaces_whitespace_and_unsafe_chars() {
        assert_eq!(
            sanitize_folder_name("Sintel Movie").as_deref(),
            Some("Sintel_Movie")
        );
        assert_eq!(
            sanitize_folder_name("foo / bar:baz").as_deref(),
            Some("foo___bar_baz")
        );
        assert!(sanitize_folder_name("   ").is_none());
        assert!(sanitize_folder_name("..").is_none());
        assert!(sanitize_folder_name("CON").is_none());
    }

    #[test]
    fn magnet_folder_uses_name_then_hash_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "08ada5a7a6183aae1e09d831df6748d566095a10";
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Sintel Movie");
        let rival = format!("magnet:?xt=urn:btih:{other}&dn=Sintel Movie");
        assert_eq!(
            claim_magnet_sub_folder(dir.path(), &magnet).unwrap(),
            "Sintel_Movie"
        );
        assert_eq!(
            folder_marker(&dir.path().join("Sintel_Movie")).as_deref(),
            Some(hash)
        );
        assert_eq!(
            claim_magnet_sub_folder(dir.path(), &rival).unwrap(),
            "Sintel_Movie_aaaaaaaa"
        );
        assert_eq!(
            claim_magnet_sub_folder(dir.path(), &magnet).unwrap(),
            "Sintel_Movie"
        );
        assert_eq!(
            magnet_sub_folder(dir.path(), &magnet).as_deref(),
            Some("Sintel_Movie")
        );
        assert_eq!(
            magnet_sub_folder(dir.path(), &rival).as_deref(),
            Some("Sintel_Movie_aaaaaaaa")
        );
    }

    #[test]
    fn magnet_folder_falls_back_to_info_hash() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "08ada5a7a6183aae1e09d831df6748d566095a10";
        let magnet = format!("magnet:?xt=urn:btih:{hash}");
        assert_eq!(claim_magnet_sub_folder(dir.path(), &magnet).unwrap(), hash);
        assert_eq!(
            magnet_sub_folder(dir.path(), &magnet).as_deref(),
            Some(hash)
        );
    }
}

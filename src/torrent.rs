use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use librqbit::{
    AddTorrent, AddTorrentOptions, DhtSessionConfig, ListenerOptions, Session, SessionOptions,
    TorrentStatsState,
};

use crate::progress::{
    check_stop, is_stopped, output_lock, watch_cli_controls, JobSnapshot, ProgressHandle,
};
use crate::validate::is_valid_magnet_url;

/// Locks on the normalised hash so base32 and hex spellings of one torrent share it.
pub(crate) fn magnet_folder_lock(hash: &str) -> Arc<tokio::sync::Mutex<()>> {
    output_lock(&format!("magnet:{}", normalize_info_hash(hash)))
}

/// The info hash as written in the magnet (lowercased). Folder names derive from
/// this spelling, so it must stay stable for existing downloads.
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

/// 40-char lowercase hex identity of the torrent: use for comparisons, locks and
/// debrid cache checks, never for folder names.
pub fn magnet_info_hash_hex(magnet_uri: &str) -> Option<String> {
    magnet_info_hash(magnet_uri).map(|hash| normalize_info_hash(&hash))
}

/// Base32 (32 chars, RFC 4648) becomes hex; anything else is lowercased as-is.
pub(crate) fn normalize_info_hash(hash: &str) -> String {
    let lower = hash.trim().to_ascii_lowercase();
    if lower.len() != 32 {
        return lower;
    }
    let mut bits: u64 = 0;
    let mut nbits = 0u32;
    let mut out = String::with_capacity(40);
    for ch in lower.bytes() {
        let value = match ch {
            b'a'..=b'z' => ch - b'a',
            b'2'..=b'7' => ch - b'2' + 26,
            _ => return lower,
        };
        bits = (bits << 5) | u64::from(value);
        nbits += 5;
        while nbits >= 8 {
            nbits -= 8;
            out.push_str(&format!("{:02x}", (bits >> nbits) & 0xff));
        }
    }
    out
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
    path.is_dir()
        && folder_marker(path)
            .is_some_and(|marker| normalize_info_hash(&marker) == normalize_info_hash(hash))
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

pub fn magnet_payload_present(dest: &Path, magnet_uri: &str) -> bool {
    let Some(name) = magnet_sub_folder(dest, magnet_uri) else {
        return false;
    };
    folder_has_payload(&dest.join(name))
}

fn folder_has_payload(root: &Path) -> bool {
    const MAX_DIRS: usize = 256;
    const MAX_ENTRIES: usize = 4096;
    let mut pending = vec![root.to_path_buf()];
    let mut dirs = 0usize;
    let mut entries = 0usize;
    while let Some(dir) = pending.pop() {
        dirs += 1;
        if dirs > MAX_DIRS {
            return false;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            entries += 1;
            if entries > MAX_ENTRIES {
                return false;
            }
            if entry.file_name() == HASH_MARKER {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                return true;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    false
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
    if !progress.is_paused() {
        progress.set_status("verifying");
        progress.set_message("Initializing torrent client & verifying data...");
    }

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
        enable_upnp_port_forwarding: true,
        ..Default::default()
    }
}

fn live_peers_and_upload(stats: &librqbit::TorrentStats, session: &Session) -> (u64, u64, f64) {
    let session_stats = session.stats_snapshot();
    let peers = stats
        .live
        .as_ref()
        .map(|live| live.snapshot.peer_stats.live as u64)
        .unwrap_or(0);
    let torrent_uploaded = stats
        .live
        .as_ref()
        .map(|live| live.snapshot.uploaded_bytes.max(stats.uploaded_bytes))
        .unwrap_or(stats.uploaded_bytes);
    let uploaded = torrent_uploaded.max(session_stats.counters.uploaded_bytes);
    let torrent_up = stats
        .live
        .as_ref()
        .map(|live| live.upload_speed.as_bytes() as f64)
        .unwrap_or(0.0);
    let up_speed = torrent_up.max(session_stats.upload_speed.as_bytes() as f64);
    (peers, uploaded, up_speed)
}

fn apply_upload_stats(
    progress: &ProgressHandle,
    stats: &librqbit::TorrentStats,
    session: &Session,
) {
    let (peers, uploaded, up_speed) = live_peers_and_upload(stats, session);
    progress.set_uploaded(uploaded);
    progress.set_upload_bps(up_speed);
    if let Some(snapshot) = progress.snapshot() {
        snapshot
            .peers
            .store(peers, std::sync::atomic::Ordering::Relaxed);
    }
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

    if !progress.is_paused() {
        progress.set_status("verifying");
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

    let mut engine_paused = false;
    if progress.is_paused() {
        session
            .pause(&handle)
            .await
            .context("failed to pause torrent")?;
        engine_paused = true;
    }

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
                apply_upload_stats(&progress, &stats, session);
                if progress.is_paused() {
                    continue;
                }
                if !seed {
                    progress.finish();
                    if progress.announce() {
                        println!("\nTorrent download finished!");
                    }
                    return Ok(());
                }
                progress.set_status("seeding");
                progress.use_seed_style();
                let (peers, uploaded, up_speed) = live_peers_and_upload(&stats, session);
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
                if let Err(error) = check_stop(progress.snapshot().map(Arc::as_ref)) {
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
                apply_upload_stats(&progress, &stats, session);
                if !want_pause && download_done && !seed {
                    progress.finish();
                    if progress.announce() {
                        println!("\nTorrent download finished!");
                    }
                    return Ok(());
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
                    let (peers, uploaded, up_speed) = live_peers_and_upload(&stats, session);
                    let verifying = matches!(stats.state, TorrentStatsState::Initializing { .. });
                    if download_done && seed {
                        progress.set_status("seeding");
                        progress.set_seed_message(peers, uploaded, up_speed);
                    } else if verifying {
                        progress.set_status("verifying");
                    } else if !download_done {
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

    const HEX: &str = "c9e15763f722f23e98a29decdfae341b98d53056";
    const B32: &str = "zhqvoy7xelzd5gfctxwn7lrudomnkmcw";

    #[test]
    fn base32_and_hex_hashes_normalise_to_the_same_hex() {
        assert_eq!(normalize_info_hash(B32), HEX);
        assert_eq!(normalize_info_hash(&B32.to_ascii_uppercase()), HEX);
        assert_eq!(normalize_info_hash(HEX), HEX);
        assert_eq!(normalize_info_hash(&HEX.to_ascii_uppercase()), HEX);
        let b32 = format!("magnet:?xt=urn:btih:{}&dn=x", B32.to_ascii_uppercase());
        let hex = format!("magnet:?xt=urn:btih:{HEX}");
        assert_eq!(magnet_info_hash_hex(&b32).as_deref(), Some(HEX));
        assert_eq!(magnet_info_hash_hex(&hex).as_deref(), Some(HEX));
        // Folder naming keeps the magnet's own spelling.
        assert_eq!(magnet_info_hash(&b32).as_deref(), Some(B32));
    }

    #[test]
    fn folder_marker_matches_across_hash_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("x");
        std::fs::create_dir(&folder).unwrap();
        write_folder_marker(&folder, B32).unwrap();
        assert!(folder_belongs_to(&folder, HEX));
        assert!(folder_belongs_to(&folder, B32));
        assert!(!folder_belongs_to(&folder, "0".repeat(40).as_str()));
    }

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

    #[test]
    fn payload_present_ignores_marker_only_folder() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "08ada5a7a6183aae1e09d831df6748d566095a10";
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Sintel");
        let folder = claim_magnet_sub_folder(dir.path(), &magnet).unwrap();
        assert!(!magnet_payload_present(dir.path(), &magnet));
        std::fs::write(dir.path().join(&folder).join("movie.mkv"), b"data").unwrap();
        assert!(magnet_payload_present(dir.path(), &magnet));
        let nested = dir.path().join(&folder).join("inner");
        std::fs::remove_file(dir.path().join(&folder).join("movie.mkv")).unwrap();
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("clip.bin"), b"x").unwrap();
        assert!(magnet_payload_present(dir.path(), &magnet));
        assert!(!magnet_payload_present(
            dir.path(),
            "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
    }
}

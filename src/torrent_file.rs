//! `.torrent` files: parse one (through librqbit only), keep a copy beside
//! `jobs.json`, and give the job the magnet built from it. Every magnet-keyed
//! path (validation, duplicates, folder lock and name, persistence, debrid)
//! then works unchanged; the engine starts from the stored bytes when it has
//! them, so there is no metadata wait.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

use crate::fsutil::atomic_write;

/// Largest `.torrent` accepted (TorBox caps uploads at 100 MB; real files are
/// rarely over a few MB).
pub const MAX_TORRENT_FILE: usize = 32 * 1024 * 1024;

/// Trackers carried into the built magnet (debrid services and a restart
/// without the stored file use them).
const MAX_TRACKERS: usize = 20;

const STORE_DIR: &str = "torrents";

/// A parsed `.torrent`, ready to queue as its magnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// 40-char lowercase hex info hash (the job's identity).
    pub hash_hex: String,
    /// The torrent's name, or the hash when it has none.
    pub name: String,
    pub trackers: Vec<String>,
    /// `magnet:?xt=urn:btih:<hex>&dn=<name>&tr=…` (no `tr` when private).
    pub magnet: String,
    /// The tracker forbids DHT / peer exchange: never resolve it through the
    /// DHT, and keep its tracker URLs (they carry the user's passkey) out of
    /// the magnet.
    pub private: bool,
}

/// Parse `.torrent` bytes. Rejects empty, oversize and anything librqbit cannot
/// parse and validate (v2-only and corrupt files included).
pub fn parse(bytes: &[u8]) -> Result<Imported> {
    if bytes.is_empty() {
        bail!("the .torrent file is empty");
    }
    if bytes.len() > MAX_TORRENT_FILE {
        bail!(
            "the .torrent file is larger than {} MB",
            MAX_TORRENT_FILE / (1024 * 1024)
        );
    }
    let meta = librqbit::torrent_from_bytes(bytes)
        .map_err(|_| anyhow::anyhow!("not a valid .torrent file"))?;
    let hash_hex = meta.info_hash.as_string();
    let mut trackers: Vec<String> = Vec::new();
    for tracker in meta.iter_announce() {
        let Ok(text) = std::str::from_utf8(tracker.as_ref()) else {
            continue;
        };
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        let usable = ["http://", "https://", "udp://"]
            .iter()
            .any(|scheme| lower.starts_with(scheme));
        if usable && !trackers.iter().any(|seen| seen == text) {
            trackers.push(text.to_string());
            if trackers.len() == MAX_TRACKERS {
                break;
            }
        }
    }
    let private = meta.info.data.private;
    let info = meta
        .info
        .data
        .validate()
        .map_err(|_| anyhow::anyhow!("not a valid .torrent file"))?;
    let name = info
        .name()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| hash_hex.clone());
    let mut magnet = format!(
        "magnet:?xt=urn:btih:{hash_hex}&dn={}",
        utf8_percent_encode(&name, NON_ALPHANUMERIC)
    );
    if !private {
        for tracker in &trackers {
            magnet.push_str("&tr=");
            magnet.extend(utf8_percent_encode(tracker, NON_ALPHANUMERIC));
        }
    }
    Ok(Imported {
        hash_hex,
        name,
        trackers,
        magnet,
        private,
    })
}

/// Read a local `.torrent`, refusing anything over `MAX_TORRENT_FILE` before
/// reading it all (never librqbit's uncapped `from_local_filename`).
pub fn read_capped(path: &Path) -> Result<Vec<u8>> {
    // Only a regular file: a FIFO or device could block the read forever (MCP).
    let is_file = std::fs::metadata(path)
        .with_context(|| format!("could not open {}", path.display()))?
        .is_file();
    if !is_file {
        bail!("{} is not a file", path.display());
    }
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_TORRENT_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read {}", path.display()))?;
    if bytes.len() > MAX_TORRENT_FILE {
        bail!(
            "the .torrent file is larger than {} MB",
            MAX_TORRENT_FILE / (1024 * 1024)
        );
    }
    Ok(bytes)
}

/// Whether a CLI / MCP argument names a `.torrent` file rather than a magnet.
pub fn looks_like_file(arg: &str) -> bool {
    let arg = arg.trim();
    !arg.to_ascii_lowercase().starts_with("magnet:")
        && (arg.to_ascii_lowercase().ends_with(".torrent") || Path::new(arg).is_file())
}

/// Where stored copies live: `torrents/` beside `jobs.json`.
pub fn store_dir(jobs_path: &Path) -> PathBuf {
    jobs_path
        .parent()
        .map(|dir| dir.join(STORE_DIR))
        .unwrap_or_else(|| PathBuf::from(STORE_DIR))
}

fn stored_path(dir: &Path, hash_hex: &str) -> Option<PathBuf> {
    let valid = hash_hex.len() == 40 && hash_hex.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| dir.join(format!("{}.torrent", hash_hex.to_ascii_lowercase())))
}

/// Keep a copy under its info hash, so restart and resume start from it.
pub fn save(dir: &Path, imported: &Imported, bytes: &[u8]) -> Result<()> {
    let path = stored_path(dir, &imported.hash_hex).context("bad info hash")?;
    atomic_write(&path, bytes, false).with_context(|| format!("could not save {}", path.display()))
}

/// The stored copy for a hash, if it is there and still parses to that hash.
pub fn load(dir: &Path, hash_hex: &str) -> Option<Vec<u8>> {
    let path = stored_path(dir, hash_hex)?;
    let bytes = read_capped(&path).ok()?;
    let parsed = parse(&bytes).ok()?;
    (parsed.hash_hex.eq_ignore_ascii_case(hash_hex)).then_some(bytes)
}

/// Delete stored copies no job refers to. `keep` holds lowercase hex hashes.
pub fn prune(dir: &Path, keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".torrent"))
        else {
            continue;
        };
        if stored_path(dir, stem).is_some() && !keep.contains(&stem.to_ascii_lowercase()) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::torrent::{magnet_display_name, magnet_info_hash_hex};
    use crate::validate::is_valid_magnet_url;

    fn bstr(text: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", text.len()).into_bytes();
        out.extend_from_slice(text);
        out
    }

    /// The bencoded info dict of a one-piece single-file torrent.
    fn info_dict_with(name: &str, private: bool) -> Vec<u8> {
        let mut info = b"d".to_vec();
        info.extend(bstr(b"length"));
        info.extend(b"i5e");
        info.extend(bstr(b"name"));
        info.extend(bstr(name.as_bytes()));
        info.extend(bstr(b"piece length"));
        info.extend(b"i16384e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[7u8; 20]));
        if private {
            info.extend(bstr(b"private"));
            info.extend(b"i1e");
        }
        info.extend(b"e");
        info
    }

    /// A minimal single-file `.torrent`, with optional trackers.
    pub(crate) fn torrent_bytes(name: &str, trackers: &[&str]) -> Vec<u8> {
        torrent_bytes_with(name, trackers, false)
    }

    /// The same, marked private (`info.private = 1`).
    pub(crate) fn private_torrent_bytes(name: &str, trackers: &[&str]) -> Vec<u8> {
        torrent_bytes_with(name, trackers, true)
    }

    fn torrent_bytes_with(name: &str, trackers: &[&str], private: bool) -> Vec<u8> {
        let mut out = b"d".to_vec();
        if let Some(first) = trackers.first() {
            out.extend(bstr(b"announce"));
            out.extend(bstr(first.as_bytes()));
            out.extend(bstr(b"announce-list"));
            out.extend(b"l");
            for tracker in trackers {
                out.extend(b"l");
                out.extend(bstr(tracker.as_bytes()));
                out.extend(b"e");
            }
            out.extend(b"e");
        }
        out.extend(bstr(b"info"));
        out.extend(info_dict_with(name, private));
        out.extend(b"e");
        out
    }

    #[test]
    fn a_torrent_becomes_its_magnet() {
        let bytes = torrent_bytes(
            "Big Buck & Bunny.mkv",
            &[
                "udp://tracker.example:1337/announce",
                "https://t.example/a?x=1&y=2",
                "udp://tracker.example:1337/announce",
                "wss://not-for-us.example",
            ],
        );
        let imported = parse(&bytes).unwrap();
        // The hash is the info dict's: trackers do not change it, the name does.
        assert_eq!(imported.hash_hex.len(), 40);
        assert_eq!(
            parse(&torrent_bytes("Big Buck & Bunny.mkv", &[]))
                .unwrap()
                .hash_hex,
            imported.hash_hex
        );
        assert_ne!(
            parse(&torrent_bytes("other.mkv", &[])).unwrap().hash_hex,
            imported.hash_hex
        );
        assert_eq!(imported.name, "Big Buck & Bunny.mkv");
        assert_eq!(
            imported.trackers,
            [
                "udp://tracker.example:1337/announce",
                "https://t.example/a?x=1&y=2"
            ]
        );
        assert!(is_valid_magnet_url(&imported.magnet), "{}", imported.magnet);
        assert_eq!(
            magnet_info_hash_hex(&imported.magnet).as_deref(),
            Some(imported.hash_hex.as_str())
        );
        // The name survives the magnet exactly (it names the job folder).
        assert_eq!(
            magnet_display_name(&imported.magnet).as_deref(),
            Some("Big Buck & Bunny.mkv")
        );
        // A tracker's own `&` cannot split the magnet.
        assert!(!imported.magnet.contains("y=2"), "{}", imported.magnet);
    }

    #[test]
    fn private_torrents_keep_their_trackers_out_of_the_magnet() {
        let passkey = "https://tracker.example/announce?passkey=SECRET123";
        let imported = parse(&private_torrent_bytes("Private.mkv", &[passkey])).unwrap();
        assert!(imported.private);
        assert!(!imported.magnet.contains("tr="), "{}", imported.magnet);
        assert!(
            !imported.magnet.contains("SECRET123"),
            "{}",
            imported.magnet
        );
        assert!(is_valid_magnet_url(&imported.magnet));
        assert!(
            !parse(&torrent_bytes("Public.mkv", &[passkey]))
                .unwrap()
                .private
        );
    }

    #[test]
    fn trackers_are_capped() {
        let many: Vec<String> = (0..40)
            .map(|i| format!("udp://t{i}.example:80/announce"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let imported = parse(&torrent_bytes("a.bin", &refs)).unwrap();
        assert_eq!(imported.trackers.len(), MAX_TRACKERS);
    }

    #[test]
    fn garbage_empty_and_oversize_are_refused() {
        assert!(parse(b"").is_err());
        assert_eq!(
            parse(b"not bencode").unwrap_err().to_string(),
            "not a valid .torrent file"
        );
        // Bencoded but no info dict.
        assert!(parse(b"d8:announce3:urle").is_err());
        let huge = vec![b'd'; MAX_TORRENT_FILE + 1];
        assert!(parse(&huge)
            .unwrap_err()
            .to_string()
            .contains("larger than"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.torrent");
        std::fs::write(&path, &huge).unwrap();
        assert!(read_capped(&path)
            .unwrap_err()
            .to_string()
            .contains("larger than"));
        // Only regular files are read (a FIFO would block).
        assert!(read_capped(dir.path())
            .unwrap_err()
            .to_string()
            .contains("is not a file"));
    }

    #[test]
    fn stored_copies_are_saved_loaded_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_dir(&dir.path().join("jobs.json"));
        assert_eq!(store, dir.path().join("torrents"));
        let bytes = torrent_bytes("a.bin", &[]);
        let imported = parse(&bytes).unwrap();
        save(&store, &imported, &bytes).unwrap();
        assert_eq!(load(&store, &imported.hash_hex), Some(bytes.clone()));
        assert_eq!(
            load(&store, &imported.hash_hex.to_ascii_uppercase()),
            Some(bytes)
        );
        assert_eq!(load(&store, "../etc"), None);
        // A file that no longer matches its name is not used.
        let other = "0".repeat(40);
        std::fs::copy(
            store.join(format!("{}.torrent", imported.hash_hex)),
            store.join(format!("{other}.torrent")),
        )
        .unwrap();
        assert_eq!(load(&store, &other), None);
        std::fs::write(store.join("notes.txt"), b"keep").unwrap();
        prune(&store, &HashSet::from([imported.hash_hex.clone()]));
        assert!(store
            .join(format!("{}.torrent", imported.hash_hex))
            .exists());
        assert!(!store.join(format!("{other}.torrent")).exists());
        assert!(store.join("notes.txt").exists());
        prune(&store, &HashSet::new());
        assert!(!store
            .join(format!("{}.torrent", imported.hash_hex))
            .exists());
    }

    #[test]
    fn file_arguments_are_told_apart_from_magnets() {
        assert!(looks_like_file("~/Downloads/linux.torrent"));
        assert!(looks_like_file("C:\\x\\Linux.TORRENT"));
        assert!(!looks_like_file("magnet:?xt=urn:btih:abc&dn=x.torrent"));
        assert!(!looks_like_file("not-a-file-anywhere"));
    }
}

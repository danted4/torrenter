# torrenter

Terminal downloader with two modes. No GUI, no HTML, no CSS.

1. **Magnet / torrent** — BitTorrent via [librqbit](https://github.com/ikatson/rqbit) (peers, speed, ETA)
2. **Direct HTTP(S)** — **4-part parallel multipart download** with byte-range resume

Shared progress bar and `./downloads/` output. Cross-platform: Linux, macOS, Windows.

## Why this exists

You often need a **terminal downloader** without a GUI — whether the source is a **magnet link** or a **plain HTTPS file URL**.

## Two download modes

| | **1. Magnet / torrent** | **2. Direct HTTP(S)** |
|---|---------------------------|------------------------|
| **When to use** | `.torrent` / magnet P2P sources | CDN, mirror, or any direct file link |
| **Input** | `magnet:?xt=urn:btih:…` | `https://example.com/file.iso` |
| **Engine** | librqbit (BitTorrent P2P) | `reqwest` + `Range: bytes=` |
| **Parallelism** | Piece picker | **4 parallel byte-range parts** |
| **Resume** | librqbit built-in | Part files in `downloads/.resume/` |
| **Peers** | Shown in progress bar | `Peers: 0` |
| **Fallback** | — | Single-stream if the server has no ranges |

## Features

- Interactive menu or CLI subcommands
- Multipart HTTP when the server honors `Range`
- Resume interrupted direct downloads
- Magnet validation (32-char base32 and 40-char hex info hashes)
- Progress: size, speed, ETA, percentage, peers
- rustls TLS (no OpenSSL required)

## Install

Pre-built binaries are attached to [GitHub Releases](https://github.com/danted4/torrenter/releases) (Linux, macOS, Windows). Download the asset for your OS, make it executable on Unix, and put it on your `PATH`.

A tag `vX.Y.Z` on `main` builds those assets automatically.

From source:

```bash
git clone https://github.com/danted4/torrenter.git
cd torrenter
cargo build --release
```

The binary is `target/release/torrenter` (`.exe` on Windows).

```bash
# Linux / macOS
./target/release/torrenter

# Windows
.\target\release\torrenter.exe
```

## Usage

### Interactive

```
Choose download type:
1. Magnet URL
2. Direct URL (multi-part)
```

### Commands

```bash
torrenter magnet "magnet:?xt=urn:btih:…"
torrenter http "https://example.com/large-file.iso"
```

**Magnet:** connects to peers, writes to `./downloads/`, exits at 100%.

**HTTP with byte ranges:** splits into 4 parts, downloads in parallel, resumes from `downloads/.resume/` if interrupted, then merges into `./downloads/`.

**No range support:** single streamed download. This fallback **does not resume**; it truncates the destination and starts over.

Filename comes from `Content-Disposition` or the URL path (a single path component only; `.` / `..` become `download.bin`).

Torrent mode enables DHT in memory only (no DHT state written outside `./downloads`) and stops the session when the download finishes.

## Progress bar

```
[████████████████░░░░] 75% | Peers: 12 | D: 750.45 MB @ 2.5 MB/s | T: 1.0 GB | ETA: 2 minutes
```

## How it works

```mermaid
flowchart TD
  A[torrenter] --> B{1 or 2?}
  B -->|Magnet| C[librqbit Session]
  C --> F[./downloads]
  B -->|Direct URL| D{Known size?}
  D -->|Yes| E[4 parallel parts + resume + merge]
  D -->|No| G[Single HTTP stream]
  E -->|Server rejects 206| G
  E --> F
  G --> F
```

## Configuration

- Download folder: `./downloads` (`DOWNLOAD_DIRECTORY` in `src/http.rs`)
- Parallel parts: `PART_COUNT = 4` in `src/http.rs`
- Progress format: `BAR_TEMPLATE` in `src/progress.rs`

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

GitHub Actions runs fmt, clippy (`-D warnings`), and tests on **Linux, macOS, and Windows** for every push and pull request to `main`.

Pushing a `v*.*.*` tag whose commit is on `main` builds release binaries for Linux, macOS (Intel + Apple Silicon), and Windows.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `Invalid magnet URL` | Must start with `magnet:?xt=urn:` and include a 32+ character info hash |
| No peers / slow torrent | Wait for DHT; try a seeded magnet; check firewall/UDP |
| `Invalid HTTP(S) URL` | Full `http://` or `https://` URL required |
| Multipart not used | Server did not return HTTP 206 — single stream is used instead |
| Resume not working (HTTP) | Keep the same URL; state is in `downloads/.resume/` |
| Rust version error | Need a recent stable toolchain (`rustup update`) |

## Responsible use

Download only **content you have the legal right to access**. Generic tool — not affiliated with rqbit beyond using their OSS library.

## License

[MIT](LICENSE)

# Changelog

All notable changes to this project are documented here.

## [3.0.0] - 2026-10-24

### Changed

- Rewrote the CLI in **Rust** (replaces Node.js / WebTorrent)
- Magnet downloads now use **librqbit**; HTTP still uses 4-part Range resume
- Added `nodewt http <url>` and `nodewt magnet <uri>` subcommands
- TLS via rustls for Linux, macOS, and Windows without OpenSSL

### Added

- Unit and integration tests for validation, formatters, CLI parsing, HTTP multipart/resume/fallback
- GitHub Actions CI on pushes and PRs to `main` (fmt, clippy, tests on Linux/macOS/Windows)

## [2.2.0] - 2026-08-29

### Changed

- Renamed repository to **torrent-http-download-cli** to reflect **both** download modes equally (torrent + multipart HTTP)
- README repositioned: dual-mode tool, not WebTorrent-only; added "Two download modes" comparison table
- Updated `package.json` description and keywords (`http-download`, `direct-download`, `parallel-download`, `byte-range`)

## [2.1.0] - 2026-08-29

### Changed

- Renamed repository to **webtorrent-cli-client** (formerly **nodeWT**) for discoverability
- Rewrote README with SEO keywords, table of contents, and clearer usage docs
- Fixed `package.json` name (was incorrectly `webtorrent`), version, repository metadata, and keywords
- Added `LICENSE`, `CHANGELOG.md`, and `.nvmrc`

## [2.0.0] - 2026-08-23

### Added

- Direct HTTP(S) URL downloads with **4 parallel byte-range parts** when the server supports ranges
- Resume support for interrupted multipart downloads (`.downloads/.nodewt/`)
- Automatic fallback to single-stream download when ranges are unsupported

## [1.0.2] - 2025-11-08

### Changed

- Node.js 22 LTS support and dependency updates

## [1.0.1] - 2024-08-28

### Fixed

- Process exit and CLI prompt reliability

## [1.0.0] - 2023-10-09

### Added

- Initial WebTorrent magnet-link CLI with progress bar, peer count, speed, and ETA

[2.2.0]: https://github.com/danted4/torrent-http-download-cli/releases/tag/v2.2.0
[2.1.0]: https://github.com/danted4/torrent-http-download-cli/releases/tag/v2.1.0
[2.0.0]: https://github.com/danted4/torrent-http-download-cli/releases/tag/v2.0.0
[1.0.2]: https://github.com/danted4/torrent-http-download-cli/releases/tag/v1.0.2
[1.0.1]: https://github.com/danted4/torrent-http-download-cli/releases/tag/v1.0.1
[1.0.0]: https://github.com/danted4/torrent-http-download-cli/releases/tag/1.0.0

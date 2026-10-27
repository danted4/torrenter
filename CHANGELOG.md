# Changelog

All notable changes to this project are documented here.

## [3.2.1] - 2026-10-27

### Added

- GUI job search: filter downloading and finished rows by title, URL, folder, mode, or info-hash (`Esc` clears)
- Dock / taskbar icon is a squircle with transparent corners (and is embedded in the Windows GUI `.exe` so pinning keeps the glyph)
- macOS GitHub Releases also attach `torrenter-gui-*-apple-darwin.app.zip` (`torrenter.app` for Applications / Launchpad). The updater still uses the naked GUI Mach-O.

### Changed

- README Install documents the `.app.zip` drag-to-Applications path, macOS quarantine / Open Anyway, and Windows SmartScreen **Run anyway**

## [3.2.0] - 2026-10-26

### Added

- Download folder picker: default `<OS Downloads>/torrenter`, optional `download_dir` in config
- CLI menu `download:` path and **6. Change download folder**; `--dir` on `http` / `magnet` for a one-shot override
- GUI dest row under the URL field; restart confirm popup (mode + folder + START); click a job title to reveal the folder
- Bottom-right toasts for update checks and folder/IO errors

### Changed

- Output no longer uses `./downloads`; magnets isolate under `<dest>/<torrent-name>/` (spaces → `_`), falling back to the info-hash when the name is missing or collides
- Idle magnet rows use **restart** (glyphs are last-mode indicators); HTTP stopped jobs use the same popup minus seed glyphs

## [3.1.0] - 2026-10-25

### Added

- Magnet **share** mode (default): stay connected after 100% and keep uploading until Stop; incoming TCP listen on a random high port
- Magnet **leech** mode: never upload; stop the session at 100%
- CLI menu `seed: on/off` and **5. Toggle seed**; GUI halo/horns glyphs on the URL row and on idle magnet rows (Restart)
- Seeding progress shows **U:** upload size and speed; window/process must stay open to share

### Changed

- `torrenter magnet` in share mode does not exit when the download finishes (`2` or Ctrl-C to stop)

## [3.0.3] - 2026-10-25

### Changed

- GUI header: magnet/HTTP swap on the URL row, auto-update beside theme, pointer cursor on clickable controls, theme swatches that only hit the color squares
- Finished job rows use a brighter accent than in-progress rows; **clear** on inactive rows
- Manual update check shows **no updates found** for about 1.5s when already up to date

## [3.0.2] - 2026-10-24

### Added

- Native GUI (`torrenter-gui`) with phosphor / amber / ice themes, no HTML/CSS
- Release assets now ship CLI and GUI binaries per OS
- `danted4/torrenter vX.Y.Z` on the CLI banner, `--version`, and GUI header
- Opt-in auto-update from GitHub Releases (`--auto-update`, GUI checkbox, or menu item 3)
- Release `SHA256SUMS`; the updater verifies SHA-256, size, host, and timeouts before replacing the binary
- GUI jobs run concurrently (cap of 4) and are keyed by id

### Fixed

- HTTP pause now drops the Range body instead of sleeping on a live connection
- Stop is honored during HTTP metadata and merge
- Magnets isolate output under `./downloads/<info-hash>/` instead of overwriting a shared tree
- Update asset URLs must be `github.com/danted4/torrenter/releases/download/<tag>/<name>` with tag matching the release; cached foreign or mismatched-tag URLs are ignored
- Same magnet cannot run twice at once (GUI refuse + in-process folder lock)
- CLI startup auto-update is gated to an interactive TTY
- GitHub API 429 / cached ETag skip instead of failing the GUI
- Job snapshot mutex poison, pause toggle races, per-frame theme apply, UTC job dates

## [3.0.1] - 2026-10-24

### Fixed

- Magnet downloads raise the process file-descriptor limit and allow overwrite so large torrents can resume instead of failing with "too many open files"

## [3.0.0] - 2026-10-24

### Changed

- Rewrote the CLI in **Rust** (replaces Node.js / WebTorrent)
- Magnet downloads now use **librqbit**; HTTP still uses 4-part Range resume
- Added `torrenter http <url>` and `torrenter magnet <uri>` subcommands
- Renamed the project from nodeWT / torrent-http-download-cli to **torrenter**
- TLS via rustls for Linux, macOS, and Windows without OpenSSL
- HTTP resume state lives in `downloads/.resume/` (not `.nodewt`)
- HTTP resume truncates oversized parts, caps Range bodies, and merges exact byte counts
- Filenames cannot escape `./downloads`; torrent sessions stop cleanly with in-memory DHT only

### Added

- Unit and integration tests for validation, formatters, CLI parsing, HTTP multipart/resume/fallback
- GitHub Actions CI on pushes and PRs to `main` (fmt, clippy, tests on Linux/macOS/Windows)
- Release workflow: `v*.*.*` tags on `main` publish Linux, macOS, and Windows binaries

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

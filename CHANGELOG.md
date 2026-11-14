# Changelog

All notable changes to this project are documented here.

## [Unreleased]

### Added

- **Debrid route for magnet jobs** (desktop GUI, CLI, MCP, Android, iOS): a magnet can go through your own debrid service, which fetches it server-side; torrenter then downloads every file over HTTP into the same `<dest>/<torrent-name>/` folder. TorBox ships in this release. Real-Debrid, AllDebrid, Premiumize and Debrid-Link adapters are built and tested against fakes and ship once each is confirmed with a real account.
- Debrid keys: desktop `credentials.json` (owner-only) or `TORRENTER_<SERVICE>_KEY`; Android Keystore encryption (excluded from backup); iOS Keychain. `torrenter debrid add|test|remove|list`, `torrenter magnet --debrid <service>`, and MCP `queue_magnet` `debrid` plus a `debrid_services` tool.
- While the service fetches, rows read `MAGNET → service · fetching %` with an **as torrent** action; then `HTTP · via service` with `file i/n`. Optional **remove from service after download** (only torrents torrenter added). Clearing an unfinished debrid job asks whether to remove it from the service.
- **Connections per download** setting (Auto = 4, or 1–16) for new HTTP and debrid downloads, capped per service.
- Full **settings page** behind the cog on desktop, Android and iOS (theme, macOS tint, debrid accounts, connections, remove after download; iOS keep alive, desktop auto-update mirrored).

### Changed

- Theme moved from the mobile main screens and the desktop cog popup into the settings page.
- HTTP rows no longer show a peer count.
- `jobs.json` and `config.json` keep job kinds and keys written by newer builds instead of dropping them; `config.json` saves atomically and a file that no longer parses moves to `config.json.bak`.

### Fixed

- The same torrent spelled with a base32 and a hex info hash now counts as one job and shares one folder lock.
- Lowering the part count no longer leaves orphaned `.part-N` files in `.resume/`.

## [3.7.1] - 2026-11-14

### Added

- iOS **keep alive** switch (off by default): a silent, mixable audio loop keeps the app running in the background until every job finishes. Sideload-only; declares the `audio` background mode.

### Changed

- iOS no longer stops jobs the instant you leave: downloads continue for the ~20 s background grace window, then live jobs are parked and restart on their own when the app is reopened (including after iOS kills it). Jobs you paused stay stopped.

### Fixed

- Restarting an HTTP job now waits for the stopped task on the same URL and folder to let go of its parts and resume manifest, matching the existing per-hash magnet lock. Stop and pause let part downloads wind down and flush their last write instead of dropping them mid-write, and a stalled read no longer delays a stop.

## [3.7.0] - 2026-11-08

### Added

- CLI `torrenter mcp`: stdio MCP (`rmcp`) that owns the same Session as the desktop GUI (magnet/HTTP, cap 4, leech). Any stdio MCP host can spawn it; stdout is the protocol. Shares `jobs.json` with the GUI — do not run both at once.

### Fixed

- MCP restore strips share-mode `seed` from GUI `jobs.json` before any engine start, so a host that launches `torrenter mcp` does not upload.
- `restart` and `remove` mark the previous job snapshot stopped so the engine cannot keep writing after the row is replaced or dropped.

### Changed

- `rmcp` is a desktop CLI dependency (`src/mcp.rs` on the `torrenter` binary), not part of the UniFFI Android/iOS natives.

## [3.6.3] - 2026-11-08

### Changed

- Windows desktop GUI is opaque like Linux: no tint slider and no Mica/acrylic. Frost and the tint control stay macOS-only (DWM is not live glass). A leftover Windows `glass_tint` (including a slider value already saved) is rewritten to 100%.

## [3.6.2] - 2026-11-08

### Added

- iOS sideload shell (`ios/`): UniFFI Swift + SwiftUI, same engines as Android. Leech-only, foreground-only (Home stops live jobs), dest is `Documents/torrenter` (Files), `jobs.json` in Application Support. Build for your device with Mac + Xcode; no GitHub IPA. README title row includes iOS; install steps cover Xcode license, device support download, signing, Developer Mode, and trust.
- Host helper `cargo run --features ios-xcframework --bin ios-xcframework` writes Swift bindings and a device+simulator `TorrenterFFI.xcframework`.
- Patch `librqbit-dualstack-sockets` so iOS bind-to-interface matches macOS (`IP_BOUND_IF`) instead of Linux `SO_BINDTODEVICE`. Windows `new_from_name` prefixes the unused interface name so `RUSTFLAGS=-D warnings` still compiles.
- CI on macOS runs `ios-xcframework` then unsigned iOS Simulator XCTest (`FormatTests`, same size/speed/ETA strings as `src/progress.rs`).

### Fixed

- iOS header wraps `danted4/torrenter vX.Y.Z` onto a second line instead of truncating with an ellipsis (same as Android).

## [3.6.1] - 2026-11-07

### Added

- README shows the Android UI next to the CLI screenshot.

### Changed

- Android native/JNI build helper is `cargo run --features android-ndk --bin android-ndk` instead of `android/build-native.sh`.

### Fixed

- Android header shows `danted4/torrenter vX.Y.Z`, same banner as CLI and desktop GUI.

## [3.6.0] - 2026-11-07

### Added

- Shared download session (`src/session.rs`): queue cap 4, duplicate magnet/URL, persist/restore. Desktop GUI uses it.
- Optional `ffi` feature: UniFFI `Engine` for Kotlin (and later Swift). Android sideload app under `android/` (Compose, foreground service, `Download/torrenter`, all-files access).
- Android UI matches the desktop chrome: magnet launcher icon, theme swatches, MAGNET ⇄ HTTP, share/leech glyphs, and the same size/speed/ETA job line.
- README title row includes Android next to Linux; install steps, dest/jobs layout, and troubleshooting for the sideload APK.
- GitHub Releases attach `torrenter-android.apk` (debug-signed sideload, `arm64-v8a` + `x86_64`).

### Fixed

- Job download/upload rates are computed in the shared session, so Android (and any FFI client) shows the same D/U speeds as the desktop GUI.
- Magnet jobs copy librqbit upload totals/rates on every poll (including session counters), so Android share rows are not stuck at `U 0.00 B`.
- Share mode asks the LAN gateway to forward the listen port (UPnP) so peers can connect in; phones behind NAT otherwise finish at `U 0.00` with no inbound peers.
- Re-adding a magnet shows **verifying** (and the hash pass) until librqbit is live, then **downloading** from the real resume point.
- Android session tick (drain, throttled persist, FGS stop) lives on the `Application`, not the Compose loop. The notification service is `START_NOT_STICKY` and does not hydrate after a kill.
- CLI magnet progress again shows the initializing/verifying message on the bar.

## [3.5.4] - 2026-11-06

### Added

- GUI **violet** theme (light lilac ink)

## [3.5.3] - 2026-11-06

### Fixed

- Windows 11 uses light Mica (main-window backdrop). Dark Mica in 3.5.1 made 0% tint too dark; Win11 acrylic is a flyout type and after minimize/idle the taskbar can show a tiny shadowed pill that never restores until restart
- Installing-update toast stays as wide as its text; the progress bar matches that width instead of stretching the card

## [3.5.2] - 2026-11-06

### Added

- GUI **scarlet** theme (neon red ink)

## [3.5.1] - 2026-11-06

### Fixed

- Frosted labels and the settings cog sit on a light brush-stroke wash so 0% tint stays readable on a light wallpaper
- Long job-status and toast lines wrap under that wash instead of overflowing
- Windows uses Mica while focused (acrylic fallback on older Windows); DWM fills an inactive window with a solid theme color

### Changed

- Installing-update toast shows the same progress bar as job rows and cannot be dismissed until success or failure
- `torrenter update` shows the CLI byte bar while the asset downloads
- Release binaries use the HTTP Range transfer (gzip/brotli off, exact 206, merge via tmp) into a file beside the running exe, not dest/`.resume` (still a pinned GitHub first hop + `SHA256SUMS`)

## [3.5.0] - 2026-11-06

### Added

- GUI settings cog: theme swatches and a tint slider for the frosted window
- macOS/Windows GUI uses a translucent window with OS backdrop blur; phosphor / amber / ice still set the ink

### Changed

- Theme and tint live in the cog; the auto-update slider stays on the header beside it
- `glass_tint` is stored in `config.json` (40–210, default 60% of that range)
- Native fullscreen (macOS/Windows) locks tint at 100% without saving that override

## [3.4.3] - 2026-11-05

### Fixed

- Linux GUI **restart now** after an in-app update spawns a new process (and ignores a `(deleted)` `/proc/self/exe` path) instead of `exec` into the old Wayland/GL session

## [3.4.2] - 2026-11-05

### Fixed

- Updater retries GitHub 502/503/504 (and connect/read timeouts) when fetching `SHA256SUMS` or the binary
- Linux GUI: set Wayland/X11 app id `torrenter`, skip AccessKit, and do not vsync-block so compositor pings are answered (Wayland client decorations stay via winit’s defaults)

## [3.4.1] - 2026-11-05

### Changed

- GUI auto-update control is a plain label plus a track-and-knob slider (same check-on-enable behavior)
- Job rows: copy icon after the title copies that job’s magnet or HTTP URL
- Waiting/starting job bar ping-pongs left and right with eased motion instead of wrapping with a jump

## [3.4.0] - 2026-11-05

### Added

- GUI job list is stored in `jobs.json` (beside `config.json`) and restored on the next launch in the last live state (queued / downloading / paused / seeding), still capped at 4 running jobs
- Startup checks each row’s own dest: missing payload on a >0% magnet, or a live HTTP job without usable `.resume` parts, stays stopped until Restart or Clear

### Changed

- Closing the GUI still drops live torrent sessions; share-mode rows that were seeding start seeding again after the restore probe
- Title click still opens that row’s dest (not the folder used for new START jobs)

### Fixed

- Restored paused magnets pause librqbit before the control loop (in-session pause also reaches `session.pause` instead of blocking on `wait_while_paused` first)
- `jobs.json` is replaced by rename, not unlink-then-rename; a leftover `.tmp` / `.prev` is recovered if the live file is missing
- Restored in-progress rows no longer flash a full bar (`ProgressHandle` was writing total=1 over saved size); the bar slides until the engine reports real progress (~50ms cadence, not an unbounded repaint)

## [3.3.0] - 2026-10-31

### Added

- GUI theme (phosphor / amber / ice) is stored in `config.json` and restored on the next launch; missing or unknown values stay phosphor

## [3.2.3] - 2026-10-27

### Changed

- macOS `.app.zip` packaging is a feature-gated Rust binary (`macos-app`) instead of `scripts/macos-app.sh`
- In-app updates restamp `torrenter.app` `Info.plist` and ad-hoc codesign after replacing the inner binary; leftover `.old` / `.update` / `.sha256sums` files are removed on the next launch

## [3.2.2] - 2026-10-27

### Changed

- GUI toasts: duplicate-job / validation errors auto-dismiss after 5 seconds (click still dismisses). In-progress update notes and dest/update fatals stay until replaced or clicked.
- GUI **Stop** marks the row stopped immediately so **clear** (and **restart**) appear for queued or incomplete jobs, not only after a finished download.

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

- Native GUI (`torrenter-gui`) with phosphor / amber / ice themes
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

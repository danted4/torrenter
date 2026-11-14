# AGENTS.md

Guidance for coding agents working on this repository. Human-facing install, usage, behavior, and troubleshooting live in [README.md](README.md). Prefer linking that file over copying it.

## Project overview

`torrenter` is a single Rust crate (`edition = "2021"`, MSRV **1.85**) that ships desktop binaries from one library, plus in-repo Android and iOS apps:

| Binary | Path | Feature | Role |
|--------|------|---------|------|
| `torrenter` | `src/main.rs` | default | Interactive CLI + `magnet` / `http` / `update` / `mcp` subcommands |
| `torrenter-gui` | `src/bin/torrenter-gui.rs` | `gui` (`eframe` 0.31) | Native egui window |
| `macos-app` | `src/bin/macos-app.rs` | `macos-app` | Release-only: wrap the GUI Mach-O in `torrenter.app` and zip it. Not a user-facing binary. |
| `android-ndk` | `src/bin/android-ndk.rs` | `android-ndk` | Host helper: UniFFI Kotlin + Android `jniLibs`. Not a user-facing binary. |
| `ios-xcframework` | `src/bin/ios-xcframework.rs` | `ios-xcframework` | Host helper (macOS + full Xcode): UniFFI Swift + `TorrenterFFI.xcframework`. Not a user-facing binary. |
| `uniffi-bindgen` | `src/bin/uniffi-bindgen.rs` | `ffi-bindgen` | Generate Kotlin/Swift bindings. Not a user-facing binary. |

Both fronts call the same engines in `src/lib.rs`:

- **Magnet / torrent** — `src/torrent.rs` via [librqbit](https://github.com/ikatson/rqbit) 9 (DHT in memory, output under `<dest>/<sanitized-name>/`, hash fallback / suffix on collision). Share mode uploads and keeps the session until Stop; leech mode disables upload and calls `session.stop()` at 100%. `.torrent` files are not accepted.
- **Direct HTTP(S)** — `src/http.rs` via `reqwest` 0.12 + rustls, multi-part `Range` download with resume under `<dest>/.resume/`. Part count is the `connections` setting (unset = Auto = `PART_COUNT` = 4, 1–16), fixed per job at creation (`PersistedJob.partCount`); the updater keeps `PART_COUNT`.
- **Debrid route** — `src/debrid/` (one self-contained adapter per service: `torbox`, `realdebrid`, `alldebrid`, `premiumize`, `debridlink`; `run.rs` runner; `fake.rs` test server) plus `src/credentials.rs`. A magnet job with a service (`JobKind::Debrid`), **not a third mode**: add/reuse on the provider, poll, then each file through `http::download_job_file` into the same `<dest>/<sanitized-name>/` folder and folder lock as a plain magnet. Resume is keyed by `service|remote_id|file_id`, never a link. `Service::shipped()` lists only providers verified with a real account (TorBox); unverified adapters stay on `feat/debrid` until confirmed.
- **Download dest** — `src/paths.rs`. Default `<OS Downloads>/torrenter`; optional `download_dir` in config after an explicit pick. `--dir` is one-shot.
- **Session / queue** — `src/session.rs` (cap 4, duplicate hash/URL, persist, restore probes, Tokio engine). Desktop GUI, UniFFI, and `torrenter mcp` all use it. Updater install stays on the GUI worker only.
- **Progress / pause / stop** — `src/progress.rs` (`JobSnapshot`, `ProgressHandle`). CLI watches stdin (`1` pause, `2` stop); GUI and Android poll snapshots.
- **Android / UniFFI** — feature `ffi` (`src/ffi.rs`, `cdylib` + `staticlib`). Kotlin Compose app in `android/` (minSdk 30, arm64 + x86_64, FGS, public `Download/torrenter`, `MANAGE_EXTERNAL_STORAGE`). SwiftUI app in `ios/` (min iOS 16, leech-only, Documents dest, Application Support `jobs.json`, Home = grace window then park + auto-restart on return, opt-in **keep alive** silent-audio loop (`UIBackgroundModes` audio, sideload-only), no Release IPA). `TorrenterApp` / `EngineOwner` own `Engine` hydrate / drain / throttled persist. Android `DownloadService` is `START_NOT_STICKY` and must not open an engine after a kill. No Android/iOS types in the FFI. Build natives with `cargo run --features android-ndk --bin android-ndk` or `cargo run --features ios-xcframework --bin ios-xcframework`. Do not point `current_asset_name` at the APK or an IPA.
- **Opt-in updater** — `src/update.rs` from GitHub Releases + `SHA256SUMS`. CLI and GUI only replace their own asset family (`torrenter-*` vs `torrenter-gui-*`).
- **Validation / identity** — `src/validate.rs`, `src/identity.rs` (`danted4/torrenter vX.Y.Z` from `CARGO_PKG_VERSION`).

Product rules (magnet vs HTTP, debrid route, resume, GUI queue of 4, TTY-gated startup update, legal-use note) are specified in [README.md](README.md) sections **Two download modes**, **Debrid (optional)**, **Updates**, **How it works**, **Configuration**, **MCP**, and **Responsible use**. CLI subcommands sit under the CLI screenshot at the top of the README. The title row includes OS icons plus MCP. Do not invent extra modes or accept `.torrent` files.

## Build and test commands

Rust **1.85+** stable. `Cargo.lock` is committed; CI and release use `--locked`.

```bash
# CLI (default features)
cargo build
cargo build --release --bin torrenter

# GUI (Linux needs GTK 3 + xcb dev packages; see README Linux / Development)
cargo build --features gui --bin torrenter-gui
cargo build --release --features gui --bin torrenter-gui

# Checks that must stay green (match .github/workflows/ci.yml)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked --all-targets --all-features

# Focused tests
cargo test --locked --all-features <name_substring>

# iOS (macOS + full Xcode; CI job `ios xcframework and swift`)
cargo run --locked --features ios-xcframework --bin ios-xcframework
```

CI also sets `RUSTFLAGS=-D warnings`. Clippy and tests always use `--all-features` so `src/gui.rs` and `src/icon.rs` compile. On Linux, install the same GUI libs CI does before `--all-features` builds: `libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev pkg-config`. The iOS job rebuilds `TorrenterFFI.xcframework` and runs unsigned Simulator XCTest; it does not sign or install on a device.

Release (do not tag unless the user asks): `git tag vX.Y.Z && git push origin vX.Y.Z` only after the commit is on `main`. Workflow: `.github/workflows/release.yml`. Bump `version` in `Cargo.toml` and add a [CHANGELOG.md](CHANGELOG.md) entry when shipping a release.

After code changes, run the relevant `fmt` / `clippy` / `test` commands and fix failures before finishing.

## Tech stack guidelines

- **Language:** Rust 2021. Prefer `anyhow::{Result, bail, Context}` at binaries and engine boundaries. Keep `?` paths; do not add `unwrap()` on fallible I/O or network unless the existing pattern is a poisoned lock recovery (`into_inner()`).
- **Async:** Tokio multi-thread runtime. HTTP and torrent work is async; GUI (`eframe`) is sync and drives jobs through `JobSnapshot` / spawned tasks. Do not block the GUI thread on downloads.
- **TLS / HTTP:** `reqwest` with `rustls-tls` only (no native-tls). Timeouts stay 20s connect / 60s between reads unless there is a documented reason to change them. HTTP resume and merge rules in README **How it works** are invariants: no zero-padding short 206 bodies; merge via `.{filename}.merge.tmp` then rename; single-stream GET does not resume.
- **Torrent:** librqbit with `default-features = false` and `rust-tls` plus `disable-upload` so leech sessions can set `disable_upload`. DHT must stay in-memory (`DhtSessionConfig`). Share mode listens on an ephemeral TCP port and calls `session.stop()` only when the job is Stopped; leech mode never uploads and stops at 100%. Isolate output per sanitized display name (hash fallback); keep the in-process folder lock so the same hash cannot run twice. iOS uses `[patch.crates-io]` `patches/librqbit-dualstack-sockets` so bind-to-interface matches macOS (`IP_BOUND_IF`), not Linux `SO_BINDTODEVICE`.
- **GUI:** `eframe` / egui, gated with `#[cfg(feature = "gui")]`. Keep phosphor / amber / ice / scarlet / violet themes. The cog opens a full, scrollable settings page in place of the URL row and job list (theme, macOS tint, auto-update mirror, debrid accounts, connections, remove after download) and turns into a back arrow; the auto-update toggle stays on the header. Android and iOS use the same pattern (header cog → full settings screen). No settings popups. macOS uses a translucent window with vibrancy (`src/glass.rs`) and a tint slider on the settings page. Windows and Linux stay opaque — no tint control, no Mica/acrylic (DWM is not live frost; Win11 `apply_acrylic` / `TRANSIENTWINDOW` on the main window can restore as a tiny flyout pill). Labels on macOS glass keep a brush-stroke text wash. Cap concurrent jobs at 4; refuse a second active job with the same magnet info-hash or HTTP URL. Do not introduce a webview front.
- **CLI:** `clap` derive. Interactive menu remains the no-args path; new menu numbers must be matched before the `_ => magnet` arm. `torrenter debrid add|test|remove|list` returns early like `update`/`mcp`; keys come from `rpassword` (hidden) or piped stdin, never argv. Startup auto-update stays TTY-only; `torrenter update` always runs when requested. `torrenter mcp` is stdio MCP (`src/mcp.rs` on the CLI binary, `rmcp`): stdout is JSON-RPC only (no banner); logs on stderr. `Session::open_leech` so restore cannot resume GUI share-mode torrents. `restart` / `remove` `mark_stopped()` the old snapshot. Same `jobs.json` as the GUI; do not run GUI and mcp together. Do not put `rmcp` on the shared lib / Android / iOS natives. MCP may queue through debrid only with an explicit `debrid` argument (default off) and can never read or set keys (`debrid_services` reports names and `configured` only).
- **Serialization:** `serde` / `serde_json`. Resume manifests use `camelCase` (`ResumeManifest` in `src/http.rs`). Updater config is `~` platform config dir `torrenter/config.json` (see `update::config_path`). `config.json`, `jobs.json` and `credentials.json` are written with `fsutil::atomic_write`; unknown keys / job kinds from newer builds are kept on rewrite (never drop them), and an unparsable `config.json` is moved to `.bak` before defaults are written.
- **Logging:** `tracing-subscriber` with `EnvFilter`, default `error`, writer `stderr`. Do not log secrets or full magnet/query strings at info in new code.
- **Tests:** unit tests live in the same file under `#[cfg(test)]`. HTTP tests use `tempfile` and in-process servers where possible. Prefer `#[tokio::test]` for async. Do not hit real GitHub, public CDNs or debrid APIs from tests; each debrid adapter has a fake in its own file built on `src/debrid/fake.rs` that enforces the provider's documented formats and quirks.

## Code style

- rustfmt defaults (no `rustfmt.toml`). Match surrounding naming: `snake_case` functions, `SCREAMING_SNAKE` for module constants (`PART_COUNT`).
- Keep public surfaces small. New engine entry points should follow existing pairs: `start_*` (CLI) and `start_*_reported` (GUI + `JobSnapshot`).
- Feature-gate GUI-only modules the same way `src/lib.rs` does (`gui`, `icon`, `icon_raster`). MCP stays on `src/main.rs` (`mod mcp`), not the shared lib. The GUI binary stays a thin `main` that calls `torrenter::gui::run()`. On Windows, `build.rs` embeds the same glyph into the `.exe` so a pinned taskbar shortcut keeps the icon.
- User-visible strings already used in tests and the README (`Invalid magnet URL!`, `Invalid HTTP(S) URL!`, banner format) should stay stable unless the change is intentional and tests/docs update together.
- Version and asset names are derived from `CARGO_PKG_VERSION` and `identity::current_asset_name`. If you add an **updater** target, update identity, README asset table, and `.github/workflows/release.yml` together. macOS also zips `torrenter.app` via `cargo run --features macos-app --bin macos-app` as `torrenter-gui-*-apple-darwin.app.zip`; Android ships `torrenter-android.apk`. Do **not** point `current_asset_name` at the `.app.zip` or the APK.

## Security considerations

This tool downloads untrusted content the user asked for. Do not weaken path, URL, or updater checks.

- Magnet: only `magnet:?xt=urn:` with 32-char base32 or 40-char hex info hash (`validate.rs`). No `.torrent` file parse. Compare and lock on `magnet_info_hash_hex` (base32 normalised); folder names keep the magnet's own spelling.
- Debrid paths from a provider are untrusted: `debrid::safe_relative_path` (no `..`, absolute or drive paths; segments sanitised) plus `inside()` before any write.
- HTTP filenames come from `Content-Disposition` or a **single** URL path segment; `.` / `..` become `download.bin`. Never write outside the configured dest / info-hash / `.resume` trees. Do not fall back to `./downloads`.
- Updater (`src/update.rs`) must keep: pinned first hop `https://github.com/danted4/torrenter/releases/download/<tag>/<name>` with tag matching the release; SHA-256 against `SHA256SUMS`; size caps; timeouts; CLI vs GUI asset prefix. Binary bytes go through `http::download_file` (same Range probe / 206 / merge-tmp helpers, gzip/brotli off) into a staging file beside the running exe — not dest/`.resume`, no resume. Fall back to a single GET if the CDN ignores Range. `SHA256SUMS` is same-publisher integrity, not a signature — do not claim otherwise.
- Auto-update defaults **off**. Do not enable it by default in config or CI.
- Never ship or commit credentials, tokens, or tracking. Do not expand the updater to arbitrary hosts.
- User-provided debrid keys: desktop `credentials.json` (0600, atomic) or `TORRENTER_<SERVICE>_KEY` (read-only); Android AES-GCM with an Android Keystore key in its own prefs file excluded from backup; iOS Keychain (`AfterFirstUnlockThisDeviceOnly`). Mobile engines keep keys in memory only (`use_memory_keys`). Keys never go in `jobs.json`, logs, MCP output, `Debug` (`debrid::Secret`) or resume manifests.
- Debrid links can carry the key (TorBox `requestdl?token=`): never log or persist them; debrid error text goes through `debrid::redact` before it reaches `jobs.json`, the UI or MCP. Blocked content (e.g. RD `infringing_file`) is shown, never worked around.

## PR and commit notes

- Keep changes scoped. Do not add Node, web, or extra crates unless required for the task (`rpassword` is the CLI's hidden key prompt).
- Doc-only edits on `main` do not need a version tag (README **Development**).
- When behavior changes, update [README.md](README.md) and [CHANGELOG.md](CHANGELOG.md) in the same change. Keep AGENTS.md to agent workflow and invariants; do not duplicate README tables or the mermaid chart.
- Match existing commit tone: short, imperative, why-focused (e.g. fix resume, isolate magnet folders).

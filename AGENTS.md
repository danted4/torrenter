# AGENTS.md

Guidance for coding agents working on this repository. Human-facing install, usage, behavior, and troubleshooting live in [README.md](README.md). Prefer linking that file over copying it.

## Project overview

`torrenter` is a single Rust crate (`edition = "2021"`, MSRV **1.85**) that ships two binaries from one library:

| Binary | Path | Feature | Role |
|--------|------|---------|------|
| `torrenter` | `src/main.rs` | default | Interactive CLI + `magnet` / `http` / `update` subcommands |
| `torrenter-gui` | `src/bin/torrenter-gui.rs` | `gui` (`eframe` 0.31) | Native egui window (no HTML/CSS) |

Both fronts call the same engines in `src/lib.rs`:

- **Magnet / torrent** — `src/torrent.rs` via [librqbit](https://github.com/ikatson/rqbit) 9 (DHT in memory, output under `./downloads/<info-hash>/`). Share mode uploads and keeps the session until Stop; leech mode disables upload and calls `session.stop()` at 100%. `.torrent` files are not accepted.
- **Direct HTTP(S)** — `src/http.rs` via `reqwest` 0.12 + rustls, 4-part `Range` download with resume under `downloads/.resume/`.
- **Progress / pause / stop** — `src/progress.rs` (`JobSnapshot`, `ProgressHandle`). CLI watches stdin (`1` pause, `2` stop); GUI shares the same snapshot.
- **Opt-in updater** — `src/update.rs` from GitHub Releases + `SHA256SUMS`. CLI and GUI only replace their own asset family (`torrenter-*` vs `torrenter-gui-*`).
- **Validation / identity** — `src/validate.rs`, `src/identity.rs` (`danted4/torrenter vX.Y.Z` from `CARGO_PKG_VERSION`).

Product rules (magnet vs HTTP, resume, GUI queue of 4, TTY-gated startup update, legal-use note) are specified in [README.md](README.md) sections **Two download modes**, **Usage**, **Updates**, **How it works**, **Configuration**, and **Responsible use**. Do not invent extra modes or accept `.torrent` files.

## Build and test commands

Rust **1.85+** stable. `Cargo.lock` is committed; CI and release use `--locked`.

```bash
# CLI (default features)
cargo build
cargo build --release --bin torrenter

# GUI (Linux needs GTK 3 + xcb dev packages; see README Install)
cargo build --features gui --bin torrenter-gui
cargo build --release --features gui --bin torrenter-gui

# Checks that must stay green (match .github/workflows/ci.yml)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked --all-targets --all-features

# Focused tests
cargo test --locked --all-features <name_substring>
```

CI also sets `RUSTFLAGS=-D warnings`. Clippy and tests always use `--all-features` so `src/gui.rs` and `src/icon.rs` compile. On Linux, install the same GUI libs CI does before `--all-features` builds: `libgtk-3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev pkg-config`.

Release (do not tag unless the user asks): `git tag vX.Y.Z && git push origin vX.Y.Z` only after the commit is on `main`. Workflow: `.github/workflows/release.yml`. Bump `version` in `Cargo.toml` and add a [CHANGELOG.md](CHANGELOG.md) entry when shipping a release.

After code changes, run the relevant `fmt` / `clippy` / `test` commands and fix failures before finishing.

## Tech stack guidelines

- **Language:** Rust 2021. Prefer `anyhow::{Result, bail, Context}` at binaries and engine boundaries. Keep `?` paths; do not add `unwrap()` on fallible I/O or network unless the existing pattern is a poisoned lock recovery (`into_inner()`).
- **Async:** Tokio multi-thread runtime. HTTP and torrent work is async; GUI (`eframe`) is sync and drives jobs through `JobSnapshot` / spawned tasks. Do not block the GUI thread on downloads.
- **TLS / HTTP:** `reqwest` with `rustls-tls` only (no native-tls). Timeouts stay 20s connect / 60s between reads unless there is a documented reason to change them. HTTP resume and merge rules in README **How it works** are invariants: no zero-padding short 206 bodies; merge via `.{filename}.merge.tmp` then rename; single-stream GET does not resume.
- **Torrent:** librqbit with `default-features = false` and `rust-tls` plus `disable-upload` so leech sessions can set `disable_upload`. DHT must stay in-memory (`DhtSessionConfig`). Share mode listens on an ephemeral TCP port and calls `session.stop()` only when the job is Stopped; leech mode never uploads and stops at 100%. Isolate output per info-hash; keep the in-process folder lock so the same hash cannot run twice.
- **GUI:** `eframe` / egui, gated with `#[cfg(feature = "gui")]`. Keep phosphor / amber / ice themes. Cap concurrent jobs at 4; refuse a second active job with the same magnet info-hash or HTTP URL. Do not introduce a webview or HTML/CSS front.
- **CLI:** `clap` derive. Interactive menu remains the no-args path. Startup auto-update stays TTY-only; `torrenter update` always runs when requested.
- **Serialization:** `serde` / `serde_json`. Resume manifests use `camelCase` (`ResumeManifest` in `src/http.rs`). Updater config is `~` platform config dir `torrenter/config.json` (see `update::config_path`).
- **Logging:** `tracing-subscriber` with `EnvFilter`, default `error`, writer `stderr`. Do not log secrets or full magnet/query strings at info in new code.
- **Tests:** unit tests live in the same file under `#[cfg(test)]`. HTTP tests use `tempfile` and in-process servers where possible. Prefer `#[tokio::test]` for async. Do not hit real GitHub or public CDNs from tests.

## Code style

- rustfmt defaults (no `rustfmt.toml`). Match surrounding naming: `snake_case` functions, `SCREAMING_SNAKE` for module constants (`DOWNLOAD_DIRECTORY`, `PART_COUNT`).
- Keep public surfaces small. New engine entry points should follow existing pairs: `start_*` (CLI) and `start_*_reported` (GUI + `JobSnapshot`).
- Feature-gate GUI-only modules the same way `src/lib.rs` does (`gui`, `icon`). The GUI binary stays a thin `main` that calls `torrenter::gui::run()`.
- User-visible strings already used in tests and the README (`Invalid magnet URL!`, `Invalid HTTP(S) URL!`, banner format) should stay stable unless the change is intentional and tests/docs update together.
- Version and asset names are derived from `CARGO_PKG_VERSION` and `identity::current_asset_name`. If you add a release target, update identity, README asset table, and `.github/workflows/release.yml` together.

## Security considerations

This tool downloads untrusted content the user asked for. Do not weaken path, URL, or updater checks.

- Magnet: only `magnet:?xt=urn:` with 32-char base32 or 40-char hex info hash (`validate.rs`). No `.torrent` file parse.
- HTTP filenames come from `Content-Disposition` or a **single** URL path segment; `.` / `..` become `download.bin`. Never write outside the configured download / info-hash / `.resume` trees.
- Updater (`src/update.rs`) must keep: pinned first hop `https://github.com/danted4/torrenter/releases/download/<tag>/<name>` with tag matching the release; SHA-256 against `SHA256SUMS`; size caps; timeouts; CLI vs GUI asset prefix. `SHA256SUMS` is same-publisher integrity, not a signature — do not claim otherwise.
- Auto-update defaults **off**. Do not enable it by default in config or CI.
- Do not add credentials, tokens, or tracking. Do not expand the updater to arbitrary hosts.

## PR and commit notes

- Keep changes scoped. Do not add Node, web, or extra crates unless required for the task.
- Doc-only edits on `main` do not need a version tag (README **Development**).
- When behavior changes, update [README.md](README.md) and [CHANGELOG.md](CHANGELOG.md) in the same change. Keep AGENTS.md to agent workflow and invariants; do not duplicate README tables or the mermaid chart.
- Match existing commit tone: short, imperative, why-focused (e.g. fix resume, isolate magnet folders).

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

@AGENTS.md

`AGENTS.md` (imported above) is the authoritative source for build/test commands, invariants, security rules, and commit conventions. `README.md` holds product behavior. The notes below add cross-file architecture that is not spelled out there. Keep them in sync with AGENTS.md rather than duplicating it.

## Quick commands

```bash
cargo test --locked --all-features <name_substring>       # one test (unit or integration)
cargo test --locked --all-features --test mcp_stdio       # MCP stdio integration test only
cargo run -- magnet '<magnet>'                            # CLI one-shot
cargo run --features gui --bin torrenter-gui              # GUI
```

Always pass `--all-features` to clippy and tests so the GUI and FFI modules compile, the same way CI does.

## How the pieces connect

- **Engine split:** `torrent.rs` and `http.rs` each expose a `start_*` (CLI, `indicatif` bars) and a `start_*_reported` variant that writes into an `Arc<JobSnapshot>` (`progress.rs`). Every non-CLI front only reads snapshots. It never touches engine internals.
- **Control flow:** pause and stop are atomic flags on `JobSnapshot`. Engines poll them through `check_stop` / `abort_if_paused` / `wait_while_paused` / `until_stop` / `honor_control`. Pause and stop surface as the typed errors `DownloadPaused` / `DownloadStopped`, which callers detect with `is_pause_abort` / `is_stopped`. They are not real failures.
- **`Session` (`session.rs`)** is the shared queue for the GUI, the UniFFI `Engine` (`ffi.rs`), and `torrenter mcp` (`mcp.rs`). It is synchronous on its public side, with state in an `Arc<Mutex<Inner>>`. A background `engine_loop` thread owns its own Tokio runtime and receives `EngineCmd` over an `mpsc` channel. Download slots (`slots.rs`, `Slots` shared by `Session` and `engine_loop`; cap = the `max_downloads` setting, default 4, 1–8, live) gate actual starts in list order (`Job.rank`), so extra jobs sit in `queued`. Each running job keeps a `SlotTicket` on its snapshot: pausing (`wait_while_paused`, the torrent tick) and seeding release it, resuming re-takes one. A disk-full failure holds the queue before the slot is freed (`hold_queue_and_detach`). Failures and restore probe results come back as `EngineEvent`s, and callers must poll `drain_events()` periodically, along with `persist(force)` (throttled by `PERSIST_EVERY`).
- **Persistence (`jobs.rs`):** `jobs.json` sits beside the updater `config.json` (`update::config_path`). `Session::hydrate()` loads `PersistedJob`s, rebuilds snapshots, and sends restore probes. Payloads missing on disk come back as `RestoreAction::Missing` (`MISSING_FILES`). An HTTP job without a resume manifest is reported as `HTTP_NOT_RESUMABLE`. `open_leech` forces `seed = false` and rewrites the file on hydrate, so restored GUI share jobs never upload from MCP.
- **FFI boundary:** `ffi.rs` is the only UniFFI surface (`Engine`, `JobView`, `Kind`, `EngineError`). Generated bindings land in `android/` and `ios/generated` through the `android-ndk` / `ios-xcframework` helper binaries. Do not hand-edit them. Regenerate them instead.
- **Tests:** unit tests are inline under `#[cfg(test)]`. `tests/mcp_stdio.rs` spawns the real `torrenter mcp` binary and isolates config by pointing `HOME` / `XDG_CONFIG_HOME` / `APPDATA` / `USERPROFILE` at a tempdir. Use the same pattern for any new test that touches `jobs.json` or `config.json`.

## Repo-local agent assets

`.cursor/skills/` contains `code-review`, `grill-me`, and `redact-gui-screenshots`. The last one covers how to anonymize GUI screenshots for `docs/`: work on copies only and never overwrite originals. `.cursor/plans/` contains past design plans, such as `mcp-stdio.md`, that are useful background for that feature.

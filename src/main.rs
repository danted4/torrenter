use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncBufReadExt, BufReader};
use torrenter::credentials::{self, KeySource};
use torrenter::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
use torrenter::debrid::{check_account, Secret, Service};
use torrenter::http::{start_url_download_in, DownloadLayout};
use torrenter::identity::{banner, AppKind, VERSION_TAG};
use torrenter::paths;
use torrenter::progress::{watch_cli_controls, JobSnapshot};
use torrenter::torrent::start_download;
use torrenter::update::{self, effective_connections, load_config};
use torrenter::validate::{is_valid_download_url, is_valid_magnet_url};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod mcp;

#[derive(Parser)]
#[command(
    name = "torrenter",
    version = VERSION_TAG,
    about = concat!(
        "danted4/torrenter v",
        env!("CARGO_PKG_VERSION"),
        " — magnet/torrent and multipart HTTP(S) downloader"
    )
)]
struct Cli {
    /// Persist auto-update from GitHub Releases (on)
    #[arg(long, conflicts_with = "no_auto_update")]
    auto_update: bool,
    /// Persist auto-update from GitHub Releases (off)
    #[arg(long)]
    no_auto_update: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download a direct HTTP(S) URL with multi-part resume (connections setting, Auto = 4)
    Http {
        url: String,
        /// One-shot download folder (does not persist)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Download a magnet / torrent link
    Magnet {
        uri: String,
        /// One-shot download folder (does not persist)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Fetch through a debrid service instead of peers (e.g. `torbox`)
        #[arg(long, value_name = "SERVICE")]
        debrid: Option<String>,
    },
    /// Check GitHub and replace this binary if a newer release exists
    Update,
    /// Speak MCP over stdio (stdout is the protocol; logs on stderr)
    Mcp,
    /// Manage debrid service API keys (stored owner-only in credentials.json)
    Debrid {
        #[command(subcommand)]
        action: DebridAction,
    },
}

#[derive(Subcommand, Debug)]
enum DebridAction {
    /// Save an API key (typed hidden, or piped on stdin)
    Add { service: String },
    /// Check the saved key with the service
    Test { service: String },
    /// Delete the saved key
    Remove { service: String },
    /// Show which services have a key
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if cli.auto_update {
        update::set_auto_update(true)?;
        eprintln!("auto-update: on");
    } else if cli.no_auto_update {
        update::set_auto_update(false)?;
        eprintln!("auto-update: off");
    }

    if matches!(cli.command, Some(Command::Mcp)) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            return mcp::run().await;
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        anyhow::bail!("mcp is not available on this target");
    }

    if matches!(cli.command, Some(Command::Update)) {
        println!("{}", banner());
        return run_update().await;
    }

    if let Some(Command::Debrid { action }) = &cli.command {
        return run_debrid_command(action).await;
    }

    maybe_auto_update().await?;
    println!("{}", banner());
    warn_ensure_dest();

    match cli.command {
        Some(Command::Http { url, dir }) => run_http(url.trim(), dir).await,
        Some(Command::Magnet { uri, dir, debrid }) => {
            let via = debrid.as_deref().map(service_arg).transpose()?;
            run_magnet(uri.trim(), dir, via).await
        }
        Some(Command::Update | Command::Mcp | Command::Debrid { .. }) => unreachable!(),
        None => prompt_user().await,
    }
}

fn warn_ensure_dest() {
    let dir = paths::effective_dir(&load_config());
    if let Err(error) = paths::ensure_dir(&dir) {
        eprintln!("download folder: {error:#}");
    }
}

fn dest_for_run(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    let dest = match override_dir {
        Some(path) => paths::resolve_user_dir(path)?,
        None => paths::effective_dir(&load_config()),
    };
    paths::ensure_dir(&dest)?;
    Ok(dest)
}

async fn run_http(url: &str, dir: Option<PathBuf>) -> Result<()> {
    let dest = dest_for_run(dir)?;
    println!("download: {}", dest.display());
    let parts = effective_connections(&load_config());
    start_url_download_in(url, DownloadLayout::in_dir(dest).with_part_count(parts)).await
}

async fn run_magnet(uri: &str, dir: Option<PathBuf>, via: Option<Service>) -> Result<()> {
    let dest = dest_for_run(dir)?;
    println!("download: {}", dest.display());
    match via {
        Some(service) => run_debrid_download(uri, dest, service).await,
        None => start_download(uri, load_config().seed, dest).await,
    }
}

/// Services this build offers (verified with a real account).
fn service_arg(value: &str) -> Result<Service> {
    let offered = Service::shipped();
    match Service::parse(value) {
        Some(service) if offered.contains(&service) => Ok(service),
        _ => bail!(
            "unknown debrid service {value:?}; available: {}",
            offered
                .iter()
                .map(|service| service.id())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn key_or_hint(service: Service) -> Result<Secret> {
    credentials::key_for(service)
        .map(|(key, _)| key)
        .with_context(|| {
            format!(
                "no {} API key; run: torrenter debrid add {}",
                service.label(),
                service.id()
            )
        })
}

async fn run_debrid_download(uri: &str, dest: PathBuf, service: Service) -> Result<()> {
    if !is_valid_magnet_url(uri) {
        bail!("Invalid magnet URL!");
    }
    let key = key_or_hint(service)?;
    let config = load_config();
    let snapshot = Arc::new(JobSnapshot::default());
    let state = SharedDebridState::default();
    let run = DebridRun {
        service,
        key,
        magnet: uri.to_string(),
        dest,
        part_count: effective_connections(&config),
        remove_after: config.debrid_remove_after,
        state: Arc::clone(&state),
        base: None,
    };
    println!("\nvia {}\n[1] pause/resume   [2] stop\n", service.label());
    let task = tokio::spawn(start_debrid_reported(run, Arc::clone(&snapshot)));
    let controls = tokio::spawn(watch_cli_controls(Arc::clone(&snapshot)));
    let bar = ProgressBar::new(1);
    bar.set_style(
        ProgressStyle::with_template(
            "[{bar:20}] {percent}% | {msg} | {bytes}/{total_bytes} @ {bytes_per_sec} | ETA: {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█░"),
    );
    while !task.is_finished() {
        let view = state.lock().map(|state| state.clone()).unwrap_or_default();
        if snapshot.status() == "fetching" {
            bar.set_length(1000);
            bar.set_position(u64::from(view.remote_permille));
            bar.set_message(format!("{} fetching", service.label()));
        } else {
            bar.set_length(snapshot.total.load(Ordering::Relaxed).max(1));
            bar.set_position(snapshot.downloaded.load(Ordering::Relaxed));
            let count = view.files.len();
            bar.set_message(if count > 0 {
                format!(
                    "{} file {}/{count}",
                    snapshot.status(),
                    view.file_index.max(1)
                )
            } else {
                snapshot.status()
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    controls.abort();
    let result = task.await.context("debrid task panicked")?;
    bar.finish_and_clear();
    result?;
    if snapshot.is_done() {
        println!("\nDebrid download finished: {}", snapshot.title());
    } else if snapshot.is_stopped() {
        println!("\nDebrid download stopped.");
    }
    Ok(())
}

/// A key typed without echo on a terminal, or one line from piped stdin.
fn read_secret(prompt: &str) -> Result<Secret> {
    let raw = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("failed to read the key")?
    } else {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("failed to read the key from stdin")?;
        line
    };
    Secret::new(raw).context("empty key")
}

async fn run_debrid_command(action: &DebridAction) -> Result<()> {
    match action {
        DebridAction::List => {
            let configured = credentials::configured();
            for service in Service::shipped() {
                let source = configured
                    .iter()
                    .find(|(found, _)| found == service)
                    .map(|(_, source)| match source {
                        KeySource::Env => "key from environment",
                        KeySource::File => "key saved",
                    })
                    .unwrap_or("no key");
                println!("{:<12} {source}", service.id());
            }
            Ok(())
        }
        DebridAction::Add { service } => {
            let service = service_arg(service)?;
            let key = read_secret(&format!("{} API key: ", service.label()))?;
            credentials::set_key(service, &key)?;
            println!("saved {} key", service.label());
            report_account(service, &key).await;
            if credentials::key_for(service).is_some_and(|(_, source)| source == KeySource::Env) {
                println!("note: {} overrides the saved key", service.env_var());
            }
            Ok(())
        }
        DebridAction::Test { service } => {
            let service = service_arg(service)?;
            let key = key_or_hint(service)?;
            report_account(service, &key).await;
            Ok(())
        }
        DebridAction::Remove { service } => {
            let service = service_arg(service)?;
            credentials::remove_key(service)?;
            println!("removed {} key", service.label());
            Ok(())
        }
    }
}

async fn report_account(service: Service, key: &Secret) {
    match check_account(service, key).await {
        Ok(account) if account.premium => println!(
            "{}: premium{}",
            service.label(),
            account
                .expires
                .map(|until| format!(" until {until}"))
                .unwrap_or_default()
        ),
        Ok(_) => println!(
            "{}: key works but the account is not premium",
            service.label()
        ),
        Err(error) => eprintln!("{}: {error}", service.label()),
    }
}

fn route_label(config: &update::AppConfig) -> String {
    match config.debrid_route.as_deref().and_then(Service::parse) {
        Some(service) => format!("via {}", service.label()),
        None => "off".into(),
    }
}

/// Next route in the cycle: off → each service with a key → off.
fn next_route(current: Option<Service>) -> Option<Service> {
    let ready: Vec<Service> = Service::shipped()
        .iter()
        .copied()
        .filter(|service| credentials::key_for(*service).is_some())
        .collect();
    match current.and_then(|now| ready.iter().position(|service| *service == now)) {
        None => ready.first().copied(),
        Some(index) => ready.get(index + 1).copied(),
    }
}

async fn maybe_auto_update() -> Result<()> {
    update::cleanup_replaced_binary();
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let Ok(found) = update::check_latest(AppKind::Cli).await else {
        return Ok(());
    };
    let Some(available) = found else {
        return Ok(());
    };
    if load_config().auto_update {
        println!(
            "Updating {VERSION_TAG} -> {} ({})",
            available.tag, available.asset_name
        );
        update::install_update(&available).await?;
        update::relaunch()?;
        return Ok(());
    }
    eprintln!(
        "a new version is available: {}  —  torrenter update",
        available.tag
    );
    Ok(())
}

async fn run_update() -> Result<()> {
    match update::check_latest(AppKind::Cli).await? {
        None => {
            println!("Already on {VERSION_TAG}");
            Ok(())
        }
        Some(available) => {
            println!("Installing {}", available.tag);
            update::install_update(&available).await?;
            println!("Updated to {}. Relaunch torrenter.", available.tag);
            Ok(())
        }
    }
}

async fn prompt_user() -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let config = load_config();
        let auto = if config.auto_update { "on" } else { "off" };
        let seed = if config.seed { "on" } else { "off" };
        let download = paths::effective_dir(&config);
        let connections = config
            .connections
            .map(|parts| parts.to_string())
            .unwrap_or_else(|| "auto (4)".into());
        print!(
            "\nauto-update: {auto}\nseed: {seed}\ndebrid: {}\nconnections: {connections}\ndownload: {}\nChoose download type:\n1. Magnet URL\n2. Direct URL (multi-part)\n3. Toggle auto-update\n4. Update now\n5. Toggle seed\n6. Change download folder\n7. Debrid: next route\n8. Debrid accounts\n9. Connections per download\n\n",
            route_label(&config),
            download.display()
        );
        std::io::stdout().flush()?;
        let mut choice = String::new();
        stdin.read_line(&mut choice).await?;

        match choice.trim() {
            "3" => {
                let enabled = !load_config().auto_update;
                update::set_auto_update(enabled)?;
                println!("auto-update: {}", if enabled { "on" } else { "off" });
                if enabled {
                    maybe_auto_update().await?;
                }
                continue;
            }
            "5" => {
                let enabled = !load_config().seed;
                update::set_seed(enabled)?;
                println!("seed: {}", if enabled { "on" } else { "off" });
                continue;
            }
            "6" => {
                print!("\nDownload folder path:\n\n");
                std::io::stdout().flush()?;
                let mut path = String::new();
                stdin.read_line(&mut path).await?;
                let path = path.trim();
                if path.is_empty() {
                    continue;
                }
                match update::set_download_dir(path) {
                    Ok(config) => {
                        println!("download: {}", paths::effective_dir(&config).display());
                    }
                    Err(error) => eprintln!("{error:#}"),
                }
                continue;
            }
            "7" => {
                let current = config.debrid_route.as_deref().and_then(Service::parse);
                let next = next_route(current);
                if next.is_none() && current.is_none() {
                    println!("add a key first (menu 8)");
                }
                let config = update::set_debrid_route(next.map(Service::id))?;
                println!("debrid: {}", route_label(&config));
                continue;
            }
            "8" => {
                let shipped = Service::shipped();
                print!(
                    "\nService ({}):\n\n",
                    shipped
                        .iter()
                        .map(|s| s.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::io::stdout().flush()?;
                let mut name = String::new();
                stdin.read_line(&mut name).await?;
                let service = match service_arg(name.trim()) {
                    Ok(service) => service,
                    Err(error) => {
                        eprintln!("{error:#}");
                        continue;
                    }
                };
                print!("\na. add key   t. test key   r. remove key\n\n");
                std::io::stdout().flush()?;
                let mut pick = String::new();
                stdin.read_line(&mut pick).await?;
                let action = match pick.trim() {
                    "a" => DebridAction::Add {
                        service: service.id().into(),
                    },
                    "t" => DebridAction::Test {
                        service: service.id().into(),
                    },
                    "r" => DebridAction::Remove {
                        service: service.id().into(),
                    },
                    _ => continue,
                };
                if let Err(error) = run_debrid_command(&action).await {
                    eprintln!("{error:#}");
                }
                continue;
            }
            "9" => {
                print!("\nConnections per download (auto or 1-16):\n\n");
                std::io::stdout().flush()?;
                let mut value = String::new();
                stdin.read_line(&mut value).await?;
                let value = value.trim();
                let parts = if value.eq_ignore_ascii_case("auto") || value.is_empty() {
                    None
                } else {
                    match value.parse::<u8>() {
                        Ok(parts) => Some(parts),
                        Err(_) => {
                            eprintln!("enter auto or a number from 1 to 16");
                            continue;
                        }
                    }
                };
                let config = update::set_connections(parts)?;
                println!(
                    "connections: {}",
                    config
                        .connections
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "auto (4)".into())
                );
                continue;
            }
            "4" => return run_update().await,
            "2" => {
                print!("\nPlease paste the direct download URL:\n\n");
                std::io::stdout().flush()?;
                let mut url = String::new();
                stdin.read_line(&mut url).await?;
                let url = url.trim().to_string();
                if !is_valid_download_url(&url) {
                    eprintln!("\nInvalid HTTP(S) URL! Try again ...");
                    continue;
                }
                return run_http(&url, None).await;
            }
            _ => {
                print!("\nPlease paste your magnetURI:\n\n");
                std::io::stdout().flush()?;
                let mut magnet = String::new();
                stdin.read_line(&mut magnet).await?;
                let magnet = magnet.trim().to_string();
                if !is_valid_magnet_url(&magnet) {
                    eprintln!("\nInvalid magnet URL! Try again ...");
                    continue;
                }
                let via = load_config()
                    .debrid_route
                    .as_deref()
                    .and_then(Service::parse)
                    .filter(|service| Service::shipped().contains(service));
                return run_magnet(&magnet, None, via).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_subcommand() {
        let cli = Cli::try_parse_from(["torrenter", "http", "https://example.com/a.bin"]).unwrap();
        match cli.command {
            Some(Command::Http { url, dir }) => {
                assert_eq!(url, "https://example.com/a.bin");
                assert!(dir.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_http_dir_flag() {
        let cli = Cli::try_parse_from([
            "torrenter",
            "http",
            "https://example.com/a.bin",
            "--dir",
            "/tmp/out",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Http { url, dir }) => {
                assert_eq!(url, "https://example.com/a.bin");
                assert_eq!(dir, Some(PathBuf::from("/tmp/out")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_magnet_subcommand() {
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10";
        let cli = Cli::try_parse_from(["torrenter", "magnet", magnet]).unwrap();
        match cli.command {
            Some(Command::Magnet { uri, dir, debrid }) => {
                assert_eq!(uri, magnet);
                assert!(dir.is_none());
                assert!(debrid.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_debrid_flag_and_subcommands() {
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10";
        let cli =
            Cli::try_parse_from(["torrenter", "magnet", magnet, "--debrid", "torbox"]).unwrap();
        match cli.command {
            Some(Command::Magnet { debrid, .. }) => assert_eq!(debrid.as_deref(), Some("torbox")),
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["torrenter", "debrid", "add", "torbox"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Debrid { action: DebridAction::Add { ref service } }) if service == "torbox"
        ));
        assert!(
            Cli::try_parse_from(["torrenter", "debrid", "add", "torbox", "SECRET"]).is_err(),
            "keys are never taken from argv"
        );
        assert!(service_arg("TorBox").is_ok());
        assert!(service_arg("nope").is_err());
    }

    #[test]
    fn parses_magnet_dir_flag() {
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10";
        let cli =
            Cli::try_parse_from(["torrenter", "magnet", magnet, "--dir", "/tmp/out"]).unwrap();
        match cli.command {
            Some(Command::Magnet { uri, dir, .. }) => {
                assert_eq!(uri, magnet);
                assert_eq!(dir, Some(PathBuf::from("/tmp/out")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_update_and_auto_update_flag() {
        let cli = Cli::try_parse_from(["torrenter", "--auto-update", "update"]).unwrap();
        assert!(cli.auto_update);
        assert!(matches!(cli.command, Some(Command::Update)));
    }

    #[test]
    fn default_is_interactive() {
        let cli = Cli::try_parse_from(["torrenter"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_mcp_subcommand() {
        let cli = Cli::try_parse_from(["torrenter", "mcp"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Mcp)));
    }
}

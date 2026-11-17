use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncBufReadExt, BufReader};
use torrenter::credentials::{self, AccountInfo, KeySource};
use torrenter::debrid::run::{start_debrid_reported, DebridRun, SharedDebridState};
use torrenter::debrid::{check_account, Secret, Service};
use torrenter::http::{start_url_download_in, DownloadLayout};
use torrenter::identity::{banner, AppKind, VERSION_TAG};
use torrenter::paths;
use torrenter::progress::{watch_cli_controls, JobSnapshot};
use torrenter::torrent::start_download;
use torrenter::torrent_file;
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
    /// Download a magnet link or a .torrent file
    Magnet {
        /// A magnet link, or the path of a .torrent file
        uri: String,
        /// One-shot download folder (does not persist)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Fetch through a debrid account instead of peers: its name, or the
        /// provider (e.g. `torbox`) when it has one account
        #[arg(long, value_name = "ACCOUNT")]
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
    /// Add an account: a provider's API key (typed hidden, or piped on stdin)
    Add {
        /// torbox, or (experimental) realdebrid, alldebrid, premiumize, debridlink
        service: String,
        /// Name shown in menus (default: the provider's name, then "TorBox 2", …)
        #[arg(long)]
        name: Option<String>,
    },
    /// Check an account's key with its provider
    Test { account: String },
    /// Delete a saved account
    Remove { account: String },
    /// List accounts (names and providers; never keys)
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
            let via = debrid.as_deref().map(account_arg).transpose()?;
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

async fn run_magnet(uri: &str, dir: Option<PathBuf>, via: Option<AccountInfo>) -> Result<()> {
    // A .torrent file runs as the magnet built from it, starting from its
    // bytes (no metadata wait); a debrid route uses that magnet.
    let cleaned = clean_path_arg(uri);
    let (magnet, torrent) = if !is_valid_magnet_url(uri) && torrent_file::looks_like_file(&cleaned)
    {
        let path = PathBuf::from(cleaned);
        let bytes = torrent_file::read_capped(&path)?;
        let imported = torrent_file::parse(&bytes)?;
        if imported.private && via.is_some() {
            bail!("a private torrent downloads only from its own tracker's peers, not through a debrid service");
        }
        println!("torrent: {}", imported.name);
        (imported.magnet, Some(bytes))
    } else {
        (uri.to_string(), None)
    };
    let dest = dest_for_run(dir)?;
    println!("download: {}", dest.display());
    match via {
        Some(account) => run_debrid_download(&magnet, dest, &account).await,
        None => start_download(&magnet, torrent, load_config().seed, dest).await,
    }
}

/// A pasted or dragged-in path: surrounding quotes are removed, and on macOS
/// and Linux the `\ ` terminals add for spaces (on Windows `\` separates
/// folders, so it stays).
fn clean_path_arg(arg: &str) -> String {
    let trimmed = arg.trim().trim_matches(|c| c == '"' || c == '\'');
    if cfg!(windows) {
        trimmed.to_string()
    } else {
        trimmed.replace("\\ ", " ")
    }
}

/// Services this build offers (experimental ones included).
fn service_arg(value: &str) -> Result<Service> {
    let offered = Service::offered();
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

/// Accounts for providers this build offers.
fn offered_accounts() -> Vec<AccountInfo> {
    credentials::accounts()
        .into_iter()
        .filter(|account| Service::offered().contains(&account.service))
        .collect()
}

/// An account by name, id, or provider (when that provider has one account).
fn account_arg(value: &str) -> Result<AccountInfo> {
    let accounts = offered_accounts();
    if let Some(found) = credentials::resolve(value)
        .filter(|found| accounts.iter().any(|account| account.id == found.id))
    {
        return Ok(found);
    }
    if accounts.is_empty() {
        bail!("no debrid accounts; run: torrenter debrid add torbox");
    }
    bail!(
        "no single debrid account matches {value:?}; accounts: {}",
        accounts
            .iter()
            .map(|account| format!("{} ({})", account.name, account.service.id()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn account_key(account: &AccountInfo) -> Result<Secret> {
    credentials::account_key(&account.id)
        .map(|(_, key)| key)
        .with_context(|| format!("no key for {}", account.name))
}

async fn run_debrid_download(uri: &str, dest: PathBuf, account: &AccountInfo) -> Result<()> {
    if !is_valid_magnet_url(uri) {
        bail!("Invalid magnet URL!");
    }
    let service = account.service;
    let key = account_key(account)?;
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
        stall_timeout: torrenter::debrid::run::STALL_TIMEOUT,
    };
    println!("\nvia {}\n[1] pause/resume   [2] stop\n", account.name);
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
            let accounts = offered_accounts();
            if accounts.is_empty() {
                println!("no debrid accounts; run: torrenter debrid add torbox");
            }
            for account in accounts {
                let source = match account.source {
                    KeySource::Env => format!("from {}", account.service.env_var()),
                    KeySource::File => "saved".to_string(),
                };
                let experimental = if account.service.verified() {
                    ""
                } else {
                    " (experimental)"
                };
                println!(
                    "{:<20} {:<12} {source}{experimental}",
                    account.name,
                    account.service.id()
                );
            }
            Ok(())
        }
        DebridAction::Add { service, name } => {
            let service = service_arg(service)?;
            let key = read_secret(&format!("{} API key: ", service.label()))?;
            let account = credentials::add_account(service, name.as_deref().unwrap_or(""), &key)?;
            println!("saved {} ({})", account.name, service.menu_label());
            if !service.verified() {
                println!(
                    "{} support is experimental: not yet tested with a real account. Please report problems at github.com/danted4/torrenter/issues",
                    service.label()
                );
            }
            report_account(&account.name, service, &key).await;
            Ok(())
        }
        DebridAction::Test { account } => {
            let account = account_arg(account)?;
            let key = account_key(&account)?;
            report_account(&account.name, account.service, &key).await;
            Ok(())
        }
        DebridAction::Remove { account } => {
            let account = account_arg(account)?;
            credentials::remove_account(&account.id)?;
            if load_config().debrid_route.as_deref() == Some(account.id.as_str()) {
                update::set_debrid_route(None)?;
            }
            println!("removed {}", account.name);
            Ok(())
        }
    }
}

async fn report_account(name: &str, service: Service, key: &Secret) {
    match check_account(service, key).await {
        Ok(account) if account.premium => println!(
            "{name}: premium{}",
            account
                .expires
                .map(|until| format!(" until {until}"))
                .unwrap_or_default()
        ),
        Ok(_) => println!("{name}: key works but the account is not premium"),
        Err(error) => eprintln!("{name}: {error}"),
    }
}

/// The remembered route's account, if it still exists.
fn route_account(config: &update::AppConfig) -> Option<AccountInfo> {
    let id = config.debrid_route.as_deref()?;
    offered_accounts()
        .into_iter()
        .find(|account| account.id == id)
}

fn route_label(config: &update::AppConfig) -> String {
    match route_account(config) {
        Some(account) => format!("via {}", account.name),
        None => "default (none)".into(),
    }
}

/// Next route in the cycle: default → each account → default.
fn next_route(current: Option<&AccountInfo>) -> Option<AccountInfo> {
    let accounts = offered_accounts();
    match current.and_then(|now| accounts.iter().position(|account| account.id == now.id)) {
        None => accounts.first().cloned(),
        Some(index) => accounts.get(index + 1).cloned(),
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
                let current = route_account(&config);
                let next = next_route(current.as_ref());
                if next.is_none() && current.is_none() {
                    println!("add an account first (menu 8)");
                }
                let config = update::set_debrid_route(next.as_ref().map(|a| a.id.as_str()))?;
                println!("debrid: {}", route_label(&config));
                continue;
            }
            "8" => {
                let _ = run_debrid_command(&DebridAction::List).await;
                print!("\na. add account   t. test account   r. remove account\n\n");
                std::io::stdout().flush()?;
                let mut pick = String::new();
                stdin.read_line(&mut pick).await?;
                let pick = pick.trim().to_string();
                let ask = |prompt: String| {
                    print!("\n{prompt}\n\n");
                    let _ = std::io::stdout().flush();
                };
                let action = match pick.as_str() {
                    "a" => {
                        ask(format!(
                            "Provider ({}):",
                            Service::offered()
                                .iter()
                                .map(|s| if s.verified() {
                                    s.id().to_string()
                                } else {
                                    format!("{} (experimental)", s.id())
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        let mut service = String::new();
                        stdin.read_line(&mut service).await?;
                        ask("Name (Enter for the provider's name):".into());
                        let mut name = String::new();
                        stdin.read_line(&mut name).await?;
                        let name = name.trim();
                        DebridAction::Add {
                            service: service.trim().into(),
                            name: (!name.is_empty()).then(|| name.to_string()),
                        }
                    }
                    "t" | "r" => {
                        ask("Account name:".into());
                        let mut account = String::new();
                        stdin.read_line(&mut account).await?;
                        let account = account.trim().to_string();
                        if pick == "t" {
                            DebridAction::Test { account }
                        } else {
                            DebridAction::Remove { account }
                        }
                    }
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
                print!("\nPlease paste your magnetURI (or the path of a .torrent file):\n\n");
                std::io::stdout().flush()?;
                let mut magnet = String::new();
                stdin.read_line(&mut magnet).await?;
                let magnet = magnet.trim().to_string();
                if !is_valid_magnet_url(&magnet)
                    && !torrent_file::looks_like_file(&clean_path_arg(&magnet))
                {
                    eprintln!("\nInvalid magnet URL! Try again ...");
                    continue;
                }
                let via = route_account(&load_config());
                return run_magnet(&magnet, None, via).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_and_dragged_paths_are_cleaned() {
        assert_eq!(clean_path_arg("  '/tmp/a b.torrent' "), "/tmp/a b.torrent");
        assert_eq!(clean_path_arg("\"/tmp/x.torrent\""), "/tmp/x.torrent");
        #[cfg(not(windows))]
        assert_eq!(
            clean_path_arg("/tmp/My\\ File.torrent"),
            "/tmp/My File.torrent"
        );
        #[cfg(windows)]
        assert_eq!(clean_path_arg(r"C:\x\ y.torrent"), r"C:\x\ y.torrent");
    }

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
            Some(Command::Debrid { action: DebridAction::Add { ref service, name: None } }) if service == "torbox"
        ));
        let named = Cli::try_parse_from(["torrenter", "debrid", "add", "torbox", "--name", "work"])
            .unwrap();
        assert!(matches!(
            named.command,
            Some(Command::Debrid { action: DebridAction::Add { name: Some(ref name), .. } }) if name == "work"
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

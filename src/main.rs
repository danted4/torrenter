use std::io::{IsTerminal, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use torrenter::http::start_url_download;
use torrenter::identity::{banner, AppKind, VERSION_TAG};
use torrenter::torrent::start_download;
use torrenter::update::{self, load_config};
use torrenter::validate::{is_valid_download_url, is_valid_magnet_url};

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
    /// Download a direct HTTP(S) URL with 4-part resume
    Http { url: String },
    /// Download a magnet / torrent link
    Magnet { uri: String },
    /// Check GitHub and replace this binary if a newer release exists
    Update,
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

    if matches!(cli.command, Some(Command::Update)) {
        println!("{}", banner());
        return run_update().await;
    }

    maybe_auto_update().await?;
    println!("{}", banner());

    match cli.command {
        Some(Command::Http { url }) => start_url_download(url.trim()).await,
        Some(Command::Magnet { uri }) => start_download(uri.trim(), load_config().seed).await,
        Some(Command::Update) => unreachable!(),
        None => prompt_user().await,
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
        print!(
            "\nauto-update: {auto}\nseed: {seed}\nChoose download type:\n1. Magnet URL\n2. Direct URL (multi-part)\n3. Toggle auto-update\n4. Update now\n5. Toggle seed\n\n"
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
                return start_url_download(&url).await;
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
                return start_download(&magnet, load_config().seed).await;
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
            Some(Command::Http { url }) => assert_eq!(url, "https://example.com/a.bin"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_magnet_subcommand() {
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10";
        let cli = Cli::try_parse_from(["torrenter", "magnet", magnet]).unwrap();
        match cli.command {
            Some(Command::Magnet { uri }) => assert_eq!(uri, magnet),
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
}

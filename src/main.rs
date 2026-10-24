mod http;
mod progress;
mod torrent;
mod validate;

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::http::start_url_download;
use crate::torrent::start_download;
use crate::validate::{is_valid_download_url, is_valid_magnet_url};

#[derive(Parser)]
#[command(
    name = "torrenter",
    about = "Magnet/torrent and multipart HTTP(S) downloader",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download a direct HTTP(S) URL with 4-part resume
    Http { url: String },
    /// Download a magnet / torrent link
    Magnet { uri: String },
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

    match Cli::parse().command {
        Some(Command::Http { url }) => start_url_download(url.trim()).await,
        Some(Command::Magnet { uri }) => start_download(uri.trim()).await,
        None => prompt_user().await,
    }
}

async fn prompt_user() -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        print!("\nChoose download type:\n1. Magnet URL\n2. Direct URL (multi-part)\n\n");
        std::io::stdout().flush()?;
        let mut choice = String::new();
        stdin.read_line(&mut choice).await?;

        if choice.trim() == "2" {
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

        print!("\nPlease paste your magnetURI:\n\n");
        std::io::stdout().flush()?;
        let mut magnet = String::new();
        stdin.read_line(&mut magnet).await?;
        let magnet = magnet.trim().to_string();
        if !is_valid_magnet_url(&magnet) {
            eprintln!("\nInvalid magnet URL! Try again ...");
            continue;
        }
        return start_download(&magnet).await;
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
    fn default_is_interactive() {
        let cli = Cli::try_parse_from(["torrenter"]).unwrap();
        assert!(cli.command.is_none());
    }
}

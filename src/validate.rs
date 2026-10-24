use std::sync::OnceLock;

use regex::Regex;
use url::Url;

fn magnet_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)^magnet:\?xt=urn:[a-z0-9-]+:(?:[a-z2-7]{32}|[a-f0-9]{40})(?:&|$)")
            .expect("valid regex")
    })
}

/// Accepts classic 32-char base32 info hashes and 40-char hex btih hashes.
pub fn is_valid_magnet_url(magnet_uri: &str) -> bool {
    magnet_pattern().is_match(magnet_uri.trim())
}

pub fn is_valid_download_url(download_url: &str) -> bool {
    match Url::parse(download_url.trim()) {
        Ok(parsed) => matches!(parsed.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_32_and_40_char_hashes() {
        assert!(is_valid_magnet_url(
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10"
        ));
        assert!(is_valid_magnet_url(
            "magnet:?xt=urn:btih:YNCKHTQCWBTRNJIV4WNAE52SJUQCZO5C"
        ));
        assert!(is_valid_magnet_url(
            "  magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=ubuntu  "
        ));
        assert!(!is_valid_magnet_url("https://example.com/file.bin"));
        assert!(!is_valid_magnet_url("magnet:?xt=urn:btih:short"));
        assert!(!is_valid_magnet_url(
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a1"
        ));
        assert!(!is_valid_magnet_url(""));
    }

    #[test]
    fn http_urls_only() {
        assert!(is_valid_download_url("https://example.com/file.iso"));
        assert!(is_valid_download_url("http://example.com/file.iso"));
        assert!(is_valid_download_url("  https://example.com/a  "));
        assert!(!is_valid_download_url("ftp://example.com/file.iso"));
        assert!(!is_valid_download_url("not a url"));
        assert!(!is_valid_download_url(""));
    }
}

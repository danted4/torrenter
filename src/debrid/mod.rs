//! Debrid route: magnet → provider API → HTTPS links → the HTTP engine.
//!
//! Keys never leave this module in clear text except as the HTTP credential a
//! provider asks for. Errors are built from provider codes and messages, never
//! from request URLs (TorBox puts the key in its download-link query).

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

mod alldebrid;
pub(crate) mod api;
mod debridlink;
#[cfg(test)]
pub(crate) mod fake;
mod premiumize;
mod realdebrid;
pub mod run;
pub(crate) mod torbox;

/// Debrid providers torrenter can talk to. `offered()` lists the ones the apps
/// show; `verified()` says which were confirmed with a real account (the rest
/// are labelled experimental).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    TorBox,
    RealDebrid,
    AllDebrid,
    Premiumize,
    DebridLink,
}

impl Service {
    pub const ALL: [Service; 5] = [
        Service::TorBox,
        Service::RealDebrid,
        Service::AllDebrid,
        Service::Premiumize,
        Service::DebridLink,
    ];

    /// Stable id used in config, `jobs.json`, CLI and MCP.
    pub fn id(self) -> &'static str {
        match self {
            Service::TorBox => "torbox",
            Service::RealDebrid => "realdebrid",
            Service::AllDebrid => "alldebrid",
            Service::Premiumize => "premiumize",
            Service::DebridLink => "debridlink",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Service::TorBox => "TorBox",
            Service::RealDebrid => "Real-Debrid",
            Service::AllDebrid => "AllDebrid",
            Service::Premiumize => "Premiumize",
            Service::DebridLink => "Debrid-Link",
        }
    }

    /// Desktop-only override, read-only: `TORRENTER_TORBOX_KEY`, ….
    pub fn env_var(self) -> String {
        format!("TORRENTER_{}_KEY", self.id().to_ascii_uppercase())
    }

    /// Most parallel connections per file the provider tolerates.
    pub fn max_connections(self) -> usize {
        match self {
            Service::TorBox => 16,
            Service::RealDebrid | Service::AllDebrid | Service::DebridLink => 8,
            Service::Premiumize => 1,
        }
    }

    pub fn parse(value: &str) -> Option<Service> {
        let wanted = value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_', ' '], "");
        Service::ALL
            .into_iter()
            .find(|service| service.id() == wanted)
    }

    /// Providers the fronts list and the session runs. All of them today; a
    /// provider whose API breaks can be withheld here until it is fixed.
    pub fn offered() -> &'static [Service] {
        &Service::ALL
    }

    /// Confirmed end to end with a real account. The others are built from
    /// the provider's docs and tested against fakes only, so the apps label
    /// them experimental and ask for bug reports. One becomes verified once
    /// someone confirms it with a real account.
    pub fn verified(self) -> bool {
        matches!(self, Service::TorBox)
    }

    /// The provider as pickers show it: "Real-Debrid (experimental)".
    pub fn menu_label(self) -> String {
        if self.verified() {
            self.label().to_string()
        } else {
            format!("{} (experimental)", self.label())
        }
    }

    /// Appended to a failed job's error on an experimental provider.
    pub fn report_hint(self) -> Option<String> {
        (!self.verified()).then(|| {
            format!(
                "{} support is experimental; if this looks wrong, please report it at github.com/danted4/torrenter/issues",
                self.label()
            )
        })
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A provider's download link, only over HTTPS: an answer must not move the
/// payload to plain HTTP. Plain HTTP is allowed to loopback only (the tests'
/// fake servers).
fn secure_link(link: String) -> Result<String, DebridError> {
    let ok = url::Url::parse(&link).is_ok_and(|url| match url.scheme() {
        "https" => true,
        "http" => {
            matches!(url.host(), Some(url::Host::Domain("localhost")))
                || matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
                || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        }
        _ => false,
    });
    if ok {
        Ok(link)
    } else {
        Err(DebridError::Service(
            "the provider returned a download link that is not HTTPS".into(),
        ))
    }
}

/// Whether a failed job's error could be an adapter fault worth a report: an
/// answer the adapter did not understand, a torrent that vanished again, or a
/// file list with nothing safe to save. Not a bad key, a plan, blocked
/// content, the network, rate limits or the stall timeout.
pub(crate) fn worth_reporting(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<DebridError>(),
            Some(DebridError::Service(_) | DebridError::Gone)
        )
    })
}

/// An API key. `Debug`/`Display` never print it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Trims whitespace; `None` for an empty key.
    pub fn new(value: impl Into<String>) -> Option<Secret> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(Secret(trimmed.to_string()))
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What the account check reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub premium: bool,
    /// Provider-formatted expiry (ISO 8601 or similar), shown as-is.
    pub expires: Option<String>,
}

/// A torrent on the provider's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub remote_id: String,
    /// False when the provider already had it on the account (re-used), so
    /// "remove after download" must leave it alone.
    pub added_by_us: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// Provider id for one file (TorBox file id, RD link index, …).
    pub id: String,
    /// Path as the provider reports it. Untrusted: see `safe_relative_path`.
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    /// Provider still fetching; per-mille progress (0–1000) when it says.
    Fetching {
        permille: u16,
    },
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub state: RemoteState,
    pub files: Vec<RemoteFile>,
}

/// Provider errors. `Display` text is safe to show and persist: built from the
/// provider's own code/detail, never from a request URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebridError {
    BadKey(String),
    NotPremium(String),
    /// Too many requests; wait this long (or a default backoff).
    RateLimited(Option<Duration>),
    /// The provider refuses this content (e.g. RD `infringing_file`). Shown,
    /// never worked around.
    Blocked(String),
    /// The remote torrent no longer exists on the account.
    Gone,
    Service(String),
    Network(String),
}

impl fmt::Display for DebridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DebridError::BadKey(detail) => write!(f, "API key rejected: {detail}"),
            DebridError::NotPremium(detail) => write!(f, "account is not premium: {detail}"),
            DebridError::RateLimited(_) => f.write_str("rate limited; slowing down"),
            DebridError::Blocked(detail) => write!(f, "refused by the provider: {detail}"),
            DebridError::Gone => f.write_str("torrent no longer on the account"),
            DebridError::Service(detail) => f.write_str(detail),
            DebridError::Network(detail) => write!(f, "network error: {detail}"),
        }
    }
}

impl std::error::Error for DebridError {}

impl DebridError {
    /// The same error with links and the key stripped from its text.
    pub fn redacted(self, key: &Secret) -> Self {
        let clean = |detail: String| redact(&detail, Some(key));
        match self {
            DebridError::BadKey(detail) => DebridError::BadKey(clean(detail)),
            DebridError::NotPremium(detail) => DebridError::NotPremium(clean(detail)),
            DebridError::Blocked(detail) => DebridError::Blocked(clean(detail)),
            DebridError::Service(detail) => DebridError::Service(clean(detail)),
            DebridError::Network(detail) => DebridError::Network(clean(detail)),
            other => other,
        }
    }
}

/// Replace anything URL-shaped (and the key, if given) so provider text can be
/// shown, saved in `jobs.json` or returned over MCP without leaking a link.
pub fn redact(text: &str, key: Option<&Secret>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("http://").or_else(|| rest.find("https://")) {
        let start = match (rest.find("http://"), rest.find("https://")) {
            (Some(a), Some(b)) => a.min(b),
            _ => start,
        };
        out.push_str(&rest[..start]);
        out.push_str("<link>");
        let tail = &rest[start..];
        let end = tail
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | '"' | '\'' | '>' | ']'))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    match key {
        Some(secret) if !secret.expose().is_empty() => out.replace(secret.expose(), "<key>"),
        _ => out,
    }
}

const SEGMENT_MAX_BYTES: usize = 200;
const PATH_MAX_DEPTH: usize = 32;

/// Turn a provider-reported path into a relative path that cannot leave the
/// job folder. `None` for `..`, absolute or drive-prefixed paths, or nothing usable.
pub fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let normalized = raw.replace('\\', "/");
    let bytes = normalized.as_bytes();
    let drive_absolute = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes.get(2), None | Some(b'/'));
    if normalized.starts_with('/') || drive_absolute {
        return None;
    }
    let mut out = PathBuf::new();
    let mut depth = 0;
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        let clean = sanitize_segment(segment)?;
        depth += 1;
        if depth > PATH_MAX_DEPTH {
            return None;
        }
        out.push(clean);
    }
    let only_normal = out
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    (depth > 0 && only_normal).then_some(out)
}

fn sanitize_segment(segment: &str) -> Option<String> {
    let mut cleaned: String = segment
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect();
    // Windows drops trailing dots and spaces, which would alias other names.
    while cleaned.ends_with(['.', ' ']) {
        cleaned.pop();
    }
    let cleaned = cleaned.trim_start().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    let cleaned = truncate_bytes(&cleaned, SEGMENT_MAX_BYTES);
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved {
        format!("_{cleaned}")
    } else {
        cleaned
    })
}

fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// True when `path` stays inside `root` after joining (defence in depth).
pub(crate) fn inside(root: &Path, relative: &Path) -> bool {
    relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        && root.join(relative).starts_with(root)
}

/// One configured provider. Adapters are self-contained files so each can be
/// merged on its own once verified against a real account.
pub(crate) enum Provider {
    TorBox(torbox::TorBox),
    RealDebrid(realdebrid::RealDebrid),
    AllDebrid(alldebrid::AllDebrid),
    Premiumize(premiumize::Premiumize),
    DebridLink(debridlink::DebridLink),
}

impl Provider {
    pub(crate) fn new(
        service: Service,
        key: Secret,
        base: Option<url::Url>,
    ) -> Result<Provider, DebridError> {
        Ok(match service {
            Service::TorBox => Provider::TorBox(torbox::TorBox::new(key, base)?),
            Service::RealDebrid => Provider::RealDebrid(realdebrid::RealDebrid::new(key, base)?),
            Service::AllDebrid => Provider::AllDebrid(alldebrid::AllDebrid::new(key, base)?),
            Service::Premiumize => Provider::Premiumize(premiumize::Premiumize::new(key, base)?),
            Service::DebridLink => Provider::DebridLink(debridlink::DebridLink::new(key, base)?),
        })
    }

    pub(crate) async fn account(&self) -> Result<Account, DebridError> {
        match self {
            Provider::TorBox(api) => api.account().await,
            Provider::RealDebrid(api) => api.account().await,
            Provider::AllDebrid(api) => api.account().await,
            Provider::Premiumize(api) => api.account().await,
            Provider::DebridLink(api) => api.account().await,
        }
    }

    pub(crate) async fn add(&self, magnet: &str, hash_hex: &str) -> Result<Added, DebridError> {
        match self {
            Provider::TorBox(api) => api.add(magnet, hash_hex).await,
            Provider::RealDebrid(api) => api.add(magnet, hash_hex).await,
            Provider::AllDebrid(api) => api.add(magnet, hash_hex).await,
            Provider::Premiumize(api) => api.add(magnet, hash_hex).await,
            Provider::DebridLink(api) => api.add(magnet, hash_hex).await,
        }
    }

    pub(crate) async fn status(&self, remote_id: &str) -> Result<Remote, DebridError> {
        match self {
            Provider::TorBox(api) => api.status(remote_id).await,
            Provider::RealDebrid(api) => api.status(remote_id).await,
            Provider::AllDebrid(api) => api.status(remote_id).await,
            Provider::Premiumize(api) => api.status(remote_id).await,
            Provider::DebridLink(api) => api.status(remote_id).await,
        }
    }

    pub(crate) async fn link(
        &self,
        remote_id: &str,
        file: &RemoteFile,
    ) -> Result<String, DebridError> {
        let link = match self {
            Provider::TorBox(api) => api.link(remote_id, file).await,
            Provider::RealDebrid(api) => api.link(remote_id, file).await,
            Provider::AllDebrid(api) => api.link(remote_id, file).await,
            Provider::Premiumize(api) => api.link(remote_id, file).await,
            Provider::DebridLink(api) => api.link(remote_id, file).await,
        }?;
        secure_link(link)
    }

    pub(crate) async fn delete(&self, remote_id: &str) -> Result<(), DebridError> {
        match self {
            Provider::TorBox(api) => api.delete(remote_id).await,
            Provider::RealDebrid(api) => api.delete(remote_id).await,
            Provider::AllDebrid(api) => api.delete(remote_id).await,
            Provider::Premiumize(api) => api.delete(remote_id).await,
            Provider::DebridLink(api) => api.delete(remote_id).await,
        }
    }
}

/// Delete a torrent from the provider outside a running job (clearing a job
/// that never finished). The error text is safe to show.
pub async fn delete_remote(
    service: Service,
    key: &Secret,
    remote_id: &str,
    base: Option<url::Url>,
) -> Result<(), DebridError> {
    let result = match Provider::new(service, key.clone(), base) {
        Ok(provider) => provider.delete(remote_id).await,
        Err(error) => Err(error),
    };
    result.map_err(|error| error.redacted(key))
}

/// Settings "Test" button / `torrenter debrid test`: ask the provider who the
/// key belongs to. The error text is safe to show.
pub async fn check_account(service: Service, key: &Secret) -> Result<Account, DebridError> {
    let result = match Provider::new(service, key.clone(), None) {
        Ok(provider) => provider.account().await,
        Err(error) => Err(error),
    };
    result.map_err(|error| error.redacted(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_is_offered_and_only_torbox_is_verified() {
        assert_eq!(Service::offered(), &Service::ALL);
        let verified: Vec<_> = Service::ALL
            .into_iter()
            .filter(|service| service.verified())
            .collect();
        assert_eq!(verified, [Service::TorBox]);
        assert_eq!(Service::TorBox.menu_label(), "TorBox");
        assert_eq!(
            Service::RealDebrid.menu_label(),
            "Real-Debrid (experimental)"
        );
        assert_eq!(Service::TorBox.report_hint(), None);
        let hint = Service::DebridLink.report_hint().unwrap();
        assert!(
            hint.starts_with("Debrid-Link support is experimental")
                && hint.contains("github.com/danted4/torrenter/issues")
                // Error texts never carry a link (leak checks look for it).
                && !hint.contains("http"),
            "{hint}"
        );
    }

    #[test]
    fn only_adapter_faults_ask_for_a_report() {
        let worth = |error: DebridError| worth_reporting(&anyhow::Error::from(error));
        assert!(worth(DebridError::Service("unexpected answer".into())));
        assert!(worth(DebridError::Gone));
        assert!(!worth(DebridError::BadKey("bad_token".into())));
        assert!(!worth(DebridError::NotPremium("free".into())));
        assert!(!worth(DebridError::Blocked("infringing_file".into())));
        assert!(!worth(DebridError::RateLimited(None)));
        assert!(!worth(DebridError::Network("timed out".into())));
        // The stall timeout and rate-limit exhaustion are plain messages.
        assert!(!worth_reporting(&anyhow::anyhow!(
            "Real-Debrid made no progress on this torrent for 30 minutes"
        )));
    }

    #[test]
    fn download_links_must_be_https() {
        for good in [
            "https://cdn.example/f/1",
            "http://127.0.0.1:8080/f/1",
            "http://localhost/f",
            "http://[::1]/f",
        ] {
            assert_eq!(secure_link(good.into()).as_deref(), Ok(good));
        }
        for bad in [
            "http://cdn.example/f/1",
            "http://10.0.0.5/f",
            "ftp://cdn.example/f",
            "not a url",
        ] {
            assert!(secure_link(bad.into()).is_err(), "{bad}");
        }
    }

    #[test]
    fn redacted_errors_drop_links_and_the_key() {
        let key = Secret::new("sekret-key").unwrap();
        let error = DebridError::Service(
            "delete failed at https://api.example/x?token=sekret-key for sekret-key".into(),
        )
        .redacted(&key);
        let text = error.to_string();
        assert!(
            !text.contains("sekret-key") && !text.contains("https://"),
            "{text}"
        );
        assert_eq!(DebridError::Gone.redacted(&key), DebridError::Gone);
    }

    #[test]
    fn service_ids_parse_loosely_and_roundtrip() {
        for service in Service::ALL {
            assert_eq!(Service::parse(service.id()), Some(service));
            assert_eq!(Service::parse(service.label()), Some(service));
        }
        assert_eq!(Service::parse("Real-Debrid"), Some(Service::RealDebrid));
        assert_eq!(Service::parse("debrid_link"), Some(Service::DebridLink));
        assert_eq!(Service::parse("nope"), None);
        assert_eq!(Service::TorBox.env_var(), "TORRENTER_TORBOX_KEY");
        assert_eq!(Service::RealDebrid.env_var(), "TORRENTER_REALDEBRID_KEY");
    }

    #[test]
    fn provider_paths_cannot_escape_the_job_folder() {
        assert_eq!(
            safe_relative_path("Album/CD1/01 Intro.flac"),
            Some(PathBuf::from("Album/CD1/01 Intro.flac"))
        );
        assert_eq!(
            safe_relative_path("Album\\CD1\\a.bin"),
            Some(PathBuf::from("Album/CD1/a.bin"))
        );
        assert_eq!(safe_relative_path("./a//b"), Some(PathBuf::from("a/b")));
        for bad in [
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:\\x",
            "c:/x",
            "",
            "./",
            "..",
        ] {
            assert_eq!(safe_relative_path(bad), None, "{bad:?}");
        }
        assert_eq!(
            safe_relative_path("a:b?c*.txt"),
            Some(PathBuf::from("a_b_c_.txt"))
        );
        assert_eq!(
            safe_relative_path("CON.txt"),
            Some(PathBuf::from("_CON.txt"))
        );
        assert_eq!(safe_relative_path("com7"), Some(PathBuf::from("_com7")));
        assert_eq!(safe_relative_path("name. "), Some(PathBuf::from("name")));
        let deep = vec!["d"; 40].join("/");
        assert_eq!(safe_relative_path(&deep), None);
        let long = "é".repeat(300);
        let cut = safe_relative_path(&long).unwrap();
        assert!(cut.to_str().unwrap().len() <= 200);
        assert!(inside(Path::new("/root/job"), Path::new("a/b")));
    }

    #[test]
    fn redact_removes_links_and_keys() {
        let key = Secret::new("sk-123").unwrap();
        let text =
            "error sending request for url (https://cdn.example/dld/x?token=sk-123): 403 sk-123";
        let clean = redact(text, Some(&key));
        assert!(
            !clean.contains("cdn.example") && !clean.contains("sk-123"),
            "{clean}"
        );
        assert!(clean.contains("<link>") && clean.contains("<key>"));
        assert_eq!(redact("plain text", None), "plain text");
    }

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("  abc123  ").unwrap();
        assert_eq!(secret.expose(), "abc123");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert!(Secret::new("   ").is_none());
    }
}

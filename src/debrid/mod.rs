//! Debrid route: magnet → provider API → HTTPS links → the HTTP engine.
//!
//! Keys never leave this module in clear text except as the HTTP credential a
//! provider asks for. Errors are built from provider codes and messages, never
//! from request URLs (TorBox puts the key in its download-link query).

use std::fmt;

/// Debrid providers torrenter can talk to. `shipped()` lists the ones verified
/// against a real account; only those are offered in the apps.
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

    /// Providers verified end to end with a real account. Others stay on the
    /// feature branch until someone confirms them.
    pub fn shipped() -> &'static [Service] {
        &[Service::TorBox]
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn secrets_never_print() {
        let secret = Secret::new("  abc123  ").unwrap();
        assert_eq!(secret.expose(), "abc123");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert!(Secret::new("   ").is_none());
    }
}

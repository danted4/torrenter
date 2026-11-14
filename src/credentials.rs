//! Desktop debrid keys: `credentials.json` next to `config.json` (owner-only,
//! atomic), overridden per service by `TORRENTER_<SERVICE>_KEY`. Android and
//! iOS keep keys in the Keystore / Keychain and hand them to the engine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::debrid::{Secret, Service};
use crate::fsutil::{atomic_write, quarantine, recover_incomplete_save};
use crate::update::config_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `TORRENTER_<SERVICE>_KEY`: read-only, never written to disk.
    Env,
    File,
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    keys: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_path()?.with_file_name("credentials.json"))
}

fn env_key(service: Service) -> Option<Secret> {
    std::env::var(service.env_var()).ok().and_then(Secret::new)
}

/// The key to use for `service`: environment first, then the file.
pub fn key_for(service: Service) -> Option<(Secret, KeySource)> {
    let path = credentials_path().ok()?;
    key_at(service, &path, env_key)
}

/// Every service with a key, and where it comes from.
pub fn configured() -> Vec<(Service, KeySource)> {
    Service::ALL
        .into_iter()
        .filter_map(|service| key_for(service).map(|(_, source)| (service, source)))
        .collect()
}

pub fn set_key(service: Service, key: &Secret) -> Result<()> {
    set_key_at(&credentials_path()?, service, Some(key))
}

pub fn remove_key(service: Service) -> Result<()> {
    set_key_at(&credentials_path()?, service, None)
}

fn read_file(path: &Path) -> Result<CredentialsFile> {
    recover_incomplete_save(path);
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("failed to parse credentials"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CredentialsFile::default())
        }
        Err(error) => Err(error).context("failed to read credentials"),
    }
}

fn key_at(
    service: Service,
    path: &Path,
    env: impl Fn(Service) -> Option<Secret>,
) -> Option<(Secret, KeySource)> {
    if let Some(secret) = env(service) {
        return Some((secret, KeySource::Env));
    }
    let file = read_file(path).ok()?;
    file.keys
        .get(service.id())
        .and_then(|value| Secret::new(value.as_str()))
        .map(|secret| (secret, KeySource::File))
}

fn set_key_at(path: &Path, service: Service, key: Option<&Secret>) -> Result<()> {
    let mut file = match read_file(path) {
        Ok(file) => file,
        // Keep unreadable bytes as credentials.json.bak rather than overwrite them.
        Err(_) => {
            quarantine(path);
            CredentialsFile::default()
        }
    };
    match key {
        Some(secret) => {
            file.keys
                .insert(service.id().to_string(), secret.expose().to_string());
        }
        None => {
            file.keys.remove(service.id());
        }
    }
    let bytes = serde_json::to_vec_pretty(&file).context("failed to serialize credentials")?;
    atomic_write(path, &bytes, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: Service) -> Option<Secret> {
        None
    }

    #[test]
    fn keys_roundtrip_and_env_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        assert!(key_at(Service::TorBox, &path, no_env).is_none());

        let key = Secret::new("file-key").unwrap();
        set_key_at(&path, Service::TorBox, Some(&key)).unwrap();
        let (found, source) = key_at(Service::TorBox, &path, no_env).unwrap();
        assert_eq!(found.expose(), "file-key");
        assert_eq!(source, KeySource::File);

        let env = |service: Service| {
            (service == Service::TorBox).then(|| Secret::new("env-key").unwrap())
        };
        let (found, source) = key_at(Service::TorBox, &path, env).unwrap();
        assert_eq!(found.expose(), "env-key");
        assert_eq!(source, KeySource::Env);
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("env-key"),
            "env keys are never written"
        );

        set_key_at(&path, Service::TorBox, None).unwrap();
        assert!(key_at(Service::TorBox, &path, no_env).is_none());
    }

    #[test]
    fn broken_file_is_kept_aside_not_overwritten_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(key_at(Service::TorBox, &path, no_env).is_none());
        set_key_at(&path, Service::RealDebrid, Some(&Secret::new("k").unwrap())).unwrap();
        assert!(crate::fsutil::bak_path(&path).exists());
        assert_eq!(
            key_at(Service::RealDebrid, &path, no_env)
                .unwrap()
                .0
                .expose(),
            "k"
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        set_key_at(&path, Service::TorBox, Some(&Secret::new("k").unwrap())).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

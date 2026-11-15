//! Desktop debrid accounts: `credentials.json` next to `config.json`
//! (owner-only, atomic). Each account is a named key for one provider, so a
//! provider can have several. `TORRENTER_<SERVICE>_KEY` adds a read-only
//! account per provider. Android and iOS keep keys in the Keystore /
//! Keychain and hand them to the engine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::debrid::{Secret, Service};
use crate::fsutil::{atomic_write, edit_lock, quarantine, recover_incomplete_save};
use crate::update::config_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `TORRENTER_<SERVICE>_KEY`: read-only, never written to disk.
    Env,
    File,
}

/// A debrid account without its key: safe to show and to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    /// Stable id jobs and the remembered route refer to (renames never break
    /// them). Keys saved before accounts existed use the provider id.
    pub id: String,
    pub name: String,
    pub service: Service,
    pub source: KeySource,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredAccount {
    id: String,
    name: String,
    /// Kept as text so an account for a provider this build lacks survives.
    service: String,
    key: String,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    accounts: Vec<StoredAccount>,
    /// Earlier format (one key per provider); read once, then rewritten as
    /// accounts.
    #[serde(default, skip_serializing)]
    keys: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl CredentialsFile {
    fn upgrade_legacy(&mut self) {
        for (service_id, key) in std::mem::take(&mut self.keys) {
            let Some(service) = Service::parse(&service_id) else {
                continue;
            };
            if self
                .accounts
                .iter()
                .any(|account| account.id == service.id())
            {
                continue;
            }
            self.accounts.push(StoredAccount {
                id: service.id().to_string(),
                name: service.label().to_string(),
                service: service.id().to_string(),
                key,
                extra: BTreeMap::new(),
            });
        }
    }
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_path()?.with_file_name("credentials.json"))
}

fn env_key(service: Service) -> Option<Secret> {
    std::env::var(service.env_var()).ok().and_then(Secret::new)
}

/// The id of the read-only account a `TORRENTER_<SERVICE>_KEY` provides.
pub fn env_account_id(service: Service) -> String {
    format!("env:{}", service.id())
}

/// Every account: saved ones in the order they were added, then env ones.
pub fn accounts() -> Vec<AccountInfo> {
    match credentials_path() {
        Ok(path) => accounts_at(&path, env_key),
        Err(_) => env_accounts(env_key),
    }
}

/// The provider and key for an account id.
pub fn account_key(id: &str) -> Option<(Service, Secret)> {
    let path = credentials_path().ok()?;
    account_key_at(id, &path, env_key)
}

/// Find an account by id, by name (any case), or by provider when exactly
/// one account uses it (`torbox`, `TorBox`). For the CLI and MCP.
pub fn resolve(query: &str) -> Option<AccountInfo> {
    resolve_in(&accounts(), query)
}

/// Save a new account. An empty name becomes the provider's name ("TorBox",
/// then "TorBox 2", …); names are unique, ignoring case.
pub fn add_account(service: Service, name: &str, key: &Secret) -> Result<AccountInfo> {
    add_account_at(&credentials_path()?, service, name, key, env_key)
}

pub fn remove_account(id: &str) -> Result<()> {
    remove_account_at(&credentials_path()?, id)
}

/// The name a new account for `service` gets when none is typed.
pub fn default_name(service: Service, taken: &[AccountInfo]) -> String {
    let base = service.label();
    let free = |name: &str| !taken.iter().any(|a| a.name.eq_ignore_ascii_case(name));
    if free(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|name| free(name))
        .unwrap_or_else(|| base.to_string())
}

fn read_file(path: &Path) -> Result<CredentialsFile> {
    recover_incomplete_save(path);
    let mut file = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<CredentialsFile>(&bytes)
            .context("failed to parse credentials")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CredentialsFile::default(),
        Err(error) => return Err(error).context("failed to read credentials"),
    };
    file.upgrade_legacy();
    Ok(file)
}

fn accounts_at(path: &Path, env: impl Fn(Service) -> Option<Secret>) -> Vec<AccountInfo> {
    let mut list: Vec<AccountInfo> = read_file(path)
        .map(|file| file.accounts)
        .unwrap_or_default()
        .into_iter()
        .filter(|stored| Secret::new(stored.key.as_str()).is_some())
        .filter_map(|stored| {
            Some(AccountInfo {
                service: Service::parse(&stored.service)?,
                id: stored.id,
                name: stored.name,
                source: KeySource::File,
            })
        })
        .collect();
    list.extend(env_accounts(env));
    list
}

fn env_accounts(env: impl Fn(Service) -> Option<Secret>) -> Vec<AccountInfo> {
    Service::ALL
        .into_iter()
        .filter(|service| env(*service).is_some())
        .map(|service| AccountInfo {
            id: env_account_id(service),
            name: format!("{} (env)", service.label()),
            service,
            source: KeySource::Env,
        })
        .collect()
}

fn account_key_at(
    id: &str,
    path: &Path,
    env: impl Fn(Service) -> Option<Secret>,
) -> Option<(Service, Secret)> {
    if let Some(service) = id.strip_prefix("env:").and_then(Service::parse) {
        return env(service).map(|key| (service, key));
    }
    let file = read_file(path).ok()?;
    let stored = file.accounts.into_iter().find(|account| account.id == id)?;
    Some((Service::parse(&stored.service)?, Secret::new(stored.key)?))
}

fn resolve_in(accounts: &[AccountInfo], query: &str) -> Option<AccountInfo> {
    let query = query.trim();
    // Ids first across every account, then names, so no name can shadow an id.
    if let Some(found) = accounts
        .iter()
        .find(|account| account.id == query)
        .or_else(|| {
            accounts
                .iter()
                .find(|account| account.name.eq_ignore_ascii_case(query))
        })
    {
        return Some(found.clone());
    }
    let service = Service::parse(query)?;
    let mut matching = accounts.iter().filter(|account| account.service == service);
    let first = matching.next()?;
    matching.next().is_none().then(|| first.clone())
}

static CREDENTIALS_EDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read-modify-write under the edit lock. A file that no longer parses is
/// kept as credentials.json.bak rather than overwritten in place.
fn edit_file<T>(path: &Path, change: impl FnOnce(&mut CredentialsFile) -> Result<T>) -> Result<T> {
    let _edit = edit_lock(&CREDENTIALS_EDIT);
    let mut file = match read_file(path) {
        Ok(file) => file,
        Err(_) => {
            quarantine(path);
            CredentialsFile::default()
        }
    };
    let out = change(&mut file)?;
    let bytes = serde_json::to_vec_pretty(&file).context("failed to serialize credentials")?;
    atomic_write(path, &bytes, true)?;
    Ok(out)
}

fn new_id(service: Service, taken: &[StoredAccount]) -> String {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    (0u128..)
        .map(|bump| format!("{}-{:06x}", service.id(), (seed + bump) & 0xff_ffff))
        .find(|id| !taken.iter().any(|account| &account.id == id))
        .unwrap_or_else(|| service.id().to_string())
}

fn add_account_at(
    path: &Path,
    service: Service,
    name: &str,
    key: &Secret,
    env: impl Fn(Service) -> Option<Secret>,
) -> Result<AccountInfo> {
    let from_env = env_accounts(env);
    edit_file(path, |file| {
        let mut taken: Vec<AccountInfo> = file
            .accounts
            .iter()
            .map(|stored| AccountInfo {
                id: stored.id.clone(),
                name: stored.name.clone(),
                service,
                source: KeySource::File,
            })
            .collect();
        taken.extend(from_env);
        let name = match name.trim() {
            "" => default_name(service, &taken),
            typed => typed.to_string(),
        };
        if taken.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) {
            bail!("an account named \"{name}\" already exists");
        }
        // A name that reads as an id (another account's, or an env one)
        // would be ambiguous for the CLI and MCP.
        if name.to_ascii_lowercase().starts_with("env:")
            || taken.iter().any(|a| a.id.eq_ignore_ascii_case(&name))
        {
            bail!("\"{name}\" is reserved; pick another name");
        }
        let id = new_id(service, &file.accounts);
        file.accounts.push(StoredAccount {
            id: id.clone(),
            name: name.clone(),
            service: service.id().to_string(),
            key: key.expose().to_string(),
            extra: BTreeMap::new(),
        });
        Ok(AccountInfo {
            id,
            name,
            service,
            source: KeySource::File,
        })
    })
}

fn remove_account_at(path: &Path, id: &str) -> Result<()> {
    if let Some(service) = id.strip_prefix("env:").and_then(Service::parse) {
        bail!(
            "that key comes from {}; unset it to remove it",
            service.env_var()
        );
    }
    edit_file(path, |file| {
        file.accounts.retain(|account| account.id != id);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: Service) -> Option<Secret> {
        None
    }

    fn key(text: &str) -> Secret {
        Secret::new(text).unwrap()
    }

    #[test]
    fn several_accounts_per_provider_with_unique_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        assert!(accounts_at(&path, no_env).is_empty());

        let first = add_account_at(&path, Service::TorBox, "", &key("k1"), no_env).unwrap();
        assert_eq!(first.name, "TorBox");
        let second = add_account_at(&path, Service::TorBox, "", &key("k2"), no_env).unwrap();
        assert_eq!(second.name, "TorBox 2");
        let work = add_account_at(&path, Service::TorBox, " work ", &key("k3"), no_env).unwrap();
        assert_eq!(work.name, "work");
        assert_ne!(first.id, second.id);
        let error = add_account_at(&path, Service::TorBox, "WORK", &key("k4"), no_env)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");

        let listed: Vec<String> = accounts_at(&path, no_env)
            .into_iter()
            .map(|account| account.name)
            .collect();
        assert_eq!(listed, ["TorBox", "TorBox 2", "work"]);
        let (service, found) = account_key_at(&second.id, &path, no_env).unwrap();
        assert_eq!((service, found.expose()), (Service::TorBox, "k2"));

        // Exact by name (any case) or id; "torbox" is also the first name.
        let all = accounts_at(&path, no_env);
        assert_eq!(resolve_in(&all, "torbox").unwrap().id, first.id);
        assert_eq!(resolve_in(&all, "Work").unwrap().id, work.id);
        assert_eq!(resolve_in(&all, &second.id).unwrap().name, "TorBox 2");

        remove_account_at(&path, &second.id).unwrap();
        assert!(account_key_at(&second.id, &path, no_env).is_none());
        assert_eq!(accounts_at(&path, no_env).len(), 2);
    }

    #[test]
    fn env_keys_are_read_only_accounts_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let env = |service: Service| (service == Service::TorBox).then(|| key("env-key"));
        add_account_at(&path, Service::TorBox, "", &key("file-key"), env).unwrap();
        let all = accounts_at(&path, env);
        assert_eq!(all.len(), 2);
        let from_env = &all[1];
        assert_eq!(from_env.source, KeySource::Env);
        assert_eq!(from_env.name, "TorBox (env)");
        assert_eq!(
            account_key_at(&from_env.id, &path, env).unwrap().1.expose(),
            "env-key"
        );
        assert!(remove_account_at(&path, &from_env.id).is_err());
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("env-key"),
            "env keys are never written"
        );
        assert_eq!(resolve_in(&all[1..], "torbox").unwrap().id, "env:torbox");
    }

    #[test]
    fn names_cannot_shadow_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, r#"{"keys":{"torbox":"old"}}"#).unwrap();
        // The migrated account has id "torbox"; renaming that slot is out.
        let all = accounts_at(&path, no_env);
        assert_eq!(all[0].id, "torbox");
        for bad in ["torbox", "TORBOX", "env:torbox", "Env:anything"] {
            let error = add_account_at(&path, Service::TorBox, bad, &key("k"), no_env)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("reserved") || error.contains("already exists"),
                "{bad}: {error}"
            );
        }
        let work = add_account_at(&path, Service::TorBox, "work", &key("k2"), no_env).unwrap();
        let all = accounts_at(&path, no_env);
        assert_eq!(resolve_in(&all, "torbox").unwrap().id, "torbox");
        assert_eq!(resolve_in(&all, &work.id).unwrap().name, "work");
    }

    #[test]
    fn a_provider_name_resolves_only_to_its_single_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let home = add_account_at(&path, Service::TorBox, "home", &key("k1"), no_env).unwrap();
        assert_eq!(
            resolve_in(&accounts_at(&path, no_env), "torbox")
                .unwrap()
                .id,
            home.id
        );
        add_account_at(&path, Service::TorBox, "work", &key("k2"), no_env).unwrap();
        assert!(
            resolve_in(&accounts_at(&path, no_env), "torbox").is_none(),
            "two TorBox accounts: the name must be given"
        );
    }

    #[test]
    fn keys_from_the_earlier_format_become_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(
            &path,
            r#"{"keys":{"torbox":"old-key","nope":"x"},"future":{"a":1}}"#,
        )
        .unwrap();
        let all = accounts_at(&path, no_env);
        assert_eq!(all.len(), 1);
        assert_eq!(
            (all[0].id.as_str(), all[0].name.as_str()),
            ("torbox", "TorBox")
        );
        assert_eq!(
            account_key_at("torbox", &path, no_env).unwrap().1.expose(),
            "old-key"
        );
        add_account_at(&path, Service::TorBox, "", &key("new"), no_env).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(raw.get("keys").is_none(), "rewritten as accounts");
        assert_eq!(raw["accounts"][0]["id"], "torbox");
        assert_eq!(raw["future"]["a"], 1, "unknown keys kept");
    }

    #[test]
    fn broken_file_is_kept_aside_not_overwritten_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(accounts_at(&path, no_env).is_empty());
        let added = add_account_at(&path, Service::RealDebrid, "", &key("k"), no_env).unwrap();
        assert!(crate::fsutil::bak_path(&path).exists());
        assert_eq!(
            account_key_at(&added.id, &path, no_env).unwrap().1.expose(),
            "k"
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        add_account_at(&path, Service::TorBox, "", &key("k"), no_env).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

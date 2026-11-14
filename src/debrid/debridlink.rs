//! Debrid-Link: adapter not built yet. Research: .cursor/plans/debrid-research/debrid-link.md

use url::Url;

use super::{Account, Added, DebridError, Remote, RemoteFile, Secret};

pub(crate) struct DebridLink;

fn unavailable() -> DebridError {
    DebridError::Service("Debrid-Link is not available in this build".into())
}

impl DebridLink {
    pub fn new(_key: Secret, _base: Option<Url>) -> Result<DebridLink, DebridError> {
        Ok(DebridLink)
    }

    pub async fn account(&self) -> Result<Account, DebridError> {
        Err(unavailable())
    }

    pub async fn add(&self, _magnet: &str, _hash_hex: &str) -> Result<Added, DebridError> {
        Err(unavailable())
    }

    pub async fn status(&self, _remote_id: &str) -> Result<Remote, DebridError> {
        Err(unavailable())
    }

    pub async fn link(&self, _remote_id: &str, _file: &RemoteFile) -> Result<String, DebridError> {
        Err(unavailable())
    }

    pub async fn delete(&self, _remote_id: &str) -> Result<(), DebridError> {
        Err(unavailable())
    }
}

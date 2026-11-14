//! Real-Debrid: adapter not built yet. Research: .cursor/plans/debrid-research/real-debrid.md

use url::Url;

use super::{Account, Added, DebridError, Remote, RemoteFile, Secret};

pub(crate) struct RealDebrid;

fn unavailable() -> DebridError {
    DebridError::Service("Real-Debrid is not available in this build".into())
}

impl RealDebrid {
    pub fn new(_key: Secret, _base: Option<Url>) -> Result<RealDebrid, DebridError> {
        Ok(RealDebrid)
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

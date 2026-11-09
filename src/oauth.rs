//! OAuth credentials for one upstream, persisted in the keyring.
//!
//! rmcp runs the OAuth flow (discovery, registration, PKCE, refresh); this only
//! stores its results, as JSON under `<server>:oauth`.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::transport::auth::{AuthError, CredentialRefreshGuard, CredentialStore, StoredCredentials};
use tokio::sync::Mutex;

use crate::secrets::SecretStore;

pub struct KeyringCredentialStore {
    store: SecretStore,
    key: String,
    refresh_lock: Arc<Mutex<()>>,
}

fn store_err(e: anyhow::Error) -> AuthError {
    AuthError::CredentialStoreError(format!("{e:#}"))
}

impl KeyringCredentialStore {
    pub fn new(store: SecretStore, server: &str) -> Self {
        Self { store, key: format!("{server}:oauth"), refresh_lock: Arc::default() }
    }
}

#[async_trait]
impl CredentialStore for KeyringCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.store.get_json(&self.key).await.map_err(store_err)
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        self.store.set_json(&self.key, &credentials).await.map_err(store_err)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.store.delete(&self.key).await.map_err(store_err)
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let guard = self.refresh_lock.clone().lock_owned().await;
        Ok(Some(CredentialRefreshGuard::new(guard)))
    }
}

/// Every keyring entry that holds credentials for `server`.
pub fn credential_keys(server: &str) -> [String; 2] {
    [format!("{server}:oauth"), format!("{server}:bearer")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let store = SecretStore::memory();
        let s = KeyringCredentialStore::new(store.clone(), "s");
        assert!(s.load().await.unwrap().is_none());
        s.save(StoredCredentials::new("client".into(), None, vec!["a".into()], Some(1))).await.unwrap();
        assert!(store.get("s:oauth").await.unwrap().is_some());
        let creds = s.load().await.unwrap().unwrap();
        assert_eq!((creds.client_id.as_str(), creds.granted_scopes.as_slice()), ("client", ["a".to_owned()].as_slice()));
        s.clear().await.unwrap();
        assert!(s.load().await.unwrap().is_none());
    }
}

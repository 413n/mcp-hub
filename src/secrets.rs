//! Secrets in the OS keyring: the Secret Service (GNOME keyring) on Linux, the
//! login Keychain on macOS. An in-memory variant serves tests.
//!
//! Keys used: `hub:inbound-token`, `<server>:oauth` (OAuth credentials) and
//! `<server>:bearer` (static tokens).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Clone)]
pub enum SecretStore {
    Os,
    Memory(Arc<Mutex<HashMap<String, String>>>),
}

fn check_key(key: &str) -> Result<()> {
    let ok = (1..=128).contains(&key.len())
        && key.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || (i > 0 && matches!(b, b':' | b'.' | b'_' | b'-'))
        });
    if !ok {
        bail!("invalid secret key \"{key}\"");
    }
    Ok(())
}

impl SecretStore {
    pub fn os() -> Self {
        SecretStore::Os
    }

    pub fn memory() -> Self {
        SecretStore::Memory(Arc::default())
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        check_key(key)?;
        match self {
            SecretStore::Os => os::get(key).await,
            SecretStore::Memory(m) => Ok(m.lock().unwrap().get(key).cloned()),
        }
    }

    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        check_key(key)?;
        match self {
            SecretStore::Os => os::set(key, value).await,
            SecretStore::Memory(m) => {
                m.lock().unwrap().insert(key.to_owned(), value.to_owned());
                Ok(())
            }
        }
    }

    /// No error if the key does not exist.
    pub async fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self {
            SecretStore::Os => os::delete(key).await,
            SecretStore::Memory(m) => {
                m.lock().unwrap().remove(key);
                Ok(())
            }
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub async fn set_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.set(key, &serde_json::to_string(value)?).await
    }
}

#[cfg(target_os = "linux")]
mod os {
    //! Items carry the same attributes as libsecret's `secret_password_store`
    //! with schema `dev.mcp-hub.Secret`, so `secret-tool lookup key <key>` finds them.
    use std::collections::HashMap;

    use anyhow::{Context, Result};
    use secret_service::{EncryptionType, SecretService};

    const SCHEMA: &str = "dev.mcp-hub.Secret";

    fn attrs(key: &str) -> HashMap<&str, &str> {
        HashMap::from([("xdg:schema", SCHEMA), ("key", key)])
    }

    async fn connect() -> Result<SecretService<'static>> {
        SecretService::connect(EncryptionType::Dh)
            .await
            .context("cannot reach the Secret Service (GNOME keyring) on the session D-Bus")
    }

    pub async fn get(key: &str) -> Result<Option<String>> {
        let ss = connect().await?;
        let found = ss.search_items(attrs(key)).await?;
        let item = if let Some(item) = found.unlocked.into_iter().next() {
            item
        } else if let Some(item) = found.locked.into_iter().next() {
            // Shows the keyring's unlock prompt.
            item.unlock().await.context("keyring is locked")?;
            item
        } else {
            return Ok(None);
        };
        let secret = item.get_secret().await?;
        Ok(Some(
            String::from_utf8(secret).context("secret is not UTF-8")?,
        ))
    }

    pub async fn set(key: &str, value: &str) -> Result<()> {
        let ss = connect().await?;
        let collection = ss.get_default_collection().await?;
        if collection.is_locked().await? {
            collection.unlock().await.context("keyring is locked")?;
        }
        collection
            .create_item(
                &format!("mcp-hub: {key}"),
                attrs(key),
                value.as_bytes(),
                true,
                "text/plain",
            )
            .await?;
        Ok(())
    }

    pub async fn delete(key: &str) -> Result<()> {
        let ss = connect().await?;
        let found = ss.search_items(attrs(key)).await?;
        for item in found.unlocked {
            item.delete().await?;
        }
        for item in found.locked {
            item.unlock().await.context("keyring is locked")?;
            item.delete().await?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod os {
    //! Generic passwords in the login Keychain: service "mcp-hub", account = key.
    use anyhow::{Context, Result, anyhow};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const SERVICE: &str = "mcp-hub";
    const NOT_FOUND: i32 = -25300; // errSecItemNotFound

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(f).await?
    }

    pub async fn get(key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        blocking(move || match get_generic_password(SERVICE, &key) {
            Ok(bytes) => Ok(Some(
                String::from_utf8(bytes).context("secret is not UTF-8")?,
            )),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(anyhow!("keychain get {key} failed: {e}")),
        })
        .await
    }

    pub async fn set(key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        blocking(move || {
            set_generic_password(SERVICE, &key, value.as_bytes())
                .map_err(|e| anyhow!("keychain set {key} failed: {e}"))
        })
        .await
    }

    pub async fn delete(key: &str) -> Result<()> {
        let key = key.to_owned();
        blocking(move || match delete_generic_password(SERVICE, &key) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == NOT_FOUND => Ok(()),
            Err(e) => Err(anyhow!("keychain delete {key} failed: {e}")),
        })
        .await
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use anyhow::{Result, bail};

    pub async fn get(_: &str) -> Result<Option<String>> {
        bail!("no OS keyring support on this platform")
    }
    pub async fn set(_: &str, _: &str) -> Result<()> {
        bail!("no OS keyring support on this platform")
    }
    pub async fn delete(_: &str) -> Result<()> {
        bail!("no OS keyring support on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for ok in [
            "hub:inbound-token",
            "sentry:oauth",
            "a",
            "my-server.v2:bearer",
        ] {
            assert!(check_key(ok).is_ok(), "{ok}");
        }
        for bad in ["", ":x", "A", "a b", "a/b", &"a".repeat(129)] {
            assert!(check_key(bad).is_err(), "{bad}");
        }
    }
}

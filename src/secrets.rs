//! Secrets in the OS keyring: the Secret Service (GNOME keyring) on Linux, the
//! login Keychain on macOS. An in-memory variant serves tests.
//!
//! Keys used: `hub:inbound-token`, `<server>:oauth` (OAuth credentials),
//! `<server>:bearer` (static tokens) and `<server>:env:<VAR>` (`secretEnv` values).
//!
//! On Linux each key is an item of its own. On macOS all of them are in one Keychain item
//! (see [`vault`]): macOS asks whether a program may read an item once per item, and asks
//! again for every new binary, which each update is.

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

/// All the secrets in one item of a store that keeps items by name (the macOS Keychain),
/// as a JSON object from key to value.
///
/// Up to 0.4 each key was an item of its own, named after the key. Such an item is moved
/// into the vault the first time its key is read, and deleted.
#[cfg(any(test, target_os = "macos"))]
mod vault {
    use std::collections::BTreeMap;

    use anyhow::{Context, Result};

    /// The vault's own item. Not a possible key: keys have no "/".
    pub const ITEM: &str = "mcp-hub/vault";

    /// The store the vault is kept in.
    pub trait Items {
        fn get(&self, item: &str) -> Result<Option<Vec<u8>>>;
        fn set(&self, item: &str, value: &[u8]) -> Result<()>;
        /// No error if there is no such item.
        fn delete(&self, item: &str) -> Result<()>;
        /// Run `change` while no other process or thread changes the vault.
        fn exclusively<T>(&self, change: impl FnOnce() -> Result<T>) -> Result<T>;
    }

    fn load(items: &impl Items) -> Result<BTreeMap<String, String>> {
        match items.get(ITEM)? {
            Some(bytes) => serde_json::from_slice(&bytes).context("the vault is not readable"),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Read the vault, `change` it and write it back if it did change.
    fn update(
        items: &impl Items,
        change: impl FnOnce(&mut BTreeMap<String, String>),
    ) -> Result<()> {
        items.exclusively(|| {
            let mut secrets = load(items)?;
            let before = secrets.clone();
            change(&mut secrets);
            if secrets != before {
                items.set(ITEM, &serde_json::to_vec(&secrets)?)?;
            }
            Ok(())
        })
    }

    pub fn get(items: &impl Items, key: &str) -> Result<Option<String>> {
        if let Some(value) = load(items)?.remove(key) {
            return Ok(Some(value));
        }
        // Kept by an earlier version: move it in.
        let Some(old) = items.get(key)? else {
            return Ok(None);
        };
        let old = String::from_utf8(old).context("secret is not UTF-8")?;
        let mut value = old.clone();
        update(items, |secrets| {
            // Unless another process just stored a newer one.
            value = secrets.entry(key.to_owned()).or_insert(old).clone();
        })?;
        if let Err(e) = items.delete(key) {
            tracing::warn!("cannot delete the old keychain item {key}: {e:#}");
        }
        Ok(Some(value))
    }

    pub fn set(items: &impl Items, key: &str, value: &str) -> Result<()> {
        update(items, |secrets| {
            secrets.insert(key.to_owned(), value.to_owned());
        })
    }

    pub fn delete(items: &impl Items, key: &str) -> Result<()> {
        update(items, |secrets| {
            secrets.remove(key);
        })?;
        // Else it would be moved back in on the next read.
        items.delete(key)
    }

    /// Hold an exclusive lock on the file at `path` until the file returned is dropped.
    #[cfg(unix)]
    pub fn lock(path: &std::path::Path) -> Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        if let Some(dir) = path.parent() {
            crate::stats::private_dir(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns; it is released when it closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot lock {}", path.display()));
        }
        Ok(file)
    }
}

#[cfg(target_os = "macos")]
mod os {
    //! One generic password in the login Keychain, service "mcp-hub", account
    //! [`super::vault::ITEM`], holds every secret.
    use anyhow::{Result, anyhow};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::vault;

    const SERVICE: &str = "mcp-hub";
    const NOT_FOUND: i32 = -25300; // errSecItemNotFound

    struct Keychain;

    impl vault::Items for Keychain {
        fn get(&self, item: &str) -> Result<Option<Vec<u8>>> {
            match get_generic_password(SERVICE, item) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(anyhow!("keychain get {item} failed: {e}")),
            }
        }

        fn set(&self, item: &str, value: &[u8]) -> Result<()> {
            set_generic_password(SERVICE, item, value)
                .map_err(|e| anyhow!("keychain set {item} failed: {e}"))
        }

        fn delete(&self, item: &str) -> Result<()> {
            match delete_generic_password(SERVICE, item) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == NOT_FOUND => Ok(()),
                Err(e) => Err(anyhow!("keychain delete {item} failed: {e}")),
            }
        }

        fn exclusively<T>(&self, change: impl FnOnce() -> Result<T>) -> Result<T> {
            // Released when it goes out of scope, after `change`.
            let _lock = vault::lock(&crate::stats::state_dir().join("keychain.lock"))?;
            change()
        }
    }

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(f).await?
    }

    pub async fn get(key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        blocking(move || vault::get(&Keychain, &key)).await
    }

    pub async fn set(key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        blocking(move || vault::set(&Keychain, &key, &value)).await
    }

    pub async fn delete(key: &str) -> Result<()> {
        let key = key.to_owned();
        blocking(move || vault::delete(&Keychain, &key)).await
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

    /// Items in memory, locked with a file as on macOS.
    struct Items {
        items: Mutex<HashMap<String, Vec<u8>>>,
        lock: std::path::PathBuf,
    }

    impl vault::Items for Items {
        fn get(&self, item: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.items.lock().unwrap().get(item).cloned())
        }
        fn set(&self, item: &str, value: &[u8]) -> Result<()> {
            // Slow, so that writes that are not exclusive would lose each other's change.
            let mut items = self.items.lock().unwrap().clone();
            std::thread::sleep(std::time::Duration::from_millis(1));
            items.insert(item.to_owned(), value.to_vec());
            *self.items.lock().unwrap() = items;
            Ok(())
        }
        fn delete(&self, item: &str) -> Result<()> {
            self.items.lock().unwrap().remove(item);
            Ok(())
        }
        fn exclusively<T>(&self, change: impl FnOnce() -> Result<T>) -> Result<T> {
            let _lock = vault::lock(&self.lock)?;
            change()
        }
    }

    #[test]
    fn vault() {
        let dir = std::env::temp_dir().join(format!("mcp-hub-vault-{}", std::process::id()));
        let items = Arc::new(Items {
            items: Mutex::default(),
            lock: dir.join("keychain.lock"),
        });
        let names = |items: &Items| {
            let mut names: Vec<_> = items.items.lock().unwrap().keys().cloned().collect();
            names.sort();
            names
        };
        // As 0.4 left them: one item per key.
        for (key, value) in [("hub:inbound-token", "t"), ("sentry:oauth", "{}")] {
            vault::Items::set(&*items, key, value.as_bytes()).unwrap();
        }

        assert_eq!(vault::get(&*items, "nothing").unwrap(), None);
        assert_eq!(names(&items), ["hub:inbound-token", "sentry:oauth"]);
        // Moved in when read.
        let token = vault::get(&*items, "hub:inbound-token").unwrap();
        assert_eq!(token.as_deref(), Some("t"));
        assert_eq!(names(&items), [vault::ITEM, "sentry:oauth"]);
        assert_eq!(
            vault::get(&*items, "hub:inbound-token").unwrap().unwrap(),
            "t"
        );

        vault::set(&*items, "a:bearer", "b").unwrap();
        vault::set(&*items, "hub:inbound-token", "t2").unwrap();
        assert_eq!(
            vault::get(&*items, "hub:inbound-token").unwrap().unwrap(),
            "t2"
        );
        // Deleted also where it was before, so that it does not come back.
        vault::delete(&*items, "sentry:oauth").unwrap();
        assert_eq!(vault::get(&*items, "sentry:oauth").unwrap(), None);
        assert_eq!(names(&items), [vault::ITEM]);

        // Writers at the same time lose nothing.
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let items = items.clone();
                std::thread::spawn(move || {
                    for m in 0..5 {
                        vault::set(&*items, &format!("s{n}:env:V{m}"), "x").unwrap();
                    }
                })
            })
            .collect();
        writers.into_iter().for_each(|w| w.join().unwrap());
        for n in 0..8 {
            for m in 0..5 {
                let value = vault::get(&*items, &format!("s{n}:env:V{m}")).unwrap();
                assert_eq!(value.as_deref(), Some("x"));
            }
        }
        assert_eq!(vault::get(&*items, "a:bearer").unwrap().unwrap(), "b");
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

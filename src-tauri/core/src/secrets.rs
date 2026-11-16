//! Where sign-in tokens live: the system keyring (Secret Service: KWallet or
//! GNOME Keyring) when it works, otherwise a file in the app's data folder
//! that only the current user can read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{AppError, AppResult};

pub const KEYRING_SERVICE: &str = "com.socialmediaconnector.desktop";

/// What the operating system calls its credential store.
pub fn system_store_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows Credential Manager"
    } else if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else {
        "your system keyring (Secret Service)"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Keyring,
    File,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Keyring => "keyring",
            Backend::File => "file",
        }
    }
}

pub struct SecretStore {
    backend: Backend,
    file: PathBuf,
    /// For the file backend: its whole content. For the keyring: values read
    /// so far, to avoid a D-Bus round trip on every request.
    cache: Mutex<HashMap<String, String>>,
    file_loaded: Mutex<bool>,
}

impl SecretStore {
    /// Opens the store. `remembered` is the backend chosen on an earlier run;
    /// keeping it avoids losing access to tokens if the keyring is briefly
    /// unavailable. `SMC_SECRET_BACKEND=file` forces the file backend.
    pub fn open(data_dir: &Path, remembered: Option<&str>) -> SecretStore {
        let forced = std::env::var("SMC_SECRET_BACKEND").ok();
        let backend = match forced.as_deref().or(remembered) {
            Some("file") => Backend::File,
            Some("keyring") => Backend::Keyring,
            _ => {
                if keyring_works() {
                    Backend::Keyring
                } else {
                    Backend::File
                }
            }
        };
        log::info!("secret storage: {}", backend.as_str());
        SecretStore {
            backend,
            file: data_dir.join("secrets.json"),
            cache: Mutex::new(HashMap::new()),
            file_loaded: Mutex::new(false),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn get(&self, key: &str) -> AppResult<Option<String>> {
        match self.backend {
            Backend::File => {
                self.load_file()?;
                Ok(self.cache.lock().unwrap().get(key).cloned())
            }
            Backend::Keyring => {
                if let Some(v) = self.cache.lock().unwrap().get(key) {
                    return Ok(Some(v.clone()));
                }
                let value = keyring_get(key)?;
                if let Some(v) = &value {
                    self.cache.lock().unwrap().insert(key.to_string(), v.clone());
                }
                Ok(value)
            }
        }
    }

    pub fn set(&self, key: &str, value: &str) -> AppResult<()> {
        match self.backend {
            Backend::File => {
                self.load_file()?;
                let mut cache = self.cache.lock().unwrap();
                cache.insert(key.to_string(), value.to_string());
                write_file(&self.file, &cache)
            }
            Backend::Keyring => {
                keyring_set(key, value)?;
                self.cache.lock().unwrap().insert(key.to_string(), value.to_string());
                Ok(())
            }
        }
    }

    pub fn delete(&self, key: &str) -> AppResult<()> {
        match self.backend {
            Backend::File => {
                self.load_file()?;
                let mut cache = self.cache.lock().unwrap();
                if cache.remove(key).is_some() {
                    write_file(&self.file, &cache)?;
                }
                Ok(())
            }
            Backend::Keyring => {
                self.cache.lock().unwrap().remove(key);
                keyring_delete(key)
            }
        }
    }

    fn load_file(&self) -> AppResult<()> {
        let mut loaded = self.file_loaded.lock().unwrap();
        if *loaded {
            return Ok(());
        }
        if self.file.exists() {
            let text = std::fs::read_to_string(&self.file)?;
            let map: HashMap<String, String> = if text.trim().is_empty() {
                HashMap::new()
            } else {
                serde_json::from_str(&text)?
            };
            *self.cache.lock().unwrap() = map;
        }
        *loaded = true;
        Ok(())
    }
}

fn keyring_err(e: keyring::Error) -> AppError {
    AppError::Secret(format!("the system keyring is not available ({e})"))
}

// Windows Credential Manager stores at most 2560 bytes per entry, as UTF-16
// (1280 code units); sign-ins with two tokens are longer than that. Long
// values are therefore split over several entries: the main entry holds a
// marker with the part count, and the parts live under `<key>#<n>`.
const CHUNK_UNITS: usize = 1200;
const CHUNK_MARKER: &str = "\u{1}smc-chunks:";

/// Splits `value` into pieces of at most `CHUNK_UNITS` UTF-16 code units.
pub(crate) fn split_chunks(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for c in value.chars() {
        let n = c.len_utf16();
        if units + n > CHUNK_UNITS {
            parts.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(c);
        units += n;
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

fn chunk_count(stored: &str) -> Option<usize> {
    stored.strip_prefix(CHUNK_MARKER).and_then(|n| n.parse().ok())
}

fn entry(key: &str) -> AppResult<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, key).map_err(keyring_err)
}

fn raw_get(key: &str) -> AppResult<Option<String>> {
    match entry(key)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(keyring_err(e)),
    }
}

fn raw_delete(key: &str) -> AppResult<()> {
    match entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(keyring_err(e)),
    }
}

fn keyring_get(key: &str) -> AppResult<Option<String>> {
    let Some(stored) = raw_get(key)? else { return Ok(None) };
    let Some(n) = chunk_count(&stored) else { return Ok(Some(stored)) };
    let mut value = String::new();
    for i in 0..n {
        match raw_get(&format!("{key}#{i}"))? {
            Some(part) => value.push_str(&part),
            None => return Err(AppError::Secret(format!("part {i} of a stored sign-in is missing"))),
        }
    }
    Ok(Some(value))
}

fn keyring_set(key: &str, value: &str) -> AppResult<()> {
    let old_parts = raw_get(key)?.as_deref().and_then(chunk_count).unwrap_or(0);
    let parts = split_chunks(value);
    if parts.len() == 1 {
        entry(key)?.set_password(value).map_err(keyring_err)?;
    } else {
        // Write the parts first, so a reader never sees a marker without them.
        for (i, part) in parts.iter().enumerate() {
            entry(&format!("{key}#{i}"))?.set_password(part).map_err(keyring_err)?;
        }
        entry(key)?
            .set_password(&format!("{CHUNK_MARKER}{}", parts.len()))
            .map_err(keyring_err)?;
    }
    let new_parts = if parts.len() == 1 { 0 } else { parts.len() };
    for i in new_parts..old_parts {
        raw_delete(&format!("{key}#{i}"))?;
    }
    Ok(())
}

fn keyring_delete(key: &str) -> AppResult<()> {
    if let Some(n) = raw_get(key)?.as_deref().and_then(chunk_count) {
        for i in 0..n {
            raw_delete(&format!("{key}#{i}"))?;
        }
    }
    raw_delete(key)
}

/// Writes, reads back and deletes a probe entry.
fn keyring_works() -> bool {
    if keyring::Entry::store_status().is_err() {
        return false;
    }
    let probe = || -> Result<bool, keyring::Error> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, "probe")?;
        entry.set_password("ok")?;
        let ok = entry.get_password()? == "ok";
        let _ = entry.delete_credential();
        Ok(ok)
    };
    // The Secret Service can hang when no agent answers; do not wait forever.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => false,
        Ok(Err(e)) => {
            log::warn!("system keyring not usable: {e}");
            false
        }
        Err(_) => {
            log::warn!("system keyring did not answer in time");
            false
        }
    }
}

fn write_file(path: &Path, map: &HashMap<String, String>) -> AppResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(map)?;
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_values_into_keyring_sized_chunks() {
        let short = "x".repeat(CHUNK_UNITS);
        assert_eq!(split_chunks(&short), vec![short.clone()]);
        assert_eq!(split_chunks(""), vec![String::new()]);
        let long = format!("{}{}", "a".repeat(2000), "😀".repeat(700));
        let parts = split_chunks(&long);
        assert!(parts.len() >= 3);
        for p in &parts {
            assert!(p.encode_utf16().count() <= CHUNK_UNITS);
            // Fits the Windows limit of 2560 bytes per entry.
            assert!(p.encode_utf16().count() * 2 <= 2560);
        }
        assert_eq!(parts.concat(), long);
        assert_eq!(chunk_count(&format!("{CHUNK_MARKER}4")), Some(4));
        assert_eq!(chunk_count("{\"accessToken\":\"x\"}"), None);
    }

    #[test]
    fn file_backend_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path(), Some("file"));
        assert_eq!(store.backend(), Backend::File);
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "secret").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("secret"));
        // A new instance reads what the first one wrote.
        let again = SecretStore::open(dir.path(), Some("file"));
        assert_eq!(again.get("a").unwrap().as_deref(), Some("secret"));
        again.delete("a").unwrap();
        assert_eq!(again.get("a").unwrap(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// Round trip through the real system keyring (Windows Credential
    /// Manager, Secret Service, macOS Keychain). It writes test entries, so it
    /// only runs when `SMC_TEST_KEYRING=1` is set (CI does this).
    #[test]
    fn system_keyring_round_trip_with_long_values() {
        if std::env::var("SMC_TEST_KEYRING").as_deref() != Ok("1") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path(), Some("keyring"));
        let key = format!("test:{}", uuid::Uuid::new_v4());
        let long = format!("{{\"accessToken\":\"{}\",\"refreshToken\":\"{}\"}}", "a".repeat(3000), "b".repeat(2500));
        store.set(&key, &long).unwrap();
        let fresh = SecretStore::open(dir.path(), Some("keyring"));
        assert_eq!(fresh.get(&key).unwrap().as_deref(), Some(long.as_str()));
        // Shrinking removes the extra parts.
        fresh.set(&key, "short").unwrap();
        let again = SecretStore::open(dir.path(), Some("keyring"));
        assert_eq!(again.get(&key).unwrap().as_deref(), Some("short"));
        assert_eq!(raw_get(&format!("{key}#0")).unwrap(), None);
        again.set(&key, &long).unwrap();
        again.delete(&key).unwrap();
        let last = SecretStore::open(dir.path(), Some("keyring"));
        assert_eq!(last.get(&key).unwrap(), None);
        assert_eq!(raw_get(&format!("{key}#0")).unwrap(), None);
    }
}

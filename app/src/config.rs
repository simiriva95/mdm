//! Configurazione persistente in `%LOCALAPPDATA%\MDM\config.json`.
//!
//! Tutto quello che prima era hardcoded (cartella, connessioni, soglia
//! dell'estensione, limite di banda) vive qui. Il salvataggio è debounced:
//! gli slider della UI muovono i valori a ogni frame, non serve un write
//! per ognuno.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const MAX_CONNECTIONS_CAP: u64 = 16;
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// vuota = cartella Downloads di sistema
    pub download_dir: PathBuf,
    /// connessioni parallele per download (1..=MAX_CONNECTIONS_CAP)
    pub max_connections: u64,
    /// download che partono insieme; gli altri restano in coda
    pub max_concurrent_downloads: u64,
    /// limite di banda globale in KB/s, 0 = illimitato
    pub speed_limit_kbps: u64,
    /// soglia oltre cui l'estensione passa il download a MDM
    pub size_threshold_mb: u64,
    /// quante volte ritentare da soli un download fallito
    pub auto_retry: u64,
    pub notify_on_complete: bool,
    pub clipboard_watch: bool,
    pub autostart: bool,
    /// connessioni tollerate per host, imparate dai 429 e ricordate tra sessioni
    pub host_conc: HashMap<String, u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::new(),
            max_connections: 8,
            max_concurrent_downloads: 3,
            speed_limit_kbps: 0,
            size_threshold_mb: 10,
            auto_retry: 3,
            notify_on_complete: true,
            clipboard_watch: false,
            autostart: false,
            host_conc: HashMap::new(),
        }
    }
}

impl Config {
    /// Valori fuori range da un file editato a mano non devono rompere l'engine.
    fn clamp(&mut self) {
        self.max_connections = self.max_connections.clamp(1, MAX_CONNECTIONS_CAP);
        self.max_concurrent_downloads = self.max_concurrent_downloads.clamp(1, 20);
        self.size_threshold_mb = self.size_threshold_mb.clamp(1, 100_000);
        self.auto_retry = self.auto_retry.min(10);
    }
}

pub fn path() -> PathBuf {
    crate::engine::data_dir().join("config.json")
}

/// Avvio con Windows: chiave `Run` per l'utente corrente, niente admin.
#[cfg(windows)]
pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_WRITE)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value("MDM", &format!("\"{}\"", exe.display()))?;
    } else {
        // già assente: non è un errore
        let _ = key.delete_value("MDM");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_autostart(_on: bool) -> anyhow::Result<()> {
    Ok(())
}

/// Config condivisa + salvataggio pigro. `get()`/`edit()` sono i soli accessi.
pub struct Store {
    inner: Mutex<Config>,
    dirty: Mutex<Option<Instant>>,
    saving: AtomicBool,
}

/// Store in memoria, senza toccare il disco: è quello che usano i test.
impl Default for Store {
    fn default() -> Self {
        Self { inner: Mutex::new(Config::default()), dirty: Mutex::new(None), saving: AtomicBool::new(false) }
    }
}

impl Store {
    pub fn load() -> Arc<Self> {
        let existing = std::fs::read(path()).ok().and_then(|raw| serde_json::from_slice::<Config>(&raw).ok());
        let missing = existing.is_none();
        let mut cfg = existing.unwrap_or_default();
        cfg.clamp();
        let store =
            Arc::new(Self { inner: Mutex::new(cfg), dirty: Mutex::new(None), saving: AtomicBool::new(false) });
        // primo avvio: scrivi subito i default, così il file esiste ed è
        // ispezionabile senza dover prima toccare qualcosa nella UI
        if missing {
            store.save_now();
        }
        store
    }

    pub fn get(&self) -> Config {
        self.inner.lock().unwrap().clone()
    }

    /// Modifica e programma il salvataggio; il write vero lo fa `flush_due`.
    pub fn edit(&self, f: impl FnOnce(&mut Config)) {
        {
            let mut cfg = self.inner.lock().unwrap();
            f(&mut cfg);
            cfg.clamp();
        }
        *self.dirty.lock().unwrap() = Some(Instant::now());
    }

    /// Scrive su disco se è passato abbastanza tempo dall'ultima modifica.
    /// Da chiamare dal loop UI (o da un task periodico).
    pub fn flush_due(&self) {
        let due = matches!(*self.dirty.lock().unwrap(), Some(at) if at.elapsed() >= SAVE_DEBOUNCE);
        if due {
            self.save_now();
        }
    }

    pub fn save_now(&self) {
        *self.dirty.lock().unwrap() = None;
        if self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let cfg = self.get();
        if let Ok(raw) = serde_json::to_vec_pretty(&cfg) {
            // scrittura atomica: un crash a metà non lascia un config illeggibile
            let tmp = path().with_extension("json.tmp");
            if std::fs::write(&tmp, raw).is_ok() {
                let _ = std::fs::rename(&tmp, path());
            }
        }
        self.saving.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.max_connections, 8);
        assert_eq!(c.speed_limit_kbps, 0); // illimitato
        assert!(c.download_dir.as_os_str().is_empty()); // = Downloads di sistema
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // config scritto da una versione precedente, senza i campi nuovi
        let c: Config = serde_json::from_str(r#"{"max_connections":4}"#).unwrap();
        assert_eq!(c.max_connections, 4);
        assert_eq!(c.max_concurrent_downloads, 3);
        assert!(c.notify_on_complete);
    }

    #[test]
    fn clamp_tames_hand_edited_files() {
        let mut c: Config = serde_json::from_str(r#"{"max_connections":999,"max_concurrent_downloads":0}"#).unwrap();
        c.clamp();
        assert_eq!(c.max_connections, MAX_CONNECTIONS_CAP);
        assert_eq!(c.max_concurrent_downloads, 1);
    }

    #[test]
    fn edit_then_flush_roundtrips() {
        let s = Store::default();
        s.edit(|c| c.speed_limit_kbps = 1234);
        assert_eq!(s.get().speed_limit_kbps, 1234);
        // flush_due non scrive subito: il debounce non è ancora scaduto
        assert!(s.dirty.lock().unwrap().is_some());
        s.flush_due();
        assert!(s.dirty.lock().unwrap().is_some());
    }
}

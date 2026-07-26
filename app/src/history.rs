//! Cronologia dei download conclusi, in `%LOCALAPPDATA%\MDM\history.jsonl`.
//!
//! Una riga JSON per download: il formato append-only regge un crash a metà
//! scrittura (si perde al massimo l'ultima riga, che viene scartata in lettura)
//! e non richiede di rileggere tutto per aggiungere una voce.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Voci tenute in memoria e riscritte alla potatura.
const KEEP: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub url: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// secondi impiegati, 0 se ignoto
    pub secs: u64,
    /// epoch in secondi
    pub at: u64,
    pub ok: bool,
}

pub fn path() -> PathBuf {
    crate::engine::data_dir().join("history.jsonl")
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Aggiunge una voce. Non fallisce mai in modo rumoroso: la cronologia è
/// un extra, non deve poter far fallire un download andato a buon fine.
pub fn append(entry: &Entry) {
    use std::io::Write as _;
    let Ok(line) = serde_json::to_string(entry) else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = writeln!(f, "{line}");
    }
}

/// Legge le voci, dalla più recente. Le righe illeggibili vengono saltate.
pub fn load() -> Vec<Entry> {
    let Ok(raw) = std::fs::read_to_string(path()) else { return Vec::new() };
    let mut v: Vec<Entry> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    v.reverse();
    v.truncate(KEEP);
    v
}

/// Riscrive il file tenendo solo le ultime `KEEP` voci.
pub fn prune() {
    let entries = load();
    let mut out = String::new();
    for e in entries.iter().rev() {
        if let Ok(l) = serde_json::to_string(e) {
            out.push_str(&l);
            out.push('\n');
        }
    }
    let tmp = path().with_extension("jsonl.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path());
    }
}

pub fn clear() {
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            url: format!("https://x.it/{name}"),
            path: PathBuf::from(format!("/d/{name}")),
            bytes: 123,
            secs: 4,
            at: 1_700_000_000,
            ok: true,
        }
    }

    #[test]
    fn entry_roundtrips() {
        let e = entry("a.zip");
        let back: Entry = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.name, "a.zip");
        assert_eq!(back.bytes, 123);
        assert!(back.ok);
    }

    #[test]
    fn broken_lines_are_skipped_not_fatal() {
        // simula un crash a metà scrittura dell'ultima riga
        let raw = format!("{}\n{{\"name\":\"tron", serde_json::to_string(&entry("ok.zip")).unwrap());
        let parsed: Vec<Entry> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ok.zip");
    }
}

//! Listener TCP localhost: riceve job (una riga JSON per connessione) dall'host.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::engine::{self, AppState, Job};

pub const PORT: u16 = 48666;

pub fn start(rt: &tokio::runtime::Runtime, listener: std::net::TcpListener, state: Arc<AppState>) {
    rt.spawn(async move {
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let state = state.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(sock);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let reply = if value.get("url").is_some() {
                    match serde_json::from_value::<Job>(value) {
                        Ok(job) => {
                            tokio::spawn(engine::run_job(state, job));
                            r#"{"ok":true}"#.to_string()
                        }
                        Err(_) => r#"{"ok":false}"#.to_string(),
                    }
                } else if value.get("cmd").and_then(|c| c.as_str()) == Some("resume_all") {
                    let paused: Vec<_> = state
                        .downloads
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|d| {
                            matches!(*d.status.lock().unwrap(), engine::Status::Paused | engine::Status::Failed(_))
                        })
                        .cloned()
                        .collect();
                    for dl in paused {
                        tokio::spawn(engine::resume(state.clone(), dl));
                    }
                    r#"{"ok":true}"#.to_string()
                } else if value.get("cmd").and_then(|c| c.as_str()) == Some("config") {
                    // l'estensione chiede all'app la soglia: un solo posto dove
                    // configurarla invece di un numero hardcoded nel background.js
                    let cfg = state.config.get();
                    format!(r#"{{"ok":true,"sizeThresholdMb":{}}}"#, cfg.size_threshold_mb)
                } else {
                    r#"{"ok":false}"#.to_string()
                };
                let _ = reader.into_inner().write_all(format!("{reply}\n").as_bytes()).await;
            });
        }
    });
}

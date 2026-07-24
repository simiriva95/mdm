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
                            "{\"ok\":true}\n"
                        }
                        Err(_) => "{\"ok\":false}\n",
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
                    "{\"ok\":true}\n"
                } else {
                    "{\"ok\":false}\n"
                };
                let _ = reader.into_inner().write_all(reply.as_bytes()).await;
            });
        }
    });
}

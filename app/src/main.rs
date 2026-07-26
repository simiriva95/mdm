#![cfg_attr(windows, windows_subsystem = "windows")]

use mdm::{engine, host, server, ui, update};

use std::sync::Arc;

fn main() {
    // Chrome lancia l'host passando l'origin dell'estensione come argomento
    if std::env::args().any(|a| a.starts_with("chrome-extension://") || a == "--host") {
        host::run();
        return;
    }

    // singola istanza: se la porta è occupata, l'app gira già
    let listener = match std::net::TcpListener::bind(("127.0.0.1", server::PORT)) {
        Ok(l) => l,
        Err(_) => return,
    };

    let state = Arc::new(engine::AppState::default());
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    *state.rt.lock().unwrap() = Some(rt.handle().clone());
    if let Some(p) = engine::system_proxy() {
        state.log(format!("proxy di sistema: {p}"));
    }
    engine::scan_resumable(&state);
    server::start(&rt, listener, state.clone());
    rt.spawn(update::check_loop(state.clone()));

    let _ = ui::run(state);
}

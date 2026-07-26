//! Libreria interna: gli stessi moduli del binario, esposti perché i test di
//! integrazione (`tests/`) possano pilotare l'engine. Il binario è `main.rs`.

pub mod config;
pub mod engine;
pub mod history;
pub mod host;
pub mod limiter;
pub mod queue;
pub mod server;
pub mod ui;
pub mod update;

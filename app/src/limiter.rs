//! Limite di banda globale: token bucket condiviso da tutte le connessioni.
//!
//! Il limite si legge dalla config a ogni ricarica, quindi lo slider della UI
//! ha effetto immediato senza riavviare i download. A limite 0 (illimitato)
//! `acquire` esce subito: nessun costo quando la funzione non serve.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// Riserva di byte accumulabile: un burst breve non viene strozzato, ma la
/// media resta al limite. Mezzo secondo di traffico è un compromesso comodo.
const BURST_SECS: f64 = 0.5;

pub struct Limiter {
    /// KB/s consentiti; 0 = illimitato
    limit_kbps: AtomicU64,
    state: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Default for Limiter {
    fn default() -> Self {
        Self {
            limit_kbps: AtomicU64::new(0),
            state: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
        }
    }
}

impl Limiter {
    pub fn new(limit_kbps: u64) -> Arc<Self> {
        let l = Arc::new(Self::default());
        l.set_limit(limit_kbps);
        l
    }

    pub fn set_limit(&self, limit_kbps: u64) {
        self.limit_kbps.store(limit_kbps, Ordering::Relaxed);
    }

    pub fn limit_kbps(&self) -> u64 {
        self.limit_kbps.load(Ordering::Relaxed)
    }

    /// Prova a consumare `n` byte. Ritorna quanto aspettare prima di riprovare
    /// (`Duration::ZERO` = via libera). Separata da `acquire` per poterla
    /// testare senza dormire davvero.
    fn try_take(&self, n: u64) -> Duration {
        let rate = self.limit_kbps.load(Ordering::Relaxed) as f64 * 1024.0;
        if rate <= 0.0 {
            return Duration::ZERO;
        }
        let mut b = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * rate).min(rate * BURST_SECS);

        let want = n as f64;
        if b.tokens >= want {
            b.tokens -= want;
            return Duration::ZERO;
        }
        // niente token: aspetta il tempo che serve a maturarli
        Duration::from_secs_f64(((want - b.tokens) / rate).min(1.0))
    }

    /// Attende finché `n` byte non rientrano nel limite.
    pub async fn acquire(&self, n: u64) {
        loop {
            let wait = self.try_take(n);
            if wait.is_zero() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let l = Limiter::new(0);
        assert_eq!(l.try_take(10 * 1024 * 1024), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_then_throttle() {
        // 100 KB/s, burst di mezzo secondo = 50 KB subito disponibili al massimo
        let l = Limiter::new(100);
        // il bucket parte vuoto: la prima richiesta deve attendere
        assert!(l.try_take(50 * 1024) > Duration::ZERO);

        // dopo un secondo di ricarica ci sono 50KB (tetto del burst)
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(l.try_take(50 * 1024), Duration::ZERO, "il burst accumulato deve passare");
        // subito dopo il bucket è a secco
        assert!(l.try_take(50 * 1024) > Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn average_rate_matches_the_limit() {
        let l = Limiter::new(64); // 64 KB/s
        let start = Instant::now();
        // 256 KB in blocchi da 16 KB: a 64 KB/s servono ~4 secondi
        for _ in 0..16 {
            l.acquire(16 * 1024).await;
        }
        let elapsed = start.elapsed().as_secs_f64();
        assert!((3.0..5.5).contains(&elapsed), "durata fuori scala: {elapsed}s");
    }

    #[tokio::test(start_paused = true)]
    async fn raising_the_limit_takes_effect_immediately() {
        let l = Limiter::new(1);
        assert!(l.try_take(64 * 1024) > Duration::ZERO);
        l.set_limit(0); // illimitato dalla UI
        assert_eq!(l.try_take(64 * 1024), Duration::ZERO);
    }
}

//! Coda dei download: quanti ne partono davvero insieme.
//!
//! Prima ogni job intercettato partiva subito: 10 file dall'estensione
//! significavano 10 x 8 = 80 connessioni contemporanee, con l'effetto di
//! rallentare tutto e farsi rate-limitare. Qui i job entrano in ordine di
//! arrivo (ticket FIFO) e gli altri aspettano il loro turno.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Timeout di sicurezza sulla notify: copre la race tra `leave` e `wait`.
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub struct Queue {
    limit: AtomicU64,
    active: AtomicU64,
    /// prossimo ticket da assegnare
    next: AtomicU64,
    /// ticket più basso ancora in attesa: solo lui può entrare
    head: AtomicU64,
    notify: tokio::sync::Notify,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            limit: AtomicU64::new(3),
            active: AtomicU64::new(0),
            next: AtomicU64::new(0),
            head: AtomicU64::new(0),
            notify: tokio::sync::Notify::new(),
        }
    }
}

/// Permesso di scaricare: alla distruzione libera lo slot.
pub struct Slot<'a>(&'a Queue);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
}

impl Queue {
    pub fn set_limit(&self, n: u64) {
        let old = self.limit.swap(n.max(1), Ordering::SeqCst);
        if n > old {
            self.notify.notify_waiters(); // alzato dalla UI: sveglia chi aspetta
        }
    }

    pub fn ticket(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    /// Quanti ci sono davanti a questo ticket (0 = tocca a lui).
    pub fn position(&self, ticket: u64) -> u64 {
        ticket.saturating_sub(self.head.load(Ordering::SeqCst))
    }

    /// Rinuncia al posto senza scaricare (download annullato in coda):
    /// il ticket successivo diventa la testa, altrimenti la coda si blocca.
    pub fn give_up(&self, ticket: u64) {
        let _ = self.head.compare_exchange(ticket, ticket + 1, Ordering::SeqCst, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Attende il turno. `abort` viene interrogata mentre si aspetta: se
    /// diventa vera si molla il posto e si ritorna `None`.
    pub async fn enter(&self, ticket: u64, abort: impl Fn() -> bool) -> Option<Slot<'_>> {
        loop {
            if abort() {
                self.give_up(ticket);
                return None;
            }
            if self.head.load(Ordering::SeqCst) == ticket
                && self.active.load(Ordering::SeqCst) < self.limit.load(Ordering::SeqCst)
            {
                self.active.fetch_add(1, Ordering::SeqCst);
                self.head.store(ticket + 1, Ordering::SeqCst);
                self.notify.notify_waiters();
                return Some(Slot(self));
            }
            let _ = tokio::time::timeout(POLL, self.notify.notified()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn only_limit_run_at_once() {
        let q = Arc::new(Queue::default());
        q.set_limit(2);

        let t0 = q.ticket();
        let t1 = q.ticket();
        let t2 = q.ticket();

        let s0 = q.enter(t0, || false).await.unwrap();
        let s1 = q.enter(t1, || false).await.unwrap();
        assert_eq!(q.active.load(Ordering::SeqCst), 2);

        // il terzo resta fuori finché uno dei due non finisce
        let blocked = tokio::time::timeout(Duration::from_millis(120), q.enter(t2, || false)).await;
        assert!(blocked.is_err(), "col limite a 2 il terzo deve aspettare");
        assert_eq!(q.position(t2), 0, "è comunque il prossimo in fila");

        drop(s0);
        let s2 = tokio::time::timeout(Duration::from_millis(600), q.enter(t2, || false))
            .await
            .expect("liberato uno slot deve entrare")
            .unwrap();
        drop(s1);
        drop(s2);
        assert_eq!(q.active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn order_is_fifo() {
        let q = Arc::new(Queue::default());
        q.set_limit(1);
        let t0 = q.ticket();
        let t1 = q.ticket();
        let t2 = q.ticket();
        assert_eq!(q.position(t0), 0);
        assert_eq!(q.position(t2), 2);

        let s0 = q.enter(t0, || false).await.unwrap();
        // t2 non può scavalcare t1 nemmeno se lo slot si libera
        drop(s0);
        let jumped = tokio::time::timeout(Duration::from_millis(120), q.enter(t2, || false)).await;
        assert!(jumped.is_err(), "t2 non deve passare davanti a t1");

        let s1 = q.enter(t1, || false).await.unwrap();
        drop(s1);
        assert!(tokio::time::timeout(Duration::from_millis(600), q.enter(t2, || false)).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_while_queued_does_not_stall_the_queue() {
        let q = Arc::new(Queue::default());
        q.set_limit(1);
        let t0 = q.ticket();
        let t1 = q.ticket();
        let t2 = q.ticket();
        let s0 = q.enter(t0, || false).await.unwrap();

        // t1 viene annullato mentre aspetta: t2 non deve restare bloccato
        assert!(q.enter(t1, || true).await.is_none());
        drop(s0);
        assert!(tokio::time::timeout(Duration::from_millis(600), q.enter(t2, || false)).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn raising_the_limit_wakes_waiters() {
        let q = Arc::new(Queue::default());
        q.set_limit(1);
        let t0 = q.ticket();
        let t1 = q.ticket();
        let _s0 = q.enter(t0, || false).await.unwrap();

        let q2 = q.clone();
        let waiter = tokio::spawn(async move { q2.enter(t1, || false).await.is_some() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        q.set_limit(2); // slider della UI alzato mentre si aspetta
        assert!(tokio::time::timeout(Duration::from_millis(600), waiter).await.unwrap().unwrap());
    }
}

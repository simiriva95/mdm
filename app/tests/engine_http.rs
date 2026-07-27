//! Test di integrazione dell'engine contro un server HTTP finto in-process.
//!
//! Copre la parte che i test unitari non possono toccare: Range, 429, stream
//! troncato a metà, pausa/resume, ripristino da sidecar dopo un crash e file
//! cambiato sul server. Nessuna dipendenza extra: HTTP/1.1 scritto a mano.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mdm::engine::{self, AppState, Job, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- server finto ----

#[derive(Default)]
struct Behavior {
    /// risponde 206 alle richieste con Range (altrimenti sempre 200 pieno)
    ranges: bool,
    /// validator corrente; cambiandolo si simula il file che muta sul server
    etag: Mutex<Option<String>>,
    /// se presente, sostituisce `etag` finita la grazia: il file "cambia"
    /// subito dopo il probe, senza dover cronometrare un task esterno
    etag_after_grace: Mutex<Option<String>>,
    /// quante richieste servire normalmente prima di iniziare a rifiutare
    /// (1 = lascia passare il probe). Evita di dover pilotare il server da un
    /// task a parte, che era una corsa contro il download.
    grace: AtomicU64,
    /// quante delle prossime richieste rispondono 429
    rate_limit: AtomicU64,
    /// se il 429 deve portare un Retry-After
    retry_after: AtomicBool,
    /// quante delle prossime risposte vengono troncate a metà corpo
    truncate: AtomicU64,
    /// richieste servite (la prima è sempre il probe dell'engine)
    hits: AtomicU64,
    /// se > 0, ogni richiesta risponde con questo status invece del file
    reject_with: AtomicU64,
    /// connessioni TCP accettate: serve a verificare il riuso del pool
    conns: AtomicU64,
}

struct Server {
    url: String,
    body: Arc<Vec<u8>>,
}

/// Corpo deterministico e non comprimibile, così un byte fuori posto si vede.
fn make_body(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x12345678;
    for _ in 0..len {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        v.push((x >> 24) as u8);
    }
    v
}

async fn spawn_server(body: Arc<Vec<u8>>, behavior: Arc<Behavior>) -> Server {
    // `behavior` resta pilotabile dal test tramite la sua copia dell'Arc
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/file.bin");

    let b = body.clone();
    let bh = behavior.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let b = b.clone();
            let bh = bh.clone();
            bh.conns.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                // keep-alive: piu' richieste sulla stessa connessione, cosi' il
                // riuso del pool di reqwest e' osservabile
                let mut sock = sock;
                while let Ok(true) = serve(&mut sock, b.clone(), bh.clone()).await {}
            });
        }
    });

    Server { url, body }
}

/// Serve una richiesta. `Ok(true)` = la connessione resta aperta per la
/// prossima (keep-alive), `Ok(false)` = va chiusa.
async fn serve(sock: &mut tokio::net::TcpStream, body: Arc<Vec<u8>>, bh: Arc<Behavior>) -> std::io::Result<bool> {
    // leggi gli header fino a riga vuota
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = sock.read(&mut buf).await?;
        if n == 0 {
            return Ok(false);
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let req = String::from_utf8_lossy(&raw).to_string();
    let header = |name: &str| -> Option<String> {
        req.lines()
            .find(|l| l.to_ascii_lowercase().starts_with(&format!("{}:", name.to_ascii_lowercase())))
            .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
    };

    bh.hits.fetch_add(1, Ordering::SeqCst);

    // rifiuto secco (404, 403, ...) a comando
    let reject = bh.reject_with.load(Ordering::SeqCst);
    if reject > 0 {
        sock.write_all(format!("HTTP/1.1 {reject} Rejected\r\nContent-Length: 0\r\n\r\n").as_bytes()).await?;
        return Ok(true);
    }

    // 429 a comando, una volta esaurita la finestra di grazia
    let in_grace = bh.grace.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
    if !in_grace && bh.rate_limit.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
        let extra = if bh.retry_after.load(Ordering::SeqCst) { "Retry-After: 1\r\n" } else { "" };
        sock.write_all(format!("HTTP/1.1 429 Too Many Requests\r\n{extra}Content-Length: 0\r\n\r\n").as_bytes())
            .await?;
        return Ok(true);
    }

    let total = body.len();
    let etag = match (in_grace, bh.etag_after_grace.lock().unwrap().clone()) {
        (false, Some(next)) => Some(next),
        _ => bh.etag.lock().unwrap().clone(),
    };
    let if_range = header("If-Range");
    // If-Range che non combacia => il server serve il file intero (RFC 9110)
    let stale = if_range.is_some() && if_range != etag;

    let range = header("Range").and_then(|v| parse_range(&v, total));
    let etag_line = etag.map(|e| format!("ETag: {e}\r\n")).unwrap_or_default();

    let (head, slice) = match range {
        Some((s, e)) if bh.ranges && !stale => (
            format!(
                "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\n{etag_line}Content-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\n\r\n",
                e - s + 1
            ),
            &body[s..=e],
        ),
        _ => (
            format!(
                "HTTP/1.1 200 OK\r\n{}{etag_line}Content-Length: {total}\r\n\r\n",
                if bh.ranges { "Accept-Ranges: bytes\r\n" } else { "" }
            ),
            &body[..],
        ),
    };

    sock.write_all(head.as_bytes()).await?;
    // troncamento: chiude a metà corpo pur avendo annunciato Content-Length.
    // La connessione va poi chiusa davvero, altrimenti resterebbe disallineata.
    let cut = !in_grace && bh.truncate.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
    let out = if cut && slice.len() > 1 { &slice[..slice.len() / 2] } else { slice };
    sock.write_all(out).await?;
    sock.flush().await?;
    Ok(!cut)
}

/// "bytes=100-199" -> (100, 199), estremi inclusivi e clampati.
fn parse_range(v: &str, total: usize) -> Option<(usize, usize)> {
    let spec = v.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: usize = a.trim().parse().ok()?;
    let end: usize = b.trim().parse().unwrap_or(total - 1);
    if start >= total {
        return None;
    }
    Some((start, end.min(total - 1)))
}

// ---- helper ----

struct Fixture {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mdm-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::default());
        state.config.edit(|c| c.download_dir = dir.clone());
        Self { state, dir }
    }

    fn job(&self, url: &str) -> Job {
        Job { url: url.to_string(), ..Default::default() }
    }

    fn only_download(&self) -> Arc<engine::Download> {
        self.state.downloads.lock().unwrap()[0].clone()
    }

    fn status(&self) -> Status {
        self.only_download().status.lock().unwrap().clone()
    }

    fn output(&self) -> Vec<u8> {
        std::fs::read(self.only_download().path.lock().unwrap().clone()).expect("file finale assente")
    }

    /// Nessun residuo: né `.part` né sidecar restano dopo un download riuscito.
    fn assert_clean(&self) {
        for e in std::fs::read_dir(&self.dir).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            assert!(!n.ends_with(".part") && !n.ends_with(".mdm.json"), "residuo sul disco: {n}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn behavior(ranges: bool, etag: Option<&str>) -> Arc<Behavior> {
    Arc::new(Behavior { ranges, etag: Mutex::new(etag.map(str::to_string)), ..Default::default() })
}

/// 12 MB: sopra 2*MIN_SEGMENT, quindi l'engine sceglie il percorso segmentato.
const BIG: usize = 12 * 1024 * 1024;

// ---- test ----

#[tokio::test(flavor = "multi_thread")]
async fn segmented_download_is_byte_exact() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(true, Some("\"v1\""))).await;
    let fx = Fixture::new("seg");

    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    assert!(matches!(fx.status(), Status::Done), "stato: {:?}", fx.output().len());
    assert_eq!(fx.output(), *srv.body, "contenuto diverso dal sorgente");
    // il file è stato spezzato davvero, non scaricato in un colpo solo
    assert!(fx.only_download().segs.lock().unwrap().len() > 1);
    fx.assert_clean();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_range_server_falls_back_to_single_stream() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(false, None)).await;
    let fx = Fixture::new("nornd");

    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    assert!(matches!(fx.status(), Status::Done));
    assert_eq!(fx.output(), *srv.body);
    assert!(!fx.only_download().resumable.load(Ordering::Relaxed), "senza Range non è ripristinabile");
    fx.assert_clean();
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limiting_shrinks_concurrency_but_completes() {
    let body = Arc::new(make_body(BIG));
    let bh = behavior(true, Some("\"v1\""));
    let srv = spawn_server(body.clone(), bh.clone()).await;
    let fx = Fixture::new("429");

    // il probe passa (grazia 1), poi i segmenti sbattono contro il rate limit
    bh.retry_after.store(true, Ordering::SeqCst);
    bh.grace.store(1, Ordering::SeqCst);
    bh.rate_limit.store(6, Ordering::SeqCst);

    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    assert!(matches!(fx.status(), Status::Done));
    assert_eq!(fx.output(), *srv.body);
    assert!(fx.only_download().conc.load(Ordering::Relaxed) < 8, "l'AIMD doveva ridurre le connessioni");
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_response_is_retried_without_holes() {
    let body = Arc::new(make_body(BIG));
    let bh = behavior(true, Some("\"v1\""));
    let srv = spawn_server(body.clone(), bh.clone()).await;
    let fx = Fixture::new("trunc");

    // le prime risposte-segmento si chiudono a metà: senza il controllo di
    // troncamento (e senza il flush sull'errore di stream) il file finale
    // resterebbe bucato
    bh.grace.store(1, Ordering::SeqCst); // il probe passa intero
    bh.truncate.store(4, Ordering::SeqCst);

    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    assert!(matches!(fx.status(), Status::Done));
    assert_eq!(fx.output(), *srv.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_then_resume_produces_the_same_file() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(true, Some("\"v1\""))).await;
    let fx = Fixture::new("pause");

    let task = tokio::spawn(engine::run_job(fx.state.clone(), fx.job(&srv.url)));
    let dl = wait_for_progress(&fx.state, BIG as u64 / 8).await;
    dl.pause.store(true, Ordering::Relaxed);
    task.await.unwrap();
    assert!(matches!(*dl.status.lock().unwrap(), Status::Paused));

    let before = dl.done.load(Ordering::Relaxed);
    assert!(before > 0 && before < BIG as u64, "pausa a metà, non a 0 né a fine");

    engine::resume(fx.state.clone(), dl.clone()).await;

    assert!(matches!(*dl.status.lock().unwrap(), Status::Done));
    assert_eq!(fx.output(), *srv.body);
    fx.assert_clean();
}

#[tokio::test(flavor = "multi_thread")]
async fn sidecar_restores_download_after_a_crash() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(true, Some("\"v1\""))).await;
    let fx = Fixture::new("crash");

    // interrompi a metà: restano .part + sidecar sul disco
    let task = tokio::spawn(engine::run_job(fx.state.clone(), fx.job(&srv.url)));
    let dl = wait_for_progress(&fx.state, BIG as u64 / 8).await;
    dl.pause.store(true, Ordering::Relaxed);
    task.await.unwrap();
    drop(dl);

    // nuovo processo: stato vuoto, stessa cartella
    let fresh = Arc::new(AppState::default());
    let d = fx.dir.clone();
    fresh.config.edit(|c| c.download_dir = d);
    engine::scan_resumable(&fresh);

    let restored = fresh.downloads.lock().unwrap().first().cloned().expect("nulla ripristinato dal sidecar");
    assert!(matches!(*restored.status.lock().unwrap(), Status::Paused));
    assert!(restored.done.load(Ordering::Relaxed) > 0, "il progresso flushed doveva sopravvivere");

    engine::resume(fresh.clone(), restored.clone()).await;

    assert!(matches!(*restored.status.lock().unwrap(), Status::Done));
    let out = std::fs::read(restored.path.lock().unwrap().clone()).unwrap();
    assert_eq!(out, *srv.body, "il file ricomposto dopo il crash è diverso");
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changed_on_server_fails_instead_of_corrupting() {
    let body = Arc::new(make_body(BIG));
    let bh = behavior(true, Some("\"v1\""));
    let srv = spawn_server(body.clone(), bh.clone()).await;
    let fx = Fixture::new("etag");

    // subito dopo il probe il server passa a un'altra versione del file:
    // If-Range non combacia più e le richieste Range tornano 200
    bh.grace.store(1, Ordering::SeqCst);
    *bh.etag_after_grace.lock().unwrap() = Some("\"v2\"".into());

    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    let dl = fx.only_download();
    match &*dl.status.lock().unwrap() {
        Status::Failed(e) => assert!(e.contains("cambiato"), "messaggio poco chiaro: {e}"),
        other => panic!("doveva fallire, invece: {}", label(other)),
    }
    // il punto: nessun file "buono" consegnato all'utente
    assert!(!dl.path.lock().unwrap().exists(), "non deve esistere un file finale da byte misti");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_download_reuses_pooled_connections() {
    let body = Arc::new(make_body(BIG));
    let bh = behavior(true, Some("\"v1\""));
    let srv = spawn_server(body.clone(), bh.clone()).await;
    let fx = Fixture::new("pool");

    engine::run_job(fx.state.clone(), fx.job(&format!("{}?a=1", srv.url))).await;
    let after_first = bh.conns.load(Ordering::SeqCst);
    assert!(after_first > 0);

    engine::run_job(fx.state.clone(), fx.job(&format!("{}?a=2", srv.url))).await;
    let opened_by_second = bh.conns.load(Ordering::SeqCst) - after_first;

    let segments = fx.state.downloads.lock().unwrap()[1].segs.lock().unwrap().len() as u64;
    // Il punto: senza cache del client ogni download ricostruiva il pool da
    // zero, quindi il secondo riapriva almeno una connessione per segmento
    // (piu' il probe). Con il riuso ne apre meno.
    assert!(
        opened_by_second < segments + 1,
        "nessun riuso: il secondo download ha aperto {opened_by_second} connessioni per {segments} segmenti"
    );
    assert_eq!(fx.output(), *srv.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_caps_how_many_run_at_once() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(true, Some("\"v1\""))).await;
    let fx = Fixture::new("queue");
    // un solo download alla volta: gli altri devono restare in coda
    fx.state.config.edit(|c| c.max_concurrent_downloads = 1);
    fx.state.apply_config();

    let mut tasks = Vec::new();
    for i in 0..3 {
        let job = fx.job(&format!("{}?n={i}", srv.url));
        tasks.push(tokio::spawn(engine::run_job(fx.state.clone(), job)));
    }

    // mentre girano, non deve mai esserci più di un download attivo
    let mut saw_queued = false;
    for _ in 0..400 {
        let (active, queued) = {
            let dls = fx.state.downloads.lock().unwrap();
            let a = dls
                .iter()
                .filter(|d| matches!(*d.status.lock().unwrap(), Status::Active | Status::Connecting))
                .count();
            let q = dls.iter().filter(|d| matches!(*d.status.lock().unwrap(), Status::Queued)).count();
            (a, q)
        };
        assert!(active <= 1, "limite a 1 ma {active} download attivi insieme");
        saw_queued |= queued > 0;
        if tasks.iter().all(|t| t.is_finished()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for t in tasks {
        t.await.unwrap();
    }

    assert!(saw_queued, "con 3 job e limite 1 qualcuno doveva aspettare in coda");
    let dls = fx.state.downloads.lock().unwrap().clone();
    assert_eq!(dls.len(), 3);
    for d in &dls {
        assert!(matches!(*d.status.lock().unwrap(), Status::Done), "tutti devono comunque completare");
        assert_eq!(std::fs::read(d.path.lock().unwrap().clone()).unwrap(), *srv.body);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_all_respects_the_queue_limit() {
    let body = Arc::new(make_body(BIG));
    let srv = spawn_server(body.clone(), behavior(true, Some("\"v1\""))).await;
    let fx = Fixture::new("resumeq");
    fx.state.config.edit(|c| c.max_concurrent_downloads = 3);
    fx.state.apply_config();

    // tre download avviati e messi in pausa a metà
    let mut paused = Vec::new();
    for i in 0..3 {
        let job = fx.job(&format!("{}?r={i}", srv.url));
        let task = tokio::spawn(engine::run_job(fx.state.clone(), job));
        let dl = wait_for_nth(&fx.state, i, BIG as u64 / 16).await;
        dl.pause.store(true, Ordering::Relaxed);
        task.await.unwrap();
        paused.push(dl);
    }
    assert!(paused.iter().all(|d| matches!(*d.status.lock().unwrap(), Status::Paused)));

    // "riprendi tutti" con limite 1: devono ripartire uno alla volta
    fx.state.config.edit(|c| c.max_concurrent_downloads = 1);
    fx.state.apply_config();
    let tasks: Vec<_> =
        paused.iter().map(|d| tokio::spawn(engine::resume(fx.state.clone(), d.clone()))).collect();

    for _ in 0..600 {
        let active = fx
            .state
            .downloads
            .lock()
            .unwrap()
            .iter()
            .filter(|d| matches!(*d.status.lock().unwrap(), Status::Active | Status::Connecting))
            .count();
        assert!(active <= 1, "resume ha scavalcato la coda: {active} attivi con limite 1");
        if tasks.iter().all(|t| t.is_finished()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for t in tasks {
        t.await.unwrap();
    }

    for d in &paused {
        assert!(matches!(*d.status.lock().unwrap(), Status::Done));
        assert_eq!(std::fs::read(d.path.lock().unwrap().clone()).unwrap(), *srv.body);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_link_fails_once_without_retrying() {
    let body = Arc::new(make_body(1024));
    let bh = behavior(true, None);
    bh.reject_with.store(404, Ordering::SeqCst);
    let srv = spawn_server(body, bh.clone()).await;
    let fx = Fixture::new("404");
    fx.state.config.edit(|c| c.auto_retry = 3);

    let start = std::time::Instant::now();
    engine::run_job(fx.state.clone(), fx.job(&srv.url)).await;

    let dl = fx.only_download();
    match &*dl.status.lock().unwrap() {
        Status::Failed(e) => assert!(e.contains("404"), "messaggio senza lo status: {e}"),
        other => panic!("doveva fallire, invece: {}", label(other)),
    }
    // il punto: un link morto non deve consumare i retry (5s + 15s + 45s)
    assert_eq!(dl.retries.load(Ordering::Relaxed), 0, "un 404 non va ritentato");
    assert!(start.elapsed() < std::time::Duration::from_secs(3), "ha aspettato un backoff inutile");
    assert!(bh.hits.load(Ordering::SeqCst) <= 3, "troppe richieste per un link morto");
    // fallendo prima di avere un nome non deve sporcare la cartella di lavoro
    assert!(!std::path::Path::new(".part.mdm.json").exists(), "sidecar orfano nella cwd");
}

async fn wait_for_progress(state: &Arc<AppState>, bytes: u64) -> Arc<engine::Download> {
    wait_for_nth(state, 0, bytes).await
}

/// Aspetta che il download in posizione `n` abbia scaricato almeno `bytes`.
async fn wait_for_nth(state: &Arc<AppState>, n: usize, bytes: u64) -> Arc<engine::Download> {
    for _ in 0..2000 {
        if let Some(dl) = state.downloads.lock().unwrap().get(n).cloned() {
            if dl.done.load(Ordering::Relaxed) >= bytes {
                return dl;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("il download {n} non ha raggiunto {bytes} byte in tempo");
}

fn label(s: &Status) -> String {
    match s {
        Status::Queued => "queued".into(),
        Status::Connecting => "connecting".into(),
        Status::Active => "active".into(),
        Status::Paused => "paused".into(),
        Status::Done => "done".into(),
        Status::Failed(e) => format!("failed({e})"),
        Status::Cancelled => "cancelled".into(),
    }
}

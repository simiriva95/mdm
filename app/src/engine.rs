//! Engine di download: segmentato (HTTP Range, N connessioni) o stream singolo,
//! con pausa/resume e ripristino dopo crash/riavvio via sidecar `.mdm.json`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use futures_util::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderValue, CONTENT_DISPOSITION, ETAG, IF_RANGE, LAST_MODIFIED, RANGE, REFERER,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// UA per i job senza uno proprio (URL incollato a mano). Quelli che arrivano
/// dall'estensione portano il UA vero di Chrome. Era fisso a "mdm/0.2", che
/// non corrispondeva più alla versione da un pezzo.
const DEFAULT_UA: &str = concat!("mdm/", env!("CARGO_PKG_VERSION"));

/// Tetto assoluto delle connessioni: la config non può superarlo.
pub const MAX_CONNECTIONS: u64 = crate::config::MAX_CONNECTIONS_CAP;
const MAX_SEGMENTS: u64 = MAX_CONNECTIONS;
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;
const SEGMENT_RETRIES: u32 = 8; // consecutivi senza alcun progresso
const SIDECAR_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
// stallo: se il server non manda byte per questo tempo la connessione viene rifatta
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// tempo massimo per ottenere gli header di risposta
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// work stealing: si dimezza un segmento solo se restano almeno 2*STEAL_MIN byte
const STEAL_MIN: u64 = 2 * 1024 * 1024;
// AIMD in salita: dopo questo periodo senza 429 si prova ad aggiungere 1 connessione
const RAMP_QUIET: std::time::Duration = std::time::Duration::from_secs(45);
// ogni quanti byte bufferizzati si forza un flush: il sidecar può così avanzare
// anche a metà segmento, senza aspettare la fine dello stream
const FLUSH_EVERY: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Job {
    pub url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub referrer: String,
    #[serde(default)]
    pub cookies: String,
    #[serde(default, rename = "userAgent")]
    pub user_agent: String,
}

#[derive(Clone, PartialEq)]
pub enum Status {
    /// in attesa di uno slot: ci sono già `max_concurrent_downloads` attivi
    Queued,
    Connecting,
    Active,
    Paused,
    Done,
    Failed(String),
    Cancelled,
}

pub struct Seg {
    pub start: u64,
    // end mobile: il work stealing può restringerlo mentre il segmento scarica
    pub end: AtomicU64,
    pub done: AtomicU64,
    /// byte usciti dal BufWriter: solo questi sopravvivono a un crash del
    /// processo, quindi è questo (non `done`) che finisce nel sidecar.
    pub flushed: AtomicU64,
}

impl Seg {
    fn new(start: u64, end: u64, done: u64) -> Arc<Self> {
        Arc::new(Self {
            start,
            end: AtomicU64::new(end),
            done: AtomicU64::new(done),
            flushed: AtomicU64::new(done),
        })
    }

    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Relaxed)
    }

    /// Da chiamare dopo ogni `flush()`: allinea l'offset sicuro a quello scritto.
    fn mark_flushed(&self) {
        self.flushed.store(self.done.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    pub fn len(&self) -> u64 {
        self.end().saturating_sub(self.start) + 1
    }

    fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.done.load(Ordering::Relaxed))
    }
}

/// Work stealing: prende il segmento col più lavoro residuo, lo dimezza e
/// ritorna la metà alta come nuovo segmento. None se non c'è nulla da rubare.
fn steal_segment(segs: &mut Vec<Arc<Seg>>) -> Option<Arc<Seg>> {
    let victim = segs
        .iter()
        .filter(|s| s.remaining() >= STEAL_MIN * 2)
        .max_by_key(|s| s.remaining())?
        .clone();
    let end = victim.end();
    let steal = victim.remaining() / 2;
    let new_end = end - steal;
    // prima si restringe la vittima, poi nasce il ladro: mai due proprietari
    victim.end.store(new_end, Ordering::Relaxed);
    let thief = Seg::new(new_end + 1, end, 0);
    segs.push(thief.clone());
    Some(thief)
}

pub struct Download {
    pub id: u64,
    pub name: Mutex<String>,
    pub path: Mutex<PathBuf>,
    pub total: AtomicU64, // 0 = ignoto
    pub done: AtomicU64,
    pub segs: Mutex<Vec<Arc<Seg>>>,
    pub conc: AtomicU64, // connessioni parallele consentite ora (AIMD)
    pub resumable: AtomicBool, // segmentato con Range: riprende dal punto esatto
    pub status: Mutex<Status>,
    pub cancel: AtomicBool,
    pub pause: AtomicBool,
    /// ETag o Last-Modified del file remoto: va in `If-Range` per accorgersi
    /// se il file cambia sotto di noi (CDN con varianti, resume dopo giorni).
    pub validator: Mutex<Option<String>>,
    /// errore terminale: tutti i worker si fermano e non si ritenta
    pub fatal: AtomicBool,
    pub fatal_msg: Mutex<String>,
    /// tentativi automatici già consumati
    pub retries: AtomicU64,
    /// posizione in coda mostrata dalla UI (0 = il prossimo)
    pub queue_pos: AtomicU64,
    /// inizio del download, per la durata in cronologia
    pub started: std::time::Instant,
    /// limite di banda condiviso con tutti gli altri download
    pub limiter: Arc<crate::limiter::Limiter>,
    pub job: Mutex<Job>,
}

impl Download {
    /// Segnala un errore che non ha senso ritentare: ferma tutti i worker.
    fn set_fatal(&self, msg: impl Into<String>) {
        *self.fatal_msg.lock().unwrap() = msg.into();
        self.fatal.store(true, Ordering::Relaxed);
    }

    /// Il worker deve mollare tutto? (pausa, abort o errore terminale)
    fn interrupted(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.pause.load(Ordering::Relaxed)
            || self.fatal.load(Ordering::Relaxed)
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    job: Job,
    total: u64,
    resumable: bool,
    #[serde(default)]
    validator: Option<String>,
    segments: Vec<SegSave>,
}

#[derive(Serialize, Deserialize)]
struct SegSave {
    start: u64,
    end: u64,
    /// offset *flushed*, non `done`: è il punto da cui si riparte davvero
    done: u64,
}

#[derive(Default)]
pub struct AppState {
    pub downloads: Mutex<Vec<Arc<Download>>>,
    pub log: Mutex<Vec<String>>,
    pub show_window: AtomicBool,
    pub quit: AtomicBool,
    /// nuova release trovata su GitHub (None = aggiornati)
    pub update: Mutex<Option<crate::update::UpdateInfo>>,
    /// download+installazione update in corso
    pub updating: AtomicBool,
    /// HWND della finestra principale: serve per riaprirla quando egui è
    /// congelato (finestra nascosta = niente WM_PAINT = niente update()).
    pub hwnd: std::sync::atomic::AtomicIsize,
    pub egui_ctx: Mutex<Option<eframe::egui::Context>>,
    pub rt: Mutex<Option<tokio::runtime::Handle>>,
    /// client HTTP per user-agent: riusa pool di connessioni e sessioni TLS
    pub clients: Mutex<std::collections::HashMap<String, reqwest::Client>>,
    /// impostazioni persistite: cartella, connessioni, banda, memoria per host
    pub config: Arc<crate::config::Store>,
    /// limite di banda globale (0 = illimitato)
    pub limiter: Arc<crate::limiter::Limiter>,
    /// quanti download possono girare insieme; gli altri aspettano il turno
    pub queue: crate::queue::Queue,
    next_id: AtomicU64,
}

/// Cartella dei dati dell'app: `%LOCALAPPDATA%\MDM` (log, config, storico).
pub fn data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("MDM");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn log_path() -> PathBuf {
    data_dir().join("mdm.log")
}

const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Append su file con rotazione singola. Con `windows_subsystem = "windows"`
/// stderr non esiste e il log in RAM muore col processo: senza questo, dei bug
/// rari non resta traccia.
///
/// L'handle resta aperto: riaprire il file a ogni riga significava due syscall
/// in più per riga, su thread del runtime tokio.
fn append_log_file(line: &str) {
    use std::io::Write as _;
    static FILE: std::sync::OnceLock<Mutex<Option<(std::fs::File, u64)>>> = std::sync::OnceLock::new();
    let slot = FILE.get_or_init(|| Mutex::new(None));
    let Ok(mut guard) = slot.lock() else { return };

    if guard.is_none() {
        let path = log_path();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
        *guard = Some((f, size));
    }
    let Some((file, written)) = guard.as_mut() else { return };

    let text = format!("{} {line}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    if file.write_all(text.as_bytes()).is_err() {
        *guard = None; // handle andato: si riapre al prossimo giro
        return;
    }
    *written += text.len() as u64;

    if *written > LOG_MAX_BYTES {
        let path = log_path();
        *guard = None; // chiudi prima di rinominare
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
}

impl AppState {
    /// Stato con una config già caricata (il binario); `default()` ne usa una
    /// in memoria, così i test non leggono né sporcano quella dell'utente.
    pub fn with_config(config: Arc<crate::config::Store>) -> Self {
        let state = Self { config, ..Default::default() };
        state.apply_config();
        state
    }

    /// Propaga i valori della config agli oggetti runtime. Da richiamare
    /// quando la config cambia, così banda e coda reagiscono agli slider
    /// senza riavviare i download.
    pub fn apply_config(&self) {
        let cfg = self.config.get();
        self.limiter.set_limit(cfg.speed_limit_kbps);
        self.queue.set_limit(cfg.max_concurrent_downloads);
    }

    /// Dove finiscono i file. Override da config, altrimenti Downloads di sistema.
    pub fn dl_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.config.get().download_dir;
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&dir).with_context(|| format!("creazione di {}", dir.display()))?;
            return Ok(dir);
        }
        dirs::download_dir().context("cartella Downloads non trovata")
    }

    /// Connessioni parallele massime per download.
    pub fn max_conc(&self) -> u64 {
        self.config.get().max_connections
    }

    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        eprintln!("[mdm] {line}");
        append_log_file(&line);
        let mut log = self.log.lock().unwrap();
        log.push(format!("> {line}"));
        let len = log.len();
        if len > 200 {
            log.drain(..len - 200);
        }
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.egui_ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    /// Mostra la finestra anche se egui è congelato (nascosta in tray).
    /// ShowWindow via Win32 forza il WM_PAINT che risveglia il loop.
    pub fn wake_and_show(&self) {
        self.show_window.store(true, Ordering::Relaxed);
        #[cfg(windows)]
        {
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
                };
                unsafe {
                    if IsIconic(hwnd as _) != 0 {
                        ShowWindow(hwnd as _, SW_RESTORE);
                    } else {
                        ShowWindow(hwnd as _, SW_SHOW);
                    }
                    SetForegroundWindow(hwnd as _);
                }
            }
        }
        self.repaint();
    }

    fn new_download(&self, job: Job) -> Arc<Download> {
        Arc::new(Download {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            name: Mutex::new(if job.filename.is_empty() { job.url.clone() } else { job.filename.clone() }),
            path: Mutex::new(PathBuf::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            segs: Mutex::new(Vec::new()),
            conc: AtomicU64::new(self.max_conc()),
            resumable: AtomicBool::new(false),
            status: Mutex::new(Status::Connecting),
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            validator: Mutex::new(None),
            fatal: AtomicBool::new(false),
            fatal_msg: Mutex::new(String::new()),
            retries: AtomicU64::new(0),
            queue_pos: AtomicU64::new(0),
            started: std::time::Instant::now(),
            limiter: self.limiter.clone(),
            job: Mutex::new(job),
        })
    }
}

pub async fn run_job(state: Arc<AppState>, job: Job) {
    // stesso URL già in coda/attivo: niente doppioni dall'estensione
    {
        let dup = state.downloads.lock().unwrap().iter().any(|d| {
            d.job.lock().unwrap().url == job.url
                && matches!(
                    *d.status.lock().unwrap(),
                    Status::Active | Status::Connecting | Status::Paused | Status::Queued
                )
        });
        if dup {
            state.log(format!("ignorato (già in lista): {}", job.url));
            state.wake_and_show();
            return;
        }
    }
    let dl = state.new_download(job.clone());
    *dl.status.lock().unwrap() = Status::Queued;
    state.downloads.lock().unwrap().push(dl.clone());
    state.log(format!("nuovo job: {}", job.url));
    state.wake_and_show();

    {
        let Some(_slot) = await_turn(&state, &dl).await else { return };
        let result = download(&state, &dl, &job, true).await;
        finish(&state, &dl, result);
    }
    // slot rilasciato prima dei retry: aspettare 45s di backoff tenendo
    // occupato un posto in coda bloccherebbe gli altri download per niente
    // (ed essendo `resume` a riprenderlo, con limite 1 sarebbe un deadlock)
    retry_loop(&state, &dl).await;
}

/// Mette il download in coda e aspetta il suo turno, aggiornando la posizione
/// mostrata. `None` = annullato mentre aspettava.
///
/// Ci passano sia i job nuovi sia i resume: senza, "riprendi tutti" farebbe
/// ripartire tutto insieme scavalcando il limite di download simultanei.
async fn await_turn<'a>(state: &'a Arc<AppState>, dl: &Arc<Download>) -> Option<crate::queue::Slot<'a>> {
    *dl.status.lock().unwrap() = Status::Queued;
    let ticket = state.queue.ticket();
    let pos = state.queue.position(ticket);
    dl.queue_pos.store(pos, Ordering::Relaxed);
    if pos > 0 {
        state.log(format!("in coda ({pos} davanti): {}", dl.name.lock().unwrap()));
    }
    state.repaint();

    // tiene viva la posizione mostrata mentre si aspetta
    let ticker = {
        let dl = dl.clone();
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                dl.queue_pos.store(state.queue.position(ticket), Ordering::Relaxed);
                state.repaint();
            }
        })
    };
    let slot = state.queue.enter(ticket, || dl.cancel.load(Ordering::Relaxed)).await;
    ticker.abort();
    dl.queue_pos.store(0, Ordering::Relaxed);

    if slot.is_none() {
        *dl.status.lock().unwrap() = Status::Cancelled;
        state.log(format!("annullato in coda: {}", dl.name.lock().unwrap()));
        state.repaint();
        return None;
    }
    *dl.status.lock().unwrap() = Status::Connecting;
    state.repaint();
    slot
}

/// Ritenta da solo un download fallito, con backoff crescente. Gli errori
/// terminali (4xx, file cambiato sul server) non passano di qui: ritentarli
/// sarebbe solo rumore.
async fn retry_loop(state: &Arc<AppState>, dl: &Arc<Download>) {
    const BACKOFF: [u64; 3] = [5, 15, 45];
    loop {
        let max = state.config.get().auto_retry;
        let n = dl.retries.load(Ordering::Relaxed);
        let failed = matches!(*dl.status.lock().unwrap(), Status::Failed(_));
        if !failed || n >= max || dl.fatal.load(Ordering::Relaxed) || dl.cancel.load(Ordering::Relaxed) {
            return;
        }
        dl.retries.store(n + 1, Ordering::Relaxed);
        let wait = BACKOFF[(n as usize).min(BACKOFF.len() - 1)];
        state.log(format!("ritento tra {wait}s ({}/{max}): {}", n + 1, dl.name.lock().unwrap()));
        state.repaint();
        nap(dl, wait).await;
        if dl.cancel.load(Ordering::Relaxed) || dl.pause.load(Ordering::Relaxed) {
            return;
        }
        resume(state.clone(), dl.clone()).await;
    }
}

/// Riprende un download in pausa/fallito/ripristinato da sidecar.
pub async fn resume(state: Arc<AppState>, dl: Arc<Download>) {
    {
        let mut status = dl.status.lock().unwrap();
        if !matches!(*status, Status::Paused | Status::Failed(_)) {
            return;
        }
        *status = Status::Connecting;
    }
    dl.pause.store(false, Ordering::Relaxed);
    dl.cancel.store(false, Ordering::Relaxed);
    dl.fatal.store(false, Ordering::Relaxed);
    state.log(format!("riprendo: {}", dl.name.lock().unwrap()));
    state.repaint();

    // anche i resume passano dalla coda: "riprendi tutti" su 20 download in
    // pausa non deve farne ripartire 20 insieme
    let Some(_slot) = await_turn(&state, &dl).await else { return };

    let job = dl.job.lock().unwrap().clone();
    let part = part_path(&dl.path.lock().unwrap());

    let result = if dl.resumable.load(Ordering::Relaxed) && dl.total.load(Ordering::Relaxed) > 0 && part.exists() {
        // riparte dal punto esatto: solo i byte flushed sono davvero su disco,
        // quelli rimasti nel buffer di un worker interrotto vanno riscaricati
        let sum: u64 = {
            let segs = dl.segs.lock().unwrap();
            for s in segs.iter() {
                s.done.store(s.flushed.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            segs.iter().map(|s| s.done.load(Ordering::Relaxed)).sum()
        };
        dl.done.store(sum, Ordering::Relaxed);
        *dl.status.lock().unwrap() = Status::Active;
        state.repaint();
        let (client, headers) = client_for(&state, &job);
        let r =
            run_segments(&client, &job.url, &headers, &part, &dl, remembered_conc(&state, &job.url), state.max_conc())
                .await;
        remember_conc(&state, &job.url, &dl);
        match r {
            Ok(()) => finalize(&dl, &part).await,
            Err(e) => Err(e),
        }
    } else {
        // niente Range o .part perso: riparte da zero sullo stesso file
        download(&state, &dl, &job, false).await
    };
    finish(&state, &dl, result);
}

fn finish(state: &AppState, dl: &Arc<Download>, result: anyhow::Result<PathBuf>) {
    let target = dl.path.lock().unwrap().clone();
    // fallito prima di scegliere un nome (404 sul probe): non c'è nessun .part
    // da salvare, e scriverlo creerebbe un ".part.mdm.json" nella cwd
    let has_file = !target.as_os_str().is_empty();
    let part = part_path(&target);
    let ok = result.is_ok();
    let mut status = dl.status.lock().unwrap();
    match result {
        Ok(path) => {
            *status = Status::Done;
            state.log(format!("completato: {}", path.display()));
        }
        Err(_) if dl.pause.load(Ordering::Relaxed) => {
            *status = Status::Paused;
            if has_file {
                let _ = write_sidecar(dl, &part);
            }
            state.log(format!("in pausa: {}", dl.name.lock().unwrap()));
        }
        Err(_) if dl.cancel.load(Ordering::Relaxed) => {
            *status = Status::Cancelled;
            if has_file {
                let _ = std::fs::remove_file(&part);
                let _ = std::fs::remove_file(sidecar_path(&part));
            }
            state.log(format!("annullato: {}", dl.name.lock().unwrap()));
        }
        Err(e) => {
            // un errore terminale (file cambiato sul server) arriva per primo
            // sul flag, mentre `e` può essere l'"interrotto" di un altro worker
            let msg = if dl.fatal.load(Ordering::Relaxed) {
                dl.fatal_msg.lock().unwrap().clone()
            } else {
                format!("{e:#}")
            };
            // file e sidecar restano: [resume] ritenta da dove era arrivato
            *status = Status::Failed(msg.clone());
            if has_file {
                let _ = write_sidecar(dl, &part);
            }
            state.log(format!("ERRORE: {msg} — url: {}", dl.job.lock().unwrap().url));
        }
    }
    let terminal = !matches!(*status, Status::Paused);
    // notifica solo sugli esiti che l'utente vuole sapere: un annullamento
    // l'ha chiesto lui, una pausa non è la fine di niente
    let notify = match &*status {
        Status::Done => Some(String::new()),
        Status::Failed(e) if dl.fatal.load(Ordering::Relaxed) => Some(e.clone()),
        _ => None,
    };
    drop(status);

    if let Some(detail) = notify {
        if state.config.get().notify_on_complete {
            crate::notify::finished(&dl.name.lock().unwrap(), ok, &detail);
        }
    }

    // cronologia: solo esiti definitivi, una pausa non è la fine di niente
    if terminal {
        crate::history::append(&crate::history::Entry {
            name: dl.name.lock().unwrap().clone(),
            url: dl.job.lock().unwrap().url.clone(),
            path: dl.path.lock().unwrap().clone(),
            bytes: dl.done.load(Ordering::Relaxed),
            secs: dl.started.elapsed().as_secs(),
            at: crate::history::now_epoch(),
            ok,
        });
    }
    state.repaint();
}

/// Elimina .part e sidecar di un download non attivo (abort da UI).
pub fn discard(dl: &Download) {
    let part = part_path(&dl.path.lock().unwrap());
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_file(sidecar_path(&part));
    *dl.status.lock().unwrap() = Status::Cancelled;
}

/// All'avvio: cerca sidecar in Downloads e ricrea i download in pausa.
pub fn scan_resumable(state: &Arc<AppState>) {
    let Ok(dir) = state.dl_dir() else { return };
    sweep_orphans(state, &dir);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for entry in rd.flatten() {
        let sc_path = entry.path();
        let name = sc_path.to_string_lossy().to_string();
        let Some(part_str) = name.strip_suffix(".mdm.json") else { continue };
        if !part_str.ends_with(".part") {
            continue;
        }
        let part = PathBuf::from(part_str);
        if !part.exists() {
            let _ = std::fs::remove_file(&sc_path);
            continue;
        }
        let Ok(raw) = std::fs::read(&sc_path) else { continue };
        let Ok(sc) = serde_json::from_slice::<Sidecar>(&raw) else { continue };

        let path = PathBuf::from(part_str.strip_suffix(".part").unwrap());
        let dl = state.new_download(sc.job);
        *dl.name.lock().unwrap() = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        *dl.path.lock().unwrap() = path;
        dl.total.store(sc.total, Ordering::Relaxed);
        dl.resumable.store(sc.resumable, Ordering::Relaxed);
        *dl.validator.lock().unwrap() = sc.validator;
        // `done` nel sidecar è già l'offset flushed: nessun rewind da fare
        let segs: Vec<Arc<Seg>> =
            sc.segments.iter().map(|s| Seg::new(s.start, s.end, if sc.resumable { s.done } else { 0 })).collect();
        dl.done.store(segs.iter().map(|s| s.done.load(Ordering::Relaxed)).sum(), Ordering::Relaxed);
        *dl.segs.lock().unwrap() = segs;
        *dl.status.lock().unwrap() = Status::Paused;
        state.log(format!("ripristinato dal disco: {} (riprendi quando vuoi)", dl.name.lock().unwrap()));
        state.downloads.lock().unwrap().push(dl);
    }
}

/// `.part` senza sidecar: nessuno saprà mai riprenderli.
///
/// Quelli vuoti (nome riservato da `reserve_path` e download morto subito
/// dopo) si cancellano: sono spazzatura certa. Quelli con dei byte dentro si
/// segnalano soltanto — cancellare roba dell'utente non è compito nostro.
fn sweep_orphans(state: &AppState, dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut stale = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "part") || sidecar_path(&path).exists() {
            continue;
        }
        match entry.metadata().map(|m| m.len()) {
            Ok(0) => {
                let _ = std::fs::remove_file(&path);
            }
            Ok(_) => stale += 1,
            Err(_) => {}
        }
    }
    if stale > 0 {
        state.log(format!("{stale} file .part senza stato in Downloads: non riprendibili, puoi cancellarli"));
    }
}

async fn download(state: &AppState, dl: &Arc<Download>, job: &Job, fresh: bool) -> anyhow::Result<PathBuf> {
    let (client, headers) = client_for(state, job);

    // probe: GET con Range 0-0. 206 => range supportati + totale da Content-Range;
    // 200 => niente range, ma la risposta è il file intero e la riusiamo come stream.
    let mut probe = tokio::time::timeout(
        SEND_TIMEOUT,
        client.get(&job.url).headers(headers.clone()).header(RANGE, "bytes=0-0").send(),
    )
    .await
    .context("il server non risponde (timeout)")?
    .context("connessione fallita")?;
    for attempt in 1..=2u64 {
        if probe.status().as_u16() != 429 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2500 * attempt)).await;
        probe = tokio::time::timeout(
            SEND_TIMEOUT,
            client.get(&job.url).headers(headers.clone()).header(RANGE, "bytes=0-0").send(),
        )
        .await
        .context("il server non risponde (timeout)")?
        .context("connessione fallita")?;
    }
    if !probe.status().is_success() {
        let st = probe.status();
        // 404/403/410...: il link è morto o richiede altro. Ritentare da soli
        // non serve a niente, meglio dirlo subito e fermarsi.
        if st.is_client_error() && st != reqwest::StatusCode::REQUEST_TIMEOUT && st.as_u16() != 429 {
            dl.set_fatal(format!("il server rifiuta il download (status {st})"));
        }
        bail!("status {st}");
    }

    let disp_name = probe
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_disposition);
    let http_version = probe.version();
    let ranges = probe.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let len: Option<u64> = if ranges {
        probe
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_total)
    } else {
        probe
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .filter(|l| *l > 0)
    };

    // ETag/Last-Modified: identità del file remoto, usata poi in If-Range
    let validator = probe
        .headers()
        .get(ETAG)
        .or_else(|| probe.headers().get(LAST_MODIFIED))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    if validator.is_none() {
        state.log("il server non manda ETag/Last-Modified: resume non verificabile");
    }
    *dl.validator.lock().unwrap() = validator;

    let path = if fresh {
        let raw_name = if !job.filename.is_empty() {
            job.filename.clone()
        } else if let Some(n) = disp_name {
            n
        } else {
            filename_from_url(&job.url)
        };
        let dir = state.dl_dir()?;
        reserve_path(&dir.join(sanitize_filename(&raw_name)))?
    } else {
        dl.path.lock().unwrap().clone() // resume da zero: stesso file
    };
    let part = part_path(&path);

    *dl.name.lock().unwrap() = path.file_name().unwrap().to_string_lossy().into_owned();
    *dl.path.lock().unwrap() = path.clone();
    dl.total.store(len.unwrap_or(0), Ordering::Relaxed);
    dl.done.store(0, Ordering::Relaxed);
    *dl.status.lock().unwrap() = Status::Active;
    state.repaint();

    // Basta che il server supporti Range: anche un file piccolo passa dal
    // percorso segmentato (con un solo segmento), così ha sidecar e resume
    // esatto. Prima ripartiva da zero a ogni intoppo.
    let segmented = ranges && len.is_some();
    dl.resumable.store(segmented, Ordering::Relaxed);

    if segmented {
        let len = len.unwrap();
        drop(probe);
        let max_conc = state.max_conc();
        let segs: Vec<Arc<Seg>> = split_segments(len, max_conc, MIN_SEGMENT)
            .into_iter()
            .map(|(start, end)| Seg::new(start, end, 0))
            .collect();
        // se questo host ha già protestato (429), riparti prudente
        let start_conc = remembered_conc(state, &job.url);
        state.log(format!("protocollo negoziato: {:?}", http_version));
        if segs.len() == 1 {
            state.log(format!("1 connessione ({}), resume disponibile", crate::ui::fmt_bytes(len)));
        } else if start_conc < max_conc {
            state.log(format!("{} segmenti, parto con {start_conc} connessioni (host già rate-limitato)", segs.len()));
        } else {
            state.log(format!("{} segmenti paralleli, {}", segs.len(), crate::ui::fmt_bytes(len)));
        }
        *dl.segs.lock().unwrap() = segs;
        let f = tokio::fs::File::create(&part).await?;
        f.set_len(len).await?; // pre-alloca
        drop(f);
        let r = run_segments(&client, &job.url, &headers, &part, dl, start_conc, max_conc).await;
        remember_conc(state, &job.url, dl);
        r?;
    } else {
        state.log(if ranges {
            "stream singolo (file piccolo)".to_string()
        } else {
            "stream singolo (il server non supporta Range)".to_string()
        });
        *dl.segs.lock().unwrap() = vec![Seg::new(0, len.unwrap_or(1).saturating_sub(1), 0)];
        let _ = write_sidecar(dl, &part);
        // se il probe era un 200 pieno riusiamo quella risposta, altrimenti nuova GET
        let reuse = if ranges { None } else { Some(probe) };
        single_stream(&client, &job.url, &headers, &part, dl, reuse).await?;
    }
    finalize(dl, &part).await
}

async fn finalize(dl: &Download, part: &Path) -> anyhow::Result<PathBuf> {
    let mut path = dl.path.lock().unwrap().clone();
    // qualcuno ha creato quel file mentre scaricavamo: su Windows `rename` usa
    // MOVEFILE_REPLACE_EXISTING e lo cancellerebbe senza dire niente
    if path.exists() {
        path = free_name(&path);
        *dl.path.lock().unwrap() = path.clone();
        *dl.name.lock().unwrap() = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    }
    tokio::fs::rename(part, &path).await?;
    let _ = std::fs::remove_file(sidecar_path(part));
    Ok(path)
}

/// Proxy di sistema Windows (WinINET): Chrome lo usa, quindi anche noi —
/// su reti aziendali il traffico diretto viene spesso strozzato o bloccato.
#[cfg(windows)]
pub fn system_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    if server.is_empty() {
        return None;
    }
    // formato "host:port" oppure "http=h:p;https=h:p;ftp=..."
    let pick = if server.contains('=') {
        server
            .split(';')
            .find_map(|p| p.strip_prefix("https="))
            .or_else(|| server.split(';').find_map(|p| p.strip_prefix("http=")))?
            .to_string()
    } else {
        server
    };
    Some(if pick.contains("://") { pick } else { format!("http://{pick}") })
}

#[cfg(not(windows))]
pub fn system_proxy() -> Option<String> {
    None
}

/// Client HTTP riusabile. La cache è per user-agent: cookie e referer viaggiano
/// negli header della singola richiesta, quindi due job con lo stesso UA
/// possono condividere pool di connessioni e sessioni TLS.
///
/// Senza cache ogni download — e ogni singolo resume — rifaceva da zero tutti
/// gli handshake TLS.
fn client_for(state: &AppState, job: &Job) -> (reqwest::Client, HeaderMap) {
    let ua = if job.user_agent.is_empty() { DEFAULT_UA.to_string() } else { job.user_agent.clone() };
    let cached = state.clients.lock().unwrap().get(&ua).cloned();
    let client = match cached {
        Some(c) => c,
        None => {
            let c = build_client_inner(&ua, state.max_conc());
            state.clients.lock().unwrap().insert(ua, c.clone());
            c
        }
    };
    (client, request_headers(job))
}

fn request_headers(job: &Job) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if !job.cookies.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&job.cookies) {
            headers.insert(reqwest::header::COOKIE, v);
        }
    }
    if !job.referrer.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&job.referrer) {
            headers.insert(REFERER, v);
        }
    }
    headers
}

fn build_client_inner(ua: &str, max_conc: u64) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(std::time::Duration::from_secs(20))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        // tenere aperte le connessioni dei segmenti tra un tentativo e l'altro
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(max_conc.max(1) as usize)
        // molti WAF guardano male una richiesta senza Accept
        .default_headers({
            let mut h = HeaderMap::new();
            h.insert(reqwest::header::ACCEPT, HeaderValue::from_static("*/*"));
            h
        });
    if let Some(p) = system_proxy() {
        if let Ok(proxy) = reqwest::Proxy::all(&p) {
            // il proxy aziendale non deve inghiottire localhost e intranet,
            // altrimenti i download da host locali falliscono sempre
            builder = builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1")));
        }
    }
    builder.build().expect("client http")
}

/// Limita la concorrenza con AIMD vero: sui 429 dimezza (8→4→2→1),
/// dopo RAMP_QUIET senza 429 risale di 1 fino a MAX_SEGMENTS.
/// (Un semaforo non basta: coi permessi tutti occupati non c'è nulla da togliere.)
struct Gate {
    max: AtomicU64,
    /// tetto configurato: la risalita non lo supera mai
    cap: u64,
    active: AtomicU64,
    notify: tokio::sync::Notify,
    last_shrink: Mutex<std::time::Instant>,
}

impl Gate {
    fn new(max: u64, cap: u64) -> Self {
        let cap = cap.clamp(1, MAX_SEGMENTS);
        Self {
            max: AtomicU64::new(max.clamp(1, cap)),
            cap,
            active: AtomicU64::new(0),
            notify: tokio::sync::Notify::new(),
            last_shrink: Mutex::new(std::time::Instant::now()),
        }
    }

    async fn enter(&self) {
        loop {
            let got = self
                .active
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |a| {
                    if a < self.max.load(Ordering::SeqCst) { Some(a + 1) } else { None }
                })
                .is_ok();
            if got {
                return;
            }
            // timeout di sicurezza contro la race notify/leave
            let _ = tokio::time::timeout(std::time::Duration::from_millis(250), self.notify.notified()).await;
        }
    }

    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// C'è uno slot libero adesso? Serve a non rubare lavoro che poi resterebbe
    /// fermo in attesa del gate.
    fn has_room(&self) -> bool {
        self.active.load(Ordering::SeqCst) < self.max.load(Ordering::SeqCst)
    }

    fn shrink(&self) -> u64 {
        let mut new = 1;
        let _ = self.max.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |m| {
            new = (m / 2).max(1);
            Some(new)
        });
        *self.last_shrink.lock().unwrap() = std::time::Instant::now();
        new
    }

    /// Additive increase: +1 connessione se non ci sono 429 da `quiet`.
    fn maybe_grow(&self, quiet: std::time::Duration) -> Option<u64> {
        if self.last_shrink.lock().unwrap().elapsed() < quiet {
            return None;
        }
        let m = self.max.load(Ordering::SeqCst);
        if m >= self.cap {
            return None;
        }
        self.max.store(m + 1, Ordering::SeqCst);
        // il grow conta come "evento": la prossima salita aspetta un altro periodo quiet
        *self.last_shrink.lock().unwrap() = std::time::Instant::now();
        self.notify.notify_waiters();
        Some(m + 1)
    }
}

enum SegErr {
    RateLimited(Option<u64>), // Retry-After in secondi, se il server lo dice
    /// If-Range non combacia: il file sul server è cambiato. Ritentare
    /// scriverebbe pezzi di due versioni diverse nello stesso .part.
    Changed,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for SegErr {
    fn from(e: E) -> Self {
        SegErr::Other(e.into())
    }
}

/// Sonnellino interrompibile: esce subito su pausa/cancel/errore terminale.
async fn nap(dl: &Download, secs: u64) {
    for _ in 0..secs * 2 {
        if dl.interrupted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn run_segments(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    part: &Path,
    dl: &Arc<Download>,
    start_conc: u64,
    max_conc: u64,
) -> anyhow::Result<()> {
    let total = dl.total.load(Ordering::Relaxed);
    // resume: assicura che il .part abbia la dimensione giusta
    if tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0) != total {
        let f = tokio::fs::OpenOptions::new().write(true).create(true).open(part).await?;
        f.set_len(total).await?;
    }
    let _ = write_sidecar(dl, part);

    let gate = Arc::new(Gate::new(start_conc, max_conc));
    dl.conc.store(gate.max.load(Ordering::SeqCst), Ordering::Relaxed);

    // coda dei segmenti da fare; quando è vuota i worker rubano lavoro
    // dimezzando il segmento più indietro (stile FDM: nessuna connessione ferma)
    let queue: Arc<Mutex<std::collections::VecDeque<Arc<Seg>>>> = Arc::new(Mutex::new(
        dl.segs.lock().unwrap().iter().filter(|s| s.remaining() > 0).cloned().collect(),
    ));

    let mut tasks = Vec::new();
    for _ in 0..gate.cap {
        let client = client.clone();
        let url = url.to_string();
        let headers = headers.clone();
        let path = part.to_path_buf();
        let dl = dl.clone();
        let gate = gate.clone();
        let queue = queue.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let popped = queue.lock().unwrap().pop_front();
                let seg = match popped {
                    Some(s) => s,
                    None => {
                        // Rubare senza avere uno slot libero spezzetta il file
                        // per niente: con l'AIMD sceso a 1 connessione, gli
                        // altri 7 worker dimezzerebbero i segmenti a ripetizione
                        // restando poi fermi sul gate.
                        if !gate.has_room() {
                            if dl.interrupted() {
                                return Ok(());
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            continue;
                        }
                        match steal_segment(&mut dl.segs.lock().unwrap()) {
                            Some(s) => s,
                            None => return Ok(()), // niente più lavoro
                        }
                    }
                };
                work_segment(&client, &url, &headers, &path, &seg, &dl, &gate).await?;
            }
        }));
    }

    // salva il sidecar ogni 2s: se l'app muore, si riparte da qui
    let saver = {
        let dl = dl.clone();
        let part = part.to_path_buf();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SIDECAR_EVERY).await;
                write_sidecar_async(&dl, &part).await;
            }
        })
    };

    // AIMD in salita: periodicamente riprova ad alzare la concorrenza
    let ramp = {
        let dl = dl.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                if let Some(new_max) = gate.maybe_grow(RAMP_QUIET) {
                    dl.conc.store(new_max, Ordering::Relaxed);
                }
            }
        })
    };

    let mut first_err = None;
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => first_err = first_err.or(Some(e)),
            Err(e) => first_err = first_err.or(Some(anyhow::anyhow!("task: {e}"))),
        }
    }
    saver.abort();
    ramp.abort();
    let _ = write_sidecar(dl, part);
    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Porta a termine un segmento con retry: i contatori di errore si azzerano
/// quando arrivano byte, quindi un download che avanza non muore mai da solo.
async fn work_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    seg: &Arc<Seg>,
    dl: &Arc<Download>,
    gate: &Gate,
) -> anyhow::Result<()> {
    let mut attempts = 0u32;
    let mut rl_hits = 0u32;
    loop {
        if dl.interrupted() {
            bail!("interrotto");
        }
        // Il tentativo precedente può aver contato byte rimasti nel BufWriter e
        // mai finiti su disco. Ripartire da `done` lascerebbe un buco nel file:
        // si riparte sempre dall'ultimo offset flushato.
        let flushed = seg.flushed.load(Ordering::Relaxed);
        let lost = seg.done.swap(flushed, Ordering::Relaxed).saturating_sub(flushed);
        if lost > 0 {
            dl.done.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| Some(d.saturating_sub(lost))).ok();
        }
        let before = seg.done.load(Ordering::Relaxed);
        gate.enter().await;
        let res = fetch_segment(client, url, headers, path, seg, dl).await;
        gate.leave();
        if seg.done.load(Ordering::Relaxed) > before {
            // progresso reale: riparte il conto degli errori consecutivi
            attempts = 0;
            rl_hits = 0;
        }
        match res {
            Ok(()) => return Ok(()),
            Err(SegErr::Changed) => {
                let msg = "il file sul server è cambiato: annulla e riscarica da capo";
                dl.set_fatal(msg);
                bail!("{msg}");
            }
            Err(SegErr::RateLimited(retry_after)) => {
                rl_hits += 1;
                if rl_hits > 12 {
                    bail!("il server continua a rispondere 429 anche a 1 connessione");
                }
                let new_max = gate.shrink();
                dl.conc.store(new_max, Ordering::Relaxed);
                let wait = retry_after.unwrap_or(2 + 3 * rl_hits as u64).min(30);
                nap(dl, wait).await;
            }
            Err(SegErr::Other(e)) => {
                attempts += 1;
                if attempts >= SEGMENT_RETRIES {
                    return Err(e);
                }
                nap(dl, attempts as u64).await;
            }
        }
    }
}

async fn fetch_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    seg: &Seg,
    dl: &Download,
) -> Result<(), SegErr> {
    let start = seg.start + seg.done.load(Ordering::Relaxed);
    if start > seg.end() {
        return Ok(());
    }
    // If-Range: se il file è cambiato il server risponde 200 invece di 206,
    // così ce ne accorgiamo prima di cucire insieme due versioni diverse
    let validator = dl.validator.lock().unwrap().clone();
    let mut req = client.get(url).headers(headers.clone()).header(RANGE, format!("bytes={start}-{}", seg.end()));
    if let Some(v) = &validator {
        if let Ok(v) = HeaderValue::from_str(v) {
            req = req.header(IF_RANGE, v);
        }
    }
    let resp = tokio::time::timeout(SEND_TIMEOUT, req.send())
        .await
        .map_err(|_| SegErr::Other(anyhow::anyhow!("il server non risponde (timeout {}s)", SEND_TIMEOUT.as_secs())))??;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        return Err(SegErr::RateLimited(retry_after));
    }
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        // 200 con If-Range mandato = il validator non combacia più: terminale.
        // Senza validator resta un 200 ambiguo (nodo del CDN senza Range): si ritenta.
        if resp.status() == reqwest::StatusCode::OK && validator.is_some() {
            return Err(SegErr::Changed);
        }
        return Err(SegErr::Other(anyhow::anyhow!("range rifiutato dal server (status {})", resp.status())));
    }

    let mut raw = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    raw.seek(std::io::SeekFrom::Start(start)).await?;
    // buffer da 512KB: molti meno syscall, pattern disco sequenziale.
    // CRASH_REWIND (2MB) copre abbondantemente quanto può restare in buffer.
    let mut file = tokio::io::BufWriter::with_capacity(512 * 1024, raw);
    let mut pos = start;
    let mut since_flush = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        if dl.interrupted() {
            file.flush().await?;
            seg.mark_flushed();
            return Err(SegErr::Other(anyhow::anyhow!("interrotto")));
        }
        // watchdog anti-stallo: connessione morta = errore = riconnessione
        let chunk = match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
            Err(_) => {
                file.flush().await?;
                seg.mark_flushed();
                return Err(SegErr::Other(anyhow::anyhow!(
                    "stallo: nessun dato per {}s, riconnetto",
                    STALL_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => break,
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                // connessione caduta a metà: salva quello che è arrivato, così
                // il retry riparte da lì invece di rileggere da capo
                file.flush().await?;
                seg.mark_flushed();
                return Err(SegErr::Other(anyhow::Error::new(e).context("stream interrotto")));
            }
        };
        // il work stealing può aver ristretto end: scrivi solo la parte nostra
        let end = seg.end();
        if pos > end {
            break;
        }
        let take = chunk.len().min((end - pos + 1) as usize);
        dl.limiter.acquire(take as u64).await;
        file.write_all(&chunk[..take]).await?;
        pos += take as u64;
        seg.done.fetch_add(take as u64, Ordering::Relaxed);
        dl.done.fetch_add(take as u64, Ordering::Relaxed);
        // flush periodico: fa avanzare l'offset sicuro del sidecar anche a
        // metà segmento, così un crash costa al massimo FLUSH_EVERY byte
        since_flush += take as u64;
        if since_flush >= FLUSH_EVERY {
            file.flush().await?;
            seg.mark_flushed();
            since_flush = 0;
        }
        if take < chunk.len() {
            break; // fine del segmento (ristretto): il resto è del ladro
        }
    }
    file.flush().await?;
    seg.mark_flushed();
    // il server può chiudere lo stream a metà: prima veniva ignorato e il file
    // finiva con un buco. Ora è un errore e il retry riparte dal byte esatto.
    if seg.start + seg.done.load(Ordering::Relaxed) <= seg.end() {
        return Err(SegErr::Other(anyhow::anyhow!("connessione chiusa a metà segmento, riprendo")));
    }
    Ok(())
}

async fn single_stream(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    part: &Path,
    dl: &Arc<Download>,
    reuse: Option<reqwest::Response>,
) -> anyhow::Result<()> {
    let resp = match reuse {
        Some(r) => r,
        None => tokio::time::timeout(SEND_TIMEOUT, client.get(url).headers(headers.clone()).send())
            .await
            .map_err(|_| anyhow::anyhow!("il server non risponde (timeout {}s)", SEND_TIMEOUT.as_secs()))??,
    };
    anyhow::ensure!(resp.status().is_success(), "status {}", resp.status());
    if let Some(len) = resp.content_length().filter(|l| *l > 0) {
        dl.total.store(len, Ordering::Relaxed);
    }

    let seg = dl.segs.lock().unwrap().first().cloned();
    let mut file = tokio::io::BufWriter::with_capacity(512 * 1024, tokio::fs::File::create(part).await?);
    let mut written = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        if dl.interrupted() {
            file.flush().await?;
            bail!("interrotto");
        }
        let chunk = match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
            Err(_) => {
                file.flush().await?;
                bail!("stallo: nessun dato per {}s", STALL_TIMEOUT.as_secs());
            }
            Ok(None) => break,
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                file.flush().await?;
                return Err(anyhow::Error::new(e).context("stream interrotto"));
            }
        };
        dl.limiter.acquire(chunk.len() as u64).await;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        dl.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        if let Some(seg) = &seg {
            seg.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }
    file.flush().await?;
    if let Some(seg) = &seg {
        seg.mark_flushed();
    }
    let total = dl.total.load(Ordering::Relaxed);
    if total > 0 && written < total {
        bail!("connessione chiusa in anticipo ({} su {})", crate::ui::fmt_bytes(written), crate::ui::fmt_bytes(total));
    }
    Ok(())
}

/// Snapshot serializzato dello stato, da scrivere su disco.
fn sidecar_bytes(dl: &Download) -> anyhow::Result<Vec<u8>> {
    let sc = Sidecar {
        job: dl.job.lock().unwrap().clone(),
        total: dl.total.load(Ordering::Relaxed),
        resumable: dl.resumable.load(Ordering::Relaxed),
        validator: dl.validator.lock().unwrap().clone(),
        segments: dl
            .segs
            .lock()
            .unwrap()
            .iter()
            // `flushed`, non `done`: solo questi byte sopravvivono a un crash
            .map(|s| SegSave { start: s.start, end: s.end(), done: s.flushed.load(Ordering::Relaxed).min(s.len()) })
            .collect(),
    };
    Ok(serde_json::to_vec(&sc)?)
}

/// Versione sincrona, per i punti che già non sono nel percorso caldo
/// (fine download, pausa, abort).
fn write_sidecar(dl: &Download, part: &Path) -> anyhow::Result<()> {
    std::fs::write(sidecar_path(part), sidecar_bytes(dl)?)?;
    Ok(())
}

/// Versione per il salvataggio periodico: la scrittura va su un thread
/// bloccante, non su un worker del runtime dove fermerebbe anche i segmenti
/// che stanno scaricando su quello stesso thread.
async fn write_sidecar_async(dl: &Download, part: &Path) {
    let Ok(bytes) = sidecar_bytes(dl) else { return };
    let path = sidecar_path(part);
    let _ = tokio::task::spawn_blocking(move || std::fs::write(path, bytes)).await;
}

fn host_of(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url).split(['/', '?', '#']).next().unwrap_or("").to_string()
}

/// Quante connessioni ha tollerato questo host l'ultima volta. La memoria è
/// nella config, quindi sopravvive al riavvio: un host che rate-limita non va
/// ri-martellato a 8 connessioni ogni volta che si apre l'app.
fn remembered_conc(state: &AppState, url: &str) -> u64 {
    let cfg = state.config.get();
    cfg.host_conc.get(&host_of(url)).copied().unwrap_or(cfg.max_connections).min(cfg.max_connections)
}

fn remember_conc(state: &AppState, url: &str, dl: &Download) {
    let conc = dl.conc.load(Ordering::Relaxed);
    let host = host_of(url);
    state.config.edit(|c| {
        if conc < c.max_connections {
            c.host_conc.insert(host, conc);
        } else {
            c.host_conc.remove(&host); // l'host regge di nuovo il massimo
        }
    });
}

fn sidecar_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_os_string();
    s.push(".mdm.json");
    PathBuf::from(s)
}

// ---- helpers puri ----

/// Range inclusivi [(start, end); n] che coprono `len` byte.
fn split_segments(len: u64, max_parts: u64, min_size: u64) -> Vec<(u64, u64)> {
    let n = (len / min_size).clamp(1, max_parts);
    let base = len / n;
    (0..n)
        .map(|i| {
            let start = i * base;
            let end = if i == n - 1 { len - 1 } else { (i + 1) * base - 1 };
            (start, end)
        })
        .collect()
}

/// Nomi DOS che su Windows non sono file: `CON.zip` non si può creare.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == ' ').to_string();
    if cleaned.is_empty() {
        return "download".to_string();
    }
    // il nome riservato vale anche con estensione, e conta solo lo stem
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) { format!("_{cleaned}") } else { cleaned }
}

/// `file.zip`, `file (1).zip`, `file (2).zip`, ...
fn nth_candidate(path: &Path, i: u32) -> PathBuf {
    if i == 0 {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    path.parent().unwrap_or(Path::new(".")).join(format!("{stem} ({i}){ext}"))
}

/// Sceglie un nome libero e **riserva** subito il `.part` con `create_new`:
/// due job concorrenti con lo stesso nome non possono più collidere, perché
/// la creazione atomica del file fa da lock.
fn reserve_path(path: &Path) -> anyhow::Result<PathBuf> {
    for i in 0..10_000 {
        let cand = nth_candidate(path, i);
        if cand.exists() {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(part_path(&cand)) {
            Ok(_) => return Ok(cand),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("creazione del file .part"),
        }
    }
    bail!("troppi file con lo stesso nome in Downloads")
}

/// Primo nome non occupato, senza riservare nulla (usato appena prima del rename).
fn free_name(path: &Path) -> PathBuf {
    (0..).map(|i| nth_candidate(path, i)).find(|p| !p.exists()).unwrap_or_else(|| path.to_path_buf())
}

fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// "bytes 0-0/104857600" -> Some(104857600)
fn content_range_total(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok().filter(|l| *l > 0)
}

fn filename_from_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let last = no_query.trim_end_matches('/').rsplit('/').next().unwrap_or("download");
    let decoded = percent_decode(last);
    if decoded.is_empty() { "download".to_string() } else { decoded }
}

fn filename_from_disposition(v: &str) -> Option<String> {
    for part in v.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("filename*=") {
            // RFC 5987: charset'lingua'valore. La lingua è quasi sempre vuota
            // ("UTF-8''nome") ma quando c'è va scartata, non incollata al nome.
            let rest = rest.trim_matches('"');
            let value = match rest.split_once('\'') {
                Some((_charset, tail)) => tail.split_once('\'').map(|(_lang, v)| v).unwrap_or(tail),
                None => rest,
            };
            return Some(percent_decode(value));
        }
        if let Some(rest) = p.strip_prefix("filename=") {
            return Some(rest.trim_matches('"').to_string());
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_everything() {
        for len in [1u64, 100, MIN_SEGMENT * 2, MIN_SEGMENT * 8 + 3, MIN_SEGMENT * 100] {
            let parts = split_segments(len, MAX_SEGMENTS, MIN_SEGMENT);
            assert!(parts.len() <= MAX_SEGMENTS as usize);
            assert_eq!(parts[0].0, 0);
            assert_eq!(parts.last().unwrap().1, len - 1);
            for w in parts.windows(2) {
                assert_eq!(w[0].1 + 1, w[1].0); // contigui, senza buchi
            }
        }
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_filename("a<b>c:d.zip"), "a_b_c_d.zip");
        assert_eq!(sanitize_filename("..."), "download");
        assert_eq!(sanitize_filename("ok name.iso"), "ok name.iso");
    }

    #[test]
    fn sanitize_reserved_dos_names() {
        // su Windows questi non sono file: senza prefisso la create fallisce
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("con.zip"), "_con.zip");
        assert_eq!(sanitize_filename("LPT9.tar.gz"), "_LPT9.tar.gz");
        // simili ma legittimi: non toccarli
        assert_eq!(sanitize_filename("CONSOLE.zip"), "CONSOLE.zip");
        assert_eq!(sanitize_filename("COM10.bin"), "COM10.bin");
    }

    #[test]
    fn candidates_numbered() {
        let p = Path::new("/d/file.zip");
        assert_eq!(nth_candidate(p, 0), PathBuf::from("/d/file.zip"));
        assert_eq!(nth_candidate(p, 2), PathBuf::from("/d/file (2).zip"));
        assert_eq!(nth_candidate(Path::new("/d/noext"), 1), PathBuf::from("/d/noext (1)"));
    }

    #[test]
    fn reserve_locks_the_name() {
        let dir = std::env::temp_dir().join(format!("mdm-reserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("f.zip");

        // il primo prende f.zip e ne riserva il .part
        let a = reserve_path(&target).unwrap();
        assert_eq!(a, target);
        assert!(part_path(&a).exists());
        // il secondo non può riprendersi lo stesso nome
        let b = reserve_path(&target).unwrap();
        assert_eq!(b, dir.join("f (1).zip"));
        // nome gia' occupato da un file vero: si salta
        std::fs::write(dir.join("f (2).zip"), b"x").unwrap();
        assert_eq!(reserve_path(&target).unwrap(), dir.join("f (3).zip"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_range() {
        assert_eq!(content_range_total("bytes 0-0/104857600"), Some(104857600));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
    }

    #[test]
    fn url_names() {
        assert_eq!(filename_from_url("https://x.it/a/file%20v2.zip?tok=1"), "file v2.zip");
        assert_eq!(filename_from_url("https://x.it/"), "x.it");
    }

    #[test]
    fn disposition_names() {
        assert_eq!(filename_from_disposition(r#"attachment; filename="a b.zip""#), Some("a b.zip".into()));
        assert_eq!(filename_from_disposition("attachment; filename*=UTF-8''a%20b.zip"), Some("a b.zip".into()));
        // con il tag di lingua valorizzato: va scartato, non finire nel nome
        assert_eq!(filename_from_disposition("attachment; filename*=UTF-8'it'relazione%202024.pdf"), Some("relazione 2024.pdf".into()));
        assert_eq!(filename_from_disposition("attachment; filename*=iso-8859-1'en'file.zip"), Some("file.zip".into()));
        assert_eq!(filename_from_disposition("inline"), None);
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://a.b.c/x/y?z=1"), "a.b.c");
        assert_eq!(host_of("http://h:8080/f.zip"), "h:8080");
    }

    #[tokio::test]
    async fn gate_aimd() {
        let g = Gate::new(8, 8);
        assert_eq!(g.shrink(), 4);
        assert_eq!(g.shrink(), 2);
        assert_eq!(g.shrink(), 1);
        assert_eq!(g.shrink(), 1); // mai sotto 1
        g.enter().await; // occupa l'unico slot
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(60), g.enter()).await.is_err();
        assert!(blocked, "con max=1 il secondo enter deve bloccare");
        g.leave();
        tokio::time::timeout(std::time::Duration::from_millis(500), g.enter())
            .await
            .expect("dopo leave() lo slot si libera");
    }

    #[test]
    fn gate_grow() {
        let g = Gate::new(8, 8);
        assert_eq!(g.shrink(), 4);
        // quiet non ancora passato: niente salita
        assert_eq!(g.maybe_grow(std::time::Duration::from_secs(3600)), None);
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), Some(5));
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), Some(6));
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), Some(7));
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), Some(8));
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), None); // mai sopra il cap
    }

    #[test]
    fn gate_respects_configured_cap() {
        // con max_connections=2 in config la risalita si ferma a 2
        let g = Gate::new(8, 2);
        assert_eq!(g.max.load(Ordering::SeqCst), 2, "lo start viene clampato al cap");
        assert_eq!(g.shrink(), 1);
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), Some(2));
        assert_eq!(g.maybe_grow(std::time::Duration::ZERO), None);
    }

    #[test]
    fn steal_splits_biggest() {
        let mb = 1024 * 1024;
        let a = Seg::new(0, 100 * mb - 1, 10 * mb); // 90 MB residui
        let b = Seg::new(100 * mb, 110 * mb - 1, 0); // 10 MB residui
        let mut segs = vec![a.clone(), b];
        let t = steal_segment(&mut segs).expect("c'è da rubare");
        // il ladro prende la metà alta di a, copertura contigua senza buchi
        assert_eq!(t.end(), 100 * mb - 1);
        assert_eq!(a.end() + 1, t.start);
        assert!(t.len() >= STEAL_MIN);
        assert!(a.remaining() >= STEAL_MIN);
        assert_eq!(segs.len(), 3);
    }

    #[test]
    fn steal_leaves_small_segments_alone() {
        let a = Seg::new(0, STEAL_MIN * 2 - 2, 0); // residuo sotto soglia
        let mut segs = vec![a];
        assert!(steal_segment(&mut segs).is_none());
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn sidecar_roundtrip() {
        let sc = Sidecar {
            job: Job {
                url: "https://x.it/f.zip".into(),
                filename: "f.zip".into(),
                referrer: String::new(),
                cookies: "a=1".into(),
                user_agent: "ua".into(),
            },
            total: 1000,
            resumable: true,
            validator: Some("\"abc123\"".into()),
            segments: vec![SegSave { start: 0, end: 499, done: 100 }, SegSave { start: 500, end: 999, done: 0 }],
        };
        let raw = serde_json::to_vec(&sc).unwrap();
        let back: Sidecar = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.total, 1000);
        assert!(back.resumable);
        assert_eq!(back.validator.as_deref(), Some("\"abc123\""));
        assert_eq!(back.segments.len(), 2);
        assert_eq!(back.segments[0].done, 100);
        assert_eq!(back.job.cookies, "a=1");
    }

    #[test]
    fn sidecar_without_validator_still_parses() {
        // sidecar scritti da versioni precedenti devono restare leggibili
        let raw = br#"{"job":{"url":"u"},"total":10,"resumable":true,"segments":[]}"#;
        let back: Sidecar = serde_json::from_slice(raw).unwrap();
        assert_eq!(back.validator, None);
        assert_eq!(back.total, 10);
    }

    #[test]
    fn flushed_trails_done() {
        let s = Seg::new(0, 999, 0);
        s.done.store(500, Ordering::Relaxed);
        // niente flush: l'offset sicuro non si è mosso
        assert_eq!(s.flushed.load(Ordering::Relaxed), 0);
        s.mark_flushed();
        assert_eq!(s.flushed.load(Ordering::Relaxed), 500);
        // ripartendo da sidecar, done e flushed coincidono
        let restored = Seg::new(0, 999, 500);
        assert_eq!(restored.done.load(Ordering::Relaxed), restored.flushed.load(Ordering::Relaxed));
    }
}

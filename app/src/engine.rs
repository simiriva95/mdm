//! Engine di download: segmentato (HTTP Range, N connessioni) o stream singolo,
//! con pausa/resume e ripristino dopo crash/riavvio via sidecar `.mdm.json`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_DISPOSITION, RANGE, REFERER};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const MAX_SEGMENTS: u64 = 8;
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;
const SEGMENT_RETRIES: u32 = 3;
const SIDECAR_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
// dopo un crash gli ultimi write potrebbero non essere arrivati su disco
// (buffer interno di tokio::fs::File): al ripristino arretriamo ogni segmento
const CRASH_REWIND: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Connecting,
    Active,
    Paused,
    Done,
    Failed(String),
    Cancelled,
}

pub struct Seg {
    pub start: u64,
    pub end: u64,
    pub done: AtomicU64,
}

impl Seg {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }
}

pub struct Download {
    pub id: u64,
    pub name: Mutex<String>,
    pub path: Mutex<PathBuf>,
    pub total: AtomicU64, // 0 = ignoto
    pub done: AtomicU64,
    pub segs: Mutex<Vec<Arc<Seg>>>,
    pub resumable: AtomicBool, // segmentato con Range: riprende dal punto esatto
    pub status: Mutex<Status>,
    pub cancel: AtomicBool,
    pub pause: AtomicBool,
    pub job: Mutex<Job>,
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    job: Job,
    total: u64,
    resumable: bool,
    segments: Vec<SegSave>,
}

#[derive(Serialize, Deserialize)]
struct SegSave {
    start: u64,
    end: u64,
    done: u64,
}

#[derive(Default)]
pub struct AppState {
    pub downloads: Mutex<Vec<Arc<Download>>>,
    pub log: Mutex<Vec<String>>,
    pub show_window: AtomicBool,
    pub egui_ctx: Mutex<Option<eframe::egui::Context>>,
    pub rt: Mutex<Option<tokio::runtime::Handle>>,
    next_id: AtomicU64,
}

impl AppState {
    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        eprintln!("[mdm] {line}");
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

    fn new_download(&self, job: Job) -> Arc<Download> {
        Arc::new(Download {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            name: Mutex::new(if job.filename.is_empty() { job.url.clone() } else { job.filename.clone() }),
            path: Mutex::new(PathBuf::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            segs: Mutex::new(Vec::new()),
            resumable: AtomicBool::new(false),
            status: Mutex::new(Status::Connecting),
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            job: Mutex::new(job),
        })
    }
}

pub async fn run_job(state: Arc<AppState>, job: Job) {
    let dl = state.new_download(job.clone());
    state.downloads.lock().unwrap().push(dl.clone());
    state.log(format!("nuovo job: {}", job.url));
    state.show_window.store(true, Ordering::Relaxed);
    state.repaint();

    let result = download(&state, &dl, &job, true).await;
    finish(&state, &dl, result);
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
    state.log(format!("riprendo: {}", dl.name.lock().unwrap()));
    state.repaint();

    let job = dl.job.lock().unwrap().clone();
    let part = part_path(&dl.path.lock().unwrap());

    let result = if dl.resumable.load(Ordering::Relaxed) && dl.total.load(Ordering::Relaxed) > 0 && part.exists() {
        // riparte dal punto esatto, segmento per segmento
        let sum: u64 = dl.segs.lock().unwrap().iter().map(|s| s.done.load(Ordering::Relaxed)).sum();
        dl.done.store(sum, Ordering::Relaxed);
        *dl.status.lock().unwrap() = Status::Active;
        state.repaint();
        let (client, headers) = build_client(&job);
        match run_segments(&client, &job.url, &headers, &part, &dl).await {
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
    let part = part_path(&dl.path.lock().unwrap());
    let mut status = dl.status.lock().unwrap();
    match result {
        Ok(path) => {
            *status = Status::Done;
            state.log(format!("completato: {}", path.display()));
        }
        Err(_) if dl.pause.load(Ordering::Relaxed) => {
            *status = Status::Paused;
            let _ = write_sidecar(dl, &part);
            state.log(format!("in pausa: {}", dl.name.lock().unwrap()));
        }
        Err(_) if dl.cancel.load(Ordering::Relaxed) => {
            *status = Status::Cancelled;
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(sidecar_path(&part));
            state.log(format!("annullato: {}", dl.name.lock().unwrap()));
        }
        Err(e) => {
            // file e sidecar restano: [resume] ritenta da dove era arrivato
            *status = Status::Failed(format!("{e:#}"));
            let _ = write_sidecar(dl, &part);
            state.log(format!("ERRORE: {e:#}"));
        }
    }
    drop(status);
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
    let Some(dir) = dirs::download_dir() else { return };
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
        let segs: Vec<Arc<Seg>> = sc
            .segments
            .iter()
            .map(|s| {
                let safe_done = if sc.resumable { s.done.saturating_sub(CRASH_REWIND) } else { 0 };
                Arc::new(Seg { start: s.start, end: s.end, done: AtomicU64::new(safe_done) })
            })
            .collect();
        dl.done.store(segs.iter().map(|s| s.done.load(Ordering::Relaxed)).sum(), Ordering::Relaxed);
        *dl.segs.lock().unwrap() = segs;
        *dl.status.lock().unwrap() = Status::Paused;
        state.log(format!("ripristinato dal disco: {} (riprendi quando vuoi)", dl.name.lock().unwrap()));
        state.downloads.lock().unwrap().push(dl);
    }
}

async fn download(state: &AppState, dl: &Arc<Download>, job: &Job, fresh: bool) -> anyhow::Result<PathBuf> {
    let (client, headers) = build_client(job);

    // probe: GET con Range 0-0. 206 => range supportati + totale da Content-Range;
    // 200 => niente range, ma la risposta è il file intero e la riusiamo come stream.
    let probe = client
        .get(&job.url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .context("connessione fallita")?;
    anyhow::ensure!(probe.status().is_success(), "status {}", probe.status());

    let disp_name = probe
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_disposition);
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

    let path = if fresh {
        let raw_name = if !job.filename.is_empty() {
            job.filename.clone()
        } else if let Some(n) = disp_name {
            n
        } else {
            filename_from_url(&job.url)
        };
        let dir = dirs::download_dir().context("cartella Downloads non trovata")?;
        dedupe_path(&dir.join(sanitize_filename(&raw_name)))
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

    let segmented = ranges && len.is_some_and(|l| l >= 2 * MIN_SEGMENT);
    dl.resumable.store(segmented, Ordering::Relaxed);

    if segmented {
        let len = len.unwrap();
        drop(probe);
        let segs: Vec<Arc<Seg>> = split_segments(len, MAX_SEGMENTS, MIN_SEGMENT)
            .into_iter()
            .map(|(start, end)| Arc::new(Seg { start, end, done: AtomicU64::new(0) }))
            .collect();
        state.log(format!("{} segmenti paralleli, {}", segs.len(), crate::ui::fmt_bytes(len)));
        *dl.segs.lock().unwrap() = segs;
        let f = tokio::fs::File::create(&part).await?;
        f.set_len(len).await?; // pre-alloca
        drop(f);
        run_segments(&client, &job.url, &headers, &part, dl).await?;
    } else {
        state.log("stream singolo (il server non supporta Range)".to_string());
        *dl.segs.lock().unwrap() = vec![Arc::new(Seg {
            start: 0,
            end: len.unwrap_or(1).saturating_sub(1),
            done: AtomicU64::new(0),
        })];
        let _ = write_sidecar(dl, &part);
        // se il probe era un 200 pieno riusiamo quella risposta, altrimenti nuova GET
        let reuse = if ranges { None } else { Some(probe) };
        single_stream(&client, &job.url, &headers, &part, dl, reuse).await?;
    }
    finalize(dl, &part).await
}

async fn finalize(dl: &Download, part: &Path) -> anyhow::Result<PathBuf> {
    let path = dl.path.lock().unwrap().clone();
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

fn build_client(job: &Job) -> (reqwest::Client, HeaderMap) {
    let ua = if job.user_agent.is_empty() { "mdm/0.2".to_string() } else { job.user_agent.clone() };
    let mut builder = reqwest::Client::builder().user_agent(ua);
    if let Some(p) = system_proxy() {
        if let Ok(proxy) = reqwest::Proxy::all(&p) {
            builder = builder.proxy(proxy);
        }
    }
    let client = builder.build().expect("client http");
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
    (client, headers)
}

async fn run_segments(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    part: &Path,
    dl: &Arc<Download>,
) -> anyhow::Result<()> {
    let total = dl.total.load(Ordering::Relaxed);
    // resume: assicura che il .part abbia la dimensione giusta
    if tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0) != total {
        let f = tokio::fs::OpenOptions::new().write(true).create(true).open(part).await?;
        f.set_len(total).await?;
    }
    let _ = write_sidecar(dl, part);

    let segs = dl.segs.lock().unwrap().clone();
    let mut tasks = Vec::new();
    for seg in segs {
        if seg.done.load(Ordering::Relaxed) >= seg.len() {
            continue;
        }
        let client = client.clone();
        let url = url.to_string();
        let headers = headers.clone();
        let path = part.to_path_buf();
        let dl = dl.clone();
        tasks.push(tokio::spawn(async move {
            let mut last_err = anyhow::anyhow!("segmento non avviato");
            for attempt in 0..SEGMENT_RETRIES {
                if dl.cancel.load(Ordering::Relaxed) || dl.pause.load(Ordering::Relaxed) {
                    bail!("interrotto");
                }
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(800 * attempt as u64)).await;
                }
                match fetch_segment(&client, &url, &headers, &path, &seg, &dl).await {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = e,
                }
            }
            Err(last_err)
        }));
    }

    // salva il sidecar ogni 2s: se l'app muore, si riparte da qui
    let saver = {
        let dl = dl.clone();
        let part = part.to_path_buf();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SIDECAR_EVERY).await;
                let _ = write_sidecar(&dl, &part);
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
    let _ = write_sidecar(dl, part);
    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

async fn fetch_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    seg: &Seg,
    dl: &Download,
) -> anyhow::Result<()> {
    let start = seg.start + seg.done.load(Ordering::Relaxed);
    if start > seg.end {
        return Ok(());
    }
    let resp = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, format!("bytes={start}-{}", seg.end))
        .send()
        .await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        bail!("range rifiutato dal server (status {})", resp.status());
    }

    let mut file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if dl.cancel.load(Ordering::Relaxed) || dl.pause.load(Ordering::Relaxed) {
            file.flush().await?;
            bail!("interrotto");
        }
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        seg.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        dl.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    file.flush().await?;
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
        None => client.get(url).headers(headers.clone()).send().await?,
    };
    anyhow::ensure!(resp.status().is_success(), "status {}", resp.status());
    if let Some(len) = resp.content_length().filter(|l| *l > 0) {
        dl.total.store(len, Ordering::Relaxed);
    }

    let seg = dl.segs.lock().unwrap().first().cloned();
    let mut file = tokio::fs::File::create(part).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if dl.cancel.load(Ordering::Relaxed) || dl.pause.load(Ordering::Relaxed) {
            file.flush().await?;
            bail!("interrotto");
        }
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        dl.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        if let Some(seg) = &seg {
            seg.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }
    file.flush().await?;
    Ok(())
}

fn write_sidecar(dl: &Download, part: &Path) -> anyhow::Result<()> {
    let sc = Sidecar {
        job: dl.job.lock().unwrap().clone(),
        total: dl.total.load(Ordering::Relaxed),
        resumable: dl.resumable.load(Ordering::Relaxed),
        segments: dl
            .segs
            .lock()
            .unwrap()
            .iter()
            .map(|s| SegSave { start: s.start, end: s.end, done: s.done.load(Ordering::Relaxed) })
            .collect(),
    };
    std::fs::write(sidecar_path(part), serde_json::to_vec(&sc)?)?;
    Ok(())
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
    if cleaned.is_empty() { "download".to_string() } else { cleaned }
}

fn dedupe_path(path: &Path) -> PathBuf {
    if !path.exists() && !part_path(path).exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists() && !part_path(p).exists())
        .unwrap()
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
            let rest = rest.trim_start_matches("UTF-8''").trim_start_matches("utf-8''");
            return Some(percent_decode(rest.trim_matches('"')));
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
        assert_eq!(filename_from_disposition("inline"), None);
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
            segments: vec![SegSave { start: 0, end: 499, done: 100 }, SegSave { start: 500, end: 999, done: 0 }],
        };
        let raw = serde_json::to_vec(&sc).unwrap();
        let back: Sidecar = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.total, 1000);
        assert!(back.resumable);
        assert_eq!(back.segments.len(), 2);
        assert_eq!(back.segments[0].done, 100);
        assert_eq!(back.job.cookies, "a=1");
    }
}

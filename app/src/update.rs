//! Auto-update via GitHub Releases: controlla periodicamente l'ultima
//! release, e su richiesta scarica l'installer, verifica lo sha256,
//! lo lancia silenzioso e riavvia l'app.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Context as _;

use crate::engine::AppState;

const REPO: &str = "simiriva95/mdm";
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub sha256: Option<String>,
}

/// "v1.2.3" -> (1,2,3)
fn parse_ver(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn client() -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().user_agent(concat!("mdm/", env!("CARGO_PKG_VERSION")));
    if let Some(p) = crate::engine::system_proxy() {
        if let Ok(proxy) = reqwest::Proxy::all(&p) {
            builder = builder.proxy(proxy);
        }
    }
    Ok(builder.build()?)
}

/// Primo check poco dopo l'avvio, poi ogni 6 ore.
pub async fn check_loop(state: Arc<AppState>) {
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    loop {
        if let Err(e) = check_once(&state).await {
            state.log(format!("controllo aggiornamenti fallito: {e:#}"));
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

async fn check_once(state: &Arc<AppState>) -> anyhow::Result<()> {
    let resp = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "status {}", resp.status());
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes().await?)?;

    let tag = v["tag_name"].as_str().unwrap_or_default().to_string();
    let latest = parse_ver(&tag).context("tag non semver")?;
    let current = parse_ver(env!("CARGO_PKG_VERSION")).expect("versione crate valida");
    if latest <= current {
        return Ok(());
    }

    let asset = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == "mdm-setup.exe"))
        .context("release senza mdm-setup.exe")?;
    let url = asset["browser_download_url"].as_str().context("asset senza url")?.to_string();
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|s| s.to_lowercase());

    let known = state.update.lock().unwrap().as_ref().map(|u| u.version.clone());
    if known.as_deref() != Some(tag.as_str()) {
        state.log(format!("aggiornamento disponibile: {tag} (sei su v{})", env!("CARGO_PKG_VERSION")));
        *state.update.lock().unwrap() = Some(UpdateInfo { version: tag, url, sha256 });
        state.repaint();
    }
    Ok(())
}

/// Scarica, verifica, installa silenzioso, riavvia.
pub async fn apply(state: Arc<AppState>) {
    if state.updating.swap(true, Ordering::SeqCst) {
        return; // già in corso
    }
    if let Err(e) = apply_inner(&state).await {
        state.updating.store(false, Ordering::SeqCst);
        state.log(format!("ERRORE aggiornamento: {e:#}"));
    }
    state.repaint();
}

async fn apply_inner(state: &Arc<AppState>) -> anyhow::Result<()> {
    let Some((url, sha256, version)) = state
        .update
        .lock()
        .unwrap()
        .as_ref()
        .map(|u| (u.url.clone(), u.sha256.clone(), u.version.clone()))
    else {
        return Ok(());
    };

    state.log(format!("scarico {version}..."));
    state.repaint();
    let bytes = client()?.get(&url).send().await?.error_for_status()?.bytes().await?;

    if let Some(want) = sha256 {
        use sha2::{Digest, Sha256};
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        anyhow::ensure!(got == want, "sha256 non corrisponde: atteso {want}, ottenuto {got}");
        state.log("sha256 verificato");
    } else {
        state.log("release senza digest: salto verifica sha256");
    }

    let setup = std::env::temp_dir().join("mdm-setup.exe");
    tokio::fs::write(&setup, &bytes).await?;

    // metti in pausa i download attivi: sidecar puliti, si riprende al riavvio
    for dl in state.downloads.lock().unwrap().iter() {
        dl.pause.store(true, Ordering::Relaxed);
    }
    // aspetta la pausa vera invece di un tempo fisso: un disco lento o un
    // segmento grosso da flushare impiegano piu' di 1200ms, e tagliare li'
    // significherebbe perdere il progresso appena fatto
    for _ in 0..100 {
        let pending = state
            .downloads
            .lock()
            .unwrap()
            .iter()
            .filter(|d| {
                matches!(
                    *d.status.lock().unwrap(),
                    crate::engine::Status::Active | crate::engine::Status::Connecting
                )
            })
            .count();
        if pending == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    state.log(format!("installo {version} e riavvio..."));
    state.repaint();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let exe = std::env::current_exe()?;
        // aspetta la nostra uscita, installa silenzioso, rilancia
        std::process::Command::new("cmd")
            .args([
                "/C",
                &format!(
                    "timeout /t 2 /nobreak >nul & \"{}\" /VERYSILENT /SUPPRESSMSGBOXES & start \"\" \"{}\"",
                    setup.display(),
                    exe.display()
                ),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
    }

    state.quit.store(true, Ordering::Relaxed);
    state.repaint();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_ver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_ver("boh"), None);
        assert!(parse_ver("v0.4.0") < parse_ver("v0.10.0")); // confronto numerico, non lessicale
        assert!(parse_ver(env!("CARGO_PKG_VERSION")).is_some());
    }
}

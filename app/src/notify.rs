//! Notifiche di sistema a fine download.
//!
//! Windows attribuisce i toast a un AppUserModelID: senza registrarne uno
//! nostro la notifica comparirebbe come "Windows PowerShell". Lo registriamo
//! sotto HKCU (niente admin) puntando all'icona dell'exe.

#[cfg(windows)]
const APP_ID: &str = "com.sriva.mdm";

/// Registra l'AUMID. Idempotente, da chiamare una volta all'avvio.
#[cfg(windows)]
pub fn register() -> anyhow::Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(format!("Software\\Classes\\AppUserModelId\\{APP_ID}"))?;
    key.set_value("DisplayName", &"MDM")?;
    let exe = std::env::current_exe()?;
    key.set_value("IconUri", &exe.to_string_lossy().to_string())?;
    Ok(())
}

#[cfg(not(windows))]
pub fn register() -> anyhow::Result<()> {
    Ok(())
}

/// Toast di fine download. Non fallisce mai in modo rumoroso: una notifica
/// non riuscita non deve disturbare un download andato a buon fine.
#[cfg(windows)]
pub fn finished(name: &str, ok: bool, detail: &str) {
    use notify_rust::Notification;
    let summary = if ok { "Download completato" } else { "Download fallito" };
    let body = if detail.is_empty() { name.to_string() } else { format!("{name}\n{detail}") };
    let _ = Notification::new().app_id(APP_ID).summary(summary).body(&body).show();
}

#[cfg(not(windows))]
pub fn finished(_name: &str, _ok: bool, _detail: &str) {}

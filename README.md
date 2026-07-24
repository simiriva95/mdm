# MDM — Mini Download Manager

Download manager minimale per Windows: intercetta i download grandi da Chrome e li scarica con 8 connessioni parallele nella cartella Downloads. Un binario Rust + un'estensione Chrome minuscola. Zero bloat.

## Features

- **8 connessioni parallele** (HTTP Range) con mappa segmenti live stile torrent nella barra di progresso
- **Pausa / resume dal punto esatto**, anche dopo crash, riavvio del PC o kill dell'app: lo stato vive in un sidecar `.mdm.json` accanto al `.part`, salvato ogni 2 secondi
- **UI "desktop di terminali"**: finestre navy con chrome `[X][+][_]`, gauge pastello, sparkline di rete, console log
- File piccoli restano in Chrome (soglia 10MB / estensioni note); click sull'icona estensione = ON/OFF
- Cookie + referer inoltrati: funzionano anche i download dietro login
- Se l'app non risponde, il download torna automaticamente a Chrome: mai perso

## Installazione one-click (Windows)

1. [**Releases**](../../releases/latest) → scarica **`mdm-setup.exe`** → doppio click. Fine (niente admin: installa in `%LOCALAPPDATA%\MDM`, registra tutto lui).
2. Una volta sola: `chrome://extensions` → **Developer mode** ON → **Load unpacked** → cartella `%LOCALAPPDATA%\MDM\extension` (il setup te la apre e te lo ricorda).

Da ora i download grandi passano da MDM.

Alternativa senza installer: dalla release scarica `mdm-windows.zip` → estrai → doppio click `install.bat`. Nuova release: `git tag v0.x.y && git push origin v0.x.y` — la CI compila, crea l'installer e pubblica la release da sola.

### Da sorgente

```
cd app
cargo build --release
..\install\install.bat
```

## Architettura

```
Chrome --onDeterminingFilename--> extension/background.js
  | cancella dal browser, raccoglie url+cookie+referer
  v sendNativeMessage (stdio)
mdm.exe --host  --TCP 127.0.0.1:48666-->  mdm.exe (UI + engine)
                (se l'app è spenta la avvia)      -> %USERPROFILE%\Downloads
```

Un solo binario, due modalità. Resume: ogni segmento salva `start/end/done` nel sidecar; al ripristino post-crash ogni segmento arretra di 2MB per coprire i write non ancora su disco.

## Sviluppo su Mac

Engine e UI girano anche su macOS (`cargo run` in `app/`), senza tray né integrazione Chrome. Test senza estensione:

```bash
echo '{"url":"https://proof.ovh.net/files/100Mb.dat"}' | nc 127.0.0.1 48666
```

Comandi TCP: `{"url":...,"cookies":...}` accoda un download, `{"cmd":"resume_all"}` riprende tutti i download in pausa/falliti.

## Limiti

Solo Chrome per ora (Firefox usa lo stesso protocollo native messaging, facile da aggiungere). Download generati da POST o con token one-shot possono fallire: l'estensione li rimanda a Chrome, oppure metti l'estensione in OFF. Niente limiti di banda.

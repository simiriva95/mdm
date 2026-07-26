# MDM — Mini Download Manager

Download manager minimale per Windows: intercetta i download grandi da Chrome e li scarica con più connessioni parallele. Un binario Rust + un'estensione Chrome minuscola. Zero bloat.

## Features

- **Fino a 8 connessioni parallele** (HTTP Range) con mappa segmenti live stile torrent nella barra di progresso
- **Pausa / resume dal punto esatto**, anche dopo crash, riavvio del PC o kill dell'app: lo stato vive in un sidecar `.mdm.json` accanto al `.part`, salvato ogni 2 secondi
- **Integrità verificata**: `If-Range` su ETag/Last-Modified. Se il file cambia sul server a metà download (CDN, resume dopo giorni) il download si ferma con un errore chiaro invece di consegnare un file cucito da due versioni
- **Coda**: solo N download partono insieme, gli altri aspettano il turno in ordine di arrivo
- **Limite di banda** globale, regolabile a caldo
- **Retry automatico** con backoff; i link morti (4xx) falliscono subito senza insistere
- **Cronologia** persistente con `[ open ]` e `[ ridownload ]`
- **Non serve Chrome**: campo per incollare un URL, drag&drop nella finestra, guardia appunti opzionale
- **UI "desktop di terminali"**: finestre navy con chrome `[X][+][_]`, gauge pastello, sparkline di rete, console log
- File piccoli restano in Chrome (soglia configurabile / estensioni note); click sull'icona estensione = ON/OFF, tasto destro su un link = "Scarica con MDM"
- Cookie + referer inoltrati: funzionano anche i download dietro login
- Notifica di sistema a fine download, tooltip del tray con velocità, progresso nel titolo della finestra
- Se l'app non risponde, il download torna automaticamente a Chrome: mai perso

## Configurazione

Tab **Settings** nell'app (o `%LOCALAPPDATA%\MDM\config.json`): cartella di destinazione, connessioni per download, download simultanei, limite di banda, soglia dell'estensione, retry, notifiche, guardia appunti, avvio con Windows.

La soglia è condivisa: l'estensione la chiede all'app all'avvio e ogni 5 minuti, con fallback al valore della sua pagina opzioni (`chrome://extensions` → MDM → Dettagli → Opzioni estensione), dove si può anche impostare una blocklist di domini.

Altri file in `%LOCALAPPDATA%\MDM`: `mdm.log` (log completo, ruotato a 2MB — bottone `[ apri log ]` nella tab Console) e `history.jsonl` (cronologia).

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

Un solo binario, due modalità. Resume: ogni segmento salva nel sidecar l'offset **flushed**, cioè i byte davvero usciti dal buffer di scrittura — quello è il punto da cui si riparte, senza arretramenti alla cieca. Il flush è forzato ogni 4MB, quindi un crash costa al massimo quello.

## Sviluppo su Mac

Engine e UI girano anche su macOS (`cargo run` in `app/`), senza tray né integrazione Chrome. Test senza estensione:

```bash
echo '{"url":"https://proof.ovh.net/files/100Mb.dat"}' | nc 127.0.0.1 48666
```

Comandi TCP: `{"url":...,"cookies":...}` accoda un download, `{"cmd":"resume_all"}` riprende tutti i download in pausa/falliti, `{"cmd":"config"}` restituisce la soglia corrente (`{"ok":true,"sizeThresholdMb":10}`).

## Test

```
cd app
cargo test
```

Oltre agli unitari sui puri, `tests/engine_http.rs` pilota l'engine contro un server HTTP finto in-process (nessuna dipendenza extra): download segmentato byte-esatto, fallback senza Range, 429 con e senza `Retry-After`, stream troncato a metà, pausa/resume, ripristino da sidecar dopo un crash, ETag cambiato sul server, coda con limite 1, link morto.

## Limiti

Solo Chrome per ora (Firefox usa lo stesso protocollo native messaging, facile da aggiungere). Download generati da POST o con token one-shot possono fallire: l'estensione li rimanda a Chrome, oppure metti l'estensione in OFF. L'estensione va caricata come "non pacchettizzata" (non è sul Web Store), quindi serve la modalità sviluppatore di Chrome.

const HOST = 'com.sriva.downloader';
// soglia di default; l'app è l'autorità e la sovrascrive appena risponde
let sizeThreshold = 10 * 1024 * 1024;

/// Chiede la soglia all'app. Se l'app è spenta non la avvia: si tiene l'ultimo
/// valore noto (persistito, così sopravvive al riciclo del service worker).
function refreshConfig() {
  chrome.runtime.sendNativeMessage(HOST, { cmd: 'config' }, (resp) => {
    void chrome.runtime.lastError; // app spenta: normale, si riprova dopo
    const mb = resp && resp.ok && Number(resp.sizeThresholdMb);
    if (mb > 0) {
      sizeThreshold = mb * 1024 * 1024;
      chrome.storage.local.set({ sizeThreshold });
    }
  });
}

let blocklist = [];

chrome.storage.local.get({ sizeThreshold: null, blocklist: [] }, (v) => {
  if (v.sizeThreshold > 0) sizeThreshold = v.sizeThreshold;
  blocklist = v.blocklist || [];
  refreshConfig();
});

// la pagina opzioni scrive in storage: recepiamo senza aspettare il riavvio
chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== 'local') return;
  if (changes.blocklist) blocklist = changes.blocklist.newValue || [];
  if (changes.sizeThreshold && changes.sizeThreshold.newValue > 0) {
    sizeThreshold = changes.sizeThreshold.newValue;
  }
  if (changes.enabled) {
    enabled = changes.enabled.newValue;
    updateBadge();
  }
});

function isBlocked(url) {
  if (!blocklist.length) return false;
  let host;
  try {
    host = new URL(url).hostname.toLowerCase();
  } catch {
    return false;
  }
  return blocklist.some((d) => host === d || host.endsWith(`.${d}`));
}
const BIG_EXTS = new Set([
  'zip', '7z', 'rar', 'gz', 'bz2', 'xz', 'tar', 'iso', 'img', 'bin',
  'exe', 'msi', 'dmg', 'pkg', 'deb', 'rpm', 'appimage',
  'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'wav',
]);

// URL già restituiti a Chrome dopo un handoff fallito. Senza questa memoria il
// download di fallback verrebbe intercettato di nuovo -> cancel -> handoff ->
// fallimento -> loop infinito con l'app spenta.
const HANDBACK_TTL = 60 * 1000;
const handedBack = new Map(); // url -> scadenza (ms)

// il service worker MV3 può essere ucciso in qualsiasi momento: senza questo
// ripristino la memoria si azzererebbe e il loop tornerebbe possibile
chrome.storage.session.get({ handedBack: {} }, (v) => {
  for (const [url, until] of Object.entries(v.handedBack || {})) handedBack.set(url, until);
});

function persistHandedBack() {
  chrome.storage.session.set({ handedBack: Object.fromEntries(handedBack) });
}

function markHandedBack(url) {
  handedBack.set(url, Date.now() + HANDBACK_TTL);
  persistHandedBack();
}

function wasHandedBack(url) {
  const until = handedBack.get(url);
  if (until === undefined) return false;
  if (Date.now() > until) {
    handedBack.delete(url);
    return false;
  }
  return true;
}

let enabled = true;
let failed = false; // ultimo handoff fallito: badge in stato errore
chrome.storage.local.get({ enabled: true }, (v) => { enabled = v.enabled; updateBadge(); });

chrome.action.onClicked.addListener(() => {
  enabled = !enabled;
  failed = false;
  chrome.storage.local.set({ enabled });
  updateBadge();
});

function updateBadge() {
  if (failed) {
    chrome.action.setBadgeText({ text: 'ERR' });
    chrome.action.setBadgeBackgroundColor({ color: '#d32f2f' });
    return;
  }
  chrome.action.setBadgeText({ text: enabled ? 'ON' : 'OFF' });
  chrome.action.setBadgeBackgroundColor({ color: enabled ? '#00c853' : '#9e9e9e' });
}

function basename(p) {
  return (p || '').split(/[\\/]/).pop();
}

function fileExt(name) {
  const i = name.lastIndexOf('.');
  return i > 0 ? name.slice(i + 1).toLowerCase() : '';
}

function shouldIntercept(item) {
  if (!enabled) return false;
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url)) return false; // blob:, data:, file: restano in Chrome
  if (wasHandedBack(url)) return false; // è il nostro fallback: lascialo a Chrome
  if (isBlocked(url)) return false;
  if (item.totalBytes >= sizeThreshold) return true;
  // dimensione ignota: decide l'estensione del file
  if (item.totalBytes <= 0) {
    const name = basename(item.filename) || basename(new URL(url).pathname);
    return BIG_EXTS.has(fileExt(name));
  }
  return false;
}

chrome.downloads.onDeterminingFilename.addListener((item) => {
  if (!shouldIntercept(item)) return;
  chrome.downloads.cancel(item.id, () => {
    void chrome.runtime.lastError; // già finito/cancellato: ignora
    chrome.downloads.erase({ id: item.id });
  });
  handOff(item);
});

// "Scarica con MDM" sui link: bypassa soglia e blocklist, l'utente l'ha chiesto
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: 'mdm-download',
    title: 'Scarica con MDM',
    contexts: ['link', 'video', 'audio', 'image'],
  });
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== 'mdm-download') return;
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/i.test(url)) return;
  handOff({ finalUrl: url, filename: '', referrer: (tab && tab.url) || info.pageUrl || '' });
});

async function handOff(item) {
  const url = item.finalUrl || item.url;
  let cookieHeader = '';
  try {
    const cookies = await chrome.cookies.getAll({ url });
    cookieHeader = cookies.map((c) => `${c.name}=${c.value}`).join('; ');
  } catch (e) { /* senza cookie: la maggior parte dei download funziona lo stesso */ }

  const msg = {
    url,
    filename: basename(item.filename),
    referrer: item.referrer || '',
    cookies: cookieHeader,
    userAgent: navigator.userAgent,
  };

  chrome.runtime.sendNativeMessage(HOST, msg, (resp) => {
    if (chrome.runtime.lastError || !resp || !resp.ok) {
      // app non installata o in errore: mai perdere il download, torna a Chrome.
      // markHandedBack PRIMA di rilanciare, altrimenti lo ri-intercettiamo.
      console.warn('MDM handoff fallito:', chrome.runtime.lastError?.message, resp);
      markHandedBack(url);
      failed = true;
      updateBadge();
      chrome.downloads.download({ url });
      return;
    }
    if (failed) {
      failed = false;
      updateBadge();
    }
  });
}

// pulizia delle voci scadute: la Map non deve crescere per sempre nel worker
chrome.alarms.create('mdm-gc', { periodInMinutes: 5 });
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name !== 'mdm-gc') return;
  const now = Date.now();
  for (const [url, until] of handedBack) {
    if (now > until) handedBack.delete(url);
  }
  persistHandedBack();
  refreshConfig(); // la soglia può essere cambiata dalla tab Settings
});

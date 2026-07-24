const HOST = 'com.sriva.downloader';
const SIZE_THRESHOLD = 10 * 1024 * 1024; // 10 MB
const BIG_EXTS = new Set([
  'zip', '7z', 'rar', 'gz', 'bz2', 'xz', 'tar', 'iso', 'img', 'bin',
  'exe', 'msi', 'dmg', 'pkg', 'deb', 'rpm', 'appimage',
  'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'wav',
]);

let enabled = true;
chrome.storage.local.get({ enabled: true }, (v) => { enabled = v.enabled; updateBadge(); });

chrome.action.onClicked.addListener(() => {
  enabled = !enabled;
  chrome.storage.local.set({ enabled });
  updateBadge();
});

function updateBadge() {
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
  if (item.totalBytes >= SIZE_THRESHOLD) return true;
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
      // app non installata o in errore: mai perdere il download, torna a Chrome
      console.warn('MDM handoff fallito:', chrome.runtime.lastError?.message, resp);
      chrome.downloads.download({ url });
    }
  });
}

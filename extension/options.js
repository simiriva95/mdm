const DEFAULTS = { enabled: true, sizeThreshold: 10 * 1024 * 1024, blocklist: [] };

const $ = (id) => document.getElementById(id);

chrome.storage.local.get(DEFAULTS, (v) => {
  $('enabled').checked = v.enabled;
  $('threshold').value = Math.round(v.sizeThreshold / (1024 * 1024));
  $('blocklist').value = (v.blocklist || []).join('\n');
});

$('save').addEventListener('click', () => {
  const mb = Math.max(1, Number($('threshold').value) || 10);
  const blocklist = $('blocklist').value
    .split('\n')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);

  chrome.storage.local.set({
    enabled: $('enabled').checked,
    sizeThreshold: mb * 1024 * 1024,
    blocklist,
  }, () => {
    $('saved').textContent = 'salvato';
    setTimeout(() => { $('saved').textContent = ''; }, 1500);
  });
});

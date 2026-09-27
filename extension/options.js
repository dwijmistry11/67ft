import { PROFILES, loadSettings } from './profiles.js';
import { checkHealth, normalizeServer } from './server.js';

const form = document.getElementById('form');
const status = document.getElementById('status');

function renderProfiles(selected) {
  document.getElementById('profiles').innerHTML = Object.entries(PROFILES)
    .map(
      ([id, p]) => `
        <div class="profile">
          <input type="radio" name="profile" id="p-${id}" value="${id}"
                 ${id === selected ? 'checked' : ''}>
          <label for="p-${id}">
            ${p.label}
            <span class="blurb">${p.blurb}</span>
          </label>
        </div>`,
    )
    .join('');
}

async function restore() {
  const s = await loadSettings();
  renderProfiles(s.profile);
  document.getElementById('customUserAgent').value = s.customUserAgent;
  document.getElementById('blockScripts').checked = s.blockScripts;
  document.getElementById('cleanOverlays').checked = s.cleanOverlays;
  document.getElementById('autoSites').value = s.autoSites.join('\n');
  document.querySelector(`input[name=mode][value="${s.mode}"]`).checked = true;
  document.getElementById('serverUrl').value = s.serverUrl;
  reportServer(s.serverUrl);
}

/** Say whether the configured server is actually there, rather than just storing it. */
async function reportServer(url) {
  const el = document.getElementById('server-status');
  if (!url) {
    el.textContent = 'Leave empty and the popup will look for one on the usual names.';
    return;
  }
  el.textContent = 'checking\u2026';
  const h = await checkHealth(url);
  el.textContent = h.ok ? `online \u00b7 ${h.ms} ms` : `not reachable \u2014 ${h.error}`;
}

form.addEventListener('submit', async (e) => {
  e.preventDefault();

  const serverUrl = normalizeServer(document.getElementById('serverUrl').value);

  await chrome.storage.sync.set({
    mode: form.querySelector('input[name=mode]:checked').value,
    serverUrl,
    profile: form.querySelector('input[name=profile]:checked').value,
    customUserAgent: document.getElementById('customUserAgent').value.trim(),
    blockScripts: document.getElementById('blockScripts').checked,
    cleanOverlays: document.getElementById('cleanOverlays').checked,
    autoSites: document
      .getElementById('autoSites')
      .value.split('\n')
      .map((line) => line.trim())
      // Paste a URL in and keep only the host, so the list works either way.
      .map((line) => line.replace(/^https?:\/\//, '').replace(/[/?#].*$/, ''))
      .filter(Boolean),
  });

  // The background page reconciles its rules from the storage change; re-read
  // so the textarea shows the list as it was actually stored.
  await restore();
  status.classList.add('show');
  setTimeout(() => status.classList.remove('show'), 1400);
});

restore();

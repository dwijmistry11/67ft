import { PROFILES, loadSettings } from './profiles.js';

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
}

form.addEventListener('submit', async (e) => {
  e.preventDefault();

  await chrome.storage.sync.set({
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

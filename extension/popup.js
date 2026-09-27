import { loadSettings, ruleDomain } from './profiles.js';
import {
  checkHealth, discoverServer, isProxiedPage, normalizeServer,
  testFetch, unproxyUrl,
} from './server.js';

const $ = (id) => document.getElementById(id);
const state = {
  settings: null, tab: null, health: null,
  lastCheck: null, lastTest: null,
  // Whether the disguise is currently applied to this tab.
  localOn: false,
};

/* ------------------------------------------------------------------ helpers */

function tabHost() {
  try {
    return new URL(state.tab.url).hostname;
  } catch {
    return null;
  }
}

/** The article this popup acts on — the real one, even when already proxied. */
function targetUrl() {
  const { serverUrl } = state.settings;
  return unproxyUrl(serverUrl, state.tab.url) || state.tab.url;
}

/**
 * Is local mode actually in force for this tab right now?
 *
 * Read from the rules themselves rather than from a stored flag: the session
 * rule is the thing that does the work, so it is the thing worth asking.
 */
async function readLocalState() {
  const host = tabHost();
  const rules = await chrome.declarativeNetRequest.getSessionRules();
  if (rules.some((r) => r.condition?.tabIds?.includes(state.tab.id))) {
    state.localOn = true;
    return;
  }
  const domain = host && ruleDomain(host);
  state.localOn = !!domain
    && state.settings.autoSites.some((s) => ruleDomain(s) === domain);
}

function row(key, value) {
  const tr = document.createElement('tr');
  tr.innerHTML = `<td class="k"></td><td></td>`;
  tr.children[0].textContent = key;
  tr.children[1].textContent = value;
  return tr;
}

/**
 * Rebuild the diagnostics table from `state`.
 *
 * Nothing here reads a previous value back out of the DOM: the rows are
 * destroyed on every repaint, so anything they held has to live in state or it
 * is gone by the time the next line runs.
 */
function paintDiag() {
  const table = $('diag');
  table.innerHTML = '';
  table.appendChild(row('last check', state.lastCheck || '\u2014'));

  const t = state.lastTest;
  if (!t) return;
  if (t.status) {
    table.appendChild(row('status', `HTTP ${t.status}`));
    table.appendChild(row('size', `${(t.bytes / 1024).toFixed(0)} KB`));
    table.appendChild(row('time', `${t.ms} ms`));
    // "hit" means the server answered from memory and never touched the site.
    table.appendChild(row('server cache', t.cache));
  } else {
    table.appendChild(row('result', t.error));
  }
}

async function save(patch) {
  Object.assign(state.settings, patch);
  await chrome.storage.sync.set(patch);
}

/* ------------------------------------------------------------ server status */

function paintServer() {
  const { serverUrl } = state.settings;
  const h = state.health;

  if (!serverUrl) {
    $('dot').className = 'dot';
    $('server-host').textContent = 'not configured';
    $('server-detail').textContent = 'Local mode works without one.';
    $('server-setup').hidden = false;
    return;
  }

  $('server-host').textContent = serverUrl.replace(/^https?:\/\//, '');
  if (!h) {
    $('dot').className = 'dot probing';
    $('server-detail').textContent = 'checking…';
  } else if (h.ok) {
    $('dot').className = 'dot ok';
    $('server-detail').textContent = `online · ${h.ms} ms`;
    $('server-setup').hidden = true;
  } else {
    $('dot').className = 'dot bad';
    $('server-detail').textContent = h.error;
    // Only offer the setup box once we know the configured one is not there.
    $('server-setup').hidden = false;
  }
  $('server-input').value = serverUrl;
  $('test').disabled = !(h && h.ok);
}

async function refreshHealth() {
  state.health = null;
  paintServer();
  state.health = await checkHealth(state.settings.serverUrl);
  state.lastCheck = new Date().toLocaleTimeString();
  paintDiag();
  paintServer();
}

/* ---------------------------------------------------------------- this page */

function paintPage() {
  const host = tabHost();
  const { mode, serverUrl, autoSites } = state.settings;
  const proxied = isProxiedPage(serverUrl, state.tab.url);

  $('page-host').firstChild.textContent = proxied
    ? new URL(targetUrl()).hostname
    : host || 'no page';

  const pill = $('page-pill');

  $('restore').hidden = !proxied;

  const go = $('go');
  if (state.localOn) {
    // The off switch. Its absence was the whole problem: a site that refuses
    // the disguise left you with no way back to the page as it really is.
    go.textContent = 'Turn off for this tab';
    go.className = 'action secondary';
  } else {
    go.textContent = mode === 'server' ? 'Read via the server' : 'Read as a crawler';
    go.className = 'action';
  }
  go.disabled = !host;

  pill.textContent = proxied ? 'via server' : state.localOn ? 'as crawler' : 'direct';
  pill.className = proxied || state.localOn ? 'pill' : 'pill off';

  $('auto-host').textContent = host ? ruleDomain(host) : 'this site';
  $('auto').checked = !!host && autoSites.some((s) => ruleDomain(s) === ruleDomain(host));

  for (const el of document.querySelectorAll('.mode')) {
    el.classList.toggle('on', el.dataset.mode === mode);
  }
}

/* ------------------------------------------------------------------ actions */

function showNotice(text) {
  $('notice').textContent = text;
  $('notice').hidden = !text;
}

async function go() {
  if (state.localOn) {
    await chrome.runtime.sendMessage({ type: 'disable-local', tabId: state.tab.id });
    window.close();
    return;
  }
  if (state.settings.mode === 'server') {
    if (!state.health?.ok) return;
    await chrome.runtime.sendMessage({ type: 'go-server', tabId: state.tab.id });
  } else {
    await chrome.runtime.sendMessage({ type: 'enable-local', tabId: state.tab.id });
  }
  window.close();
}

async function runTest() {
  const btn = $('test');
  btn.disabled = true;
  btn.textContent = 'fetching…';

  state.lastTest = await testFetch(state.settings.serverUrl, targetUrl());
  paintDiag();

  const r = state.lastTest;
  btn.textContent = 'Test again';
  btn.disabled = false;
  $('test-hint').textContent = r.cache === 'hit'
    ? 'Served from the server’s cache — the publisher saw nothing.'
    : 'Fetches without navigating, and reports what the server did.';
}

/* -------------------------------------------------------------------- wiring */

async function init() {
  state.settings = await loadSettings();
  [state.tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  await readLocalState();

  paintPage();
  paintServer();

  const { lastFallback } = await chrome.storage.session.get('lastFallback');
  if (lastFallback && lastFallback.host === tabHost()
      && Date.now() - lastFallback.at < 120_000) {
    showNotice(lastFallback.route === 'server'
      ? 'This looks like a bot wall rather than the article. The server is '
        + 'blocked here — the site is often readable in this browser as it is.'
      : 'This looks like a bot wall rather than the article. This site refuses '
        + 'the crawler disguise — turn it off above to see it normally.');
  }

  // A server that was never configured is worth one quiet probe: the common
  // case is that one is running on a name the defaults already know.
  if (!state.settings.serverUrl) {
    const found = await discoverServer();
    if (found) {
      await save({ serverUrl: found.server });
      state.health = found;
      state.lastCheck = new Date().toLocaleTimeString();
      paintDiag();
      paintServer();
      paintPage();
      return;
    }
    paintServer();
    return;
  }
  await refreshHealth();
}

$('go').addEventListener('click', go);
$('test').addEventListener('click', runTest);
$('recheck').addEventListener('click', refreshHealth);
$('options').addEventListener('click', () => chrome.runtime.openOptionsPage());

$('restore').addEventListener('click', async () => {
  await chrome.tabs.update(state.tab.id, { url: targetUrl() });
  window.close();
});

for (const el of document.querySelectorAll('.mode')) {
  el.addEventListener('click', async () => {
    await save({ mode: el.dataset.mode });
    paintPage();
    if (el.dataset.mode === 'server' && !state.health) await refreshHealth();
  });
}

$('auto').addEventListener('change', async (e) => {
  const host = tabHost();
  if (!host) return;
  const domain = ruleDomain(host);
  const autoSites = e.target.checked
    ? [...state.settings.autoSites, domain]
    : state.settings.autoSites.filter((s) => ruleDomain(s) !== domain);
  await save({ autoSites });
});

$('server-save').addEventListener('click', async () => {
  await save({ serverUrl: normalizeServer($('server-input').value) });
  await refreshHealth();
  paintPage();
});

$('server-discover').addEventListener('click', async () => {
  $('server-detail').textContent = 'looking…';
  $('dot').className = 'dot probing';
  const found = await discoverServer([normalizeServer($('server-input').value)]);
  if (found) {
    await save({ serverUrl: found.server });
    state.health = found;
  } else {
    state.health = { ok: false, error: 'nothing answered' };
  }
  state.lastCheck = new Date().toLocaleTimeString();
  paintDiag();
  paintServer();
  paintPage();
});

init();

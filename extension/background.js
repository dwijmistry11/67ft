import { loadSettings, actionFor, ruleDomain } from './profiles.js';

// Two populations of rules, because they answer different questions.
//
// Session rules carry a `tabIds` condition — the only kind that can — and back
// the manual toggle: this tab, starting now. They die with the browser, which
// is the right lifetime for "just this once".
//
// Dynamic rules are domain-scoped and persistent, and back the auto-site list.
// They matter most on the very first request of a navigation, before any
// listener of ours could have run, so a site you always want disguised is never
// visited undisguised.
const SESSION_ID = (tabId) => tabId + 1; // rule ids must be >= 1
const AUTO_ID_BASE = 1_000_000;

/** Is 67ft active for this tab — by manual toggle, or because the site is on the auto list? */
async function activeFor(tab) {
  if (!tab?.url || !/^https?:/.test(tab.url)) return false;

  const session = await chrome.declarativeNetRequest.getSessionRules();
  if (session.some((r) => r.id === SESSION_ID(tab.id))) return true;

  const { autoSites } = await loadSettings();
  return matchesAutoSite(new URL(tab.url).hostname, autoSites);
}

function matchesAutoSite(hostname, autoSites) {
  const host = hostname.toLowerCase();
  return autoSites.some((entry) => {
    const domain = ruleDomain(entry.trim().toLowerCase());
    return domain && (host === domain || host.endsWith(`.${domain}`));
  });
}

// ---------------------------------------------------------------- manual toggle

async function enableTab(tab) {
  const action = actionFor(await loadSettings());

  await chrome.declarativeNetRequest.updateSessionRules({
    removeRuleIds: [SESSION_ID(tab.id)],
    addRules: [
      {
        id: SESSION_ID(tab.id),
        priority: 1,
        action,
        condition: {
          tabIds: [tab.id],
          requestDomains: [ruleDomain(new URL(tab.url).hostname)],
          // The document only. Images, CSS and fonts keep the real user agent
          // and the site's cookies, so they load exactly as they normally do.
          resourceTypes: ['main_frame'],
        },
      },
    ],
  });

  // bypassCache so the disguised request actually reaches the origin rather
  // than being answered from the copy fetched as yourself a moment ago.
  await chrome.tabs.reload(tab.id, { bypassCache: true });
}

async function disableTab(tabId) {
  await chrome.declarativeNetRequest.updateSessionRules({
    removeRuleIds: [SESSION_ID(tabId)],
  });
  await chrome.action.setBadgeText({ tabId, text: '' });
  await chrome.tabs.reload(tabId, { bypassCache: true });
}

async function toggleTab(tab) {
  if (!tab?.url || !/^https?:/.test(tab.url)) return;

  const session = await chrome.declarativeNetRequest.getSessionRules();
  if (session.some((r) => r.id === SESSION_ID(tab.id))) {
    await disableTab(tab.id);
  } else {
    await enableTab(tab);
  }
}

// ------------------------------------------------------------------ auto sites

/**
 * Rebuild every auto-site rule from the stored list.
 *
 * Rewriting the whole set on each change keeps rule ids a pure function of list
 * position, so there is no id bookkeeping to drift out of sync with storage.
 */
async function syncAutoRules() {
  const settings = await loadSettings();
  const { autoSites } = settings;
  const action = actionFor(settings);

  const existing = await chrome.declarativeNetRequest.getDynamicRules();
  const domains = [...new Set(
    autoSites.map((s) => ruleDomain(s.trim().toLowerCase())).filter(Boolean),
  )];

  await chrome.declarativeNetRequest.updateDynamicRules({
    removeRuleIds: existing.map((r) => r.id),
    addRules: domains.map((domain, i) => ({
      id: AUTO_ID_BASE + i,
      priority: 1,
      action,
      condition: { requestDomains: [domain], resourceTypes: ['main_frame'] },
    })),
  });
}

async function toggleAutoSite(tab) {
  const settings = await loadSettings();
  const domain = ruleDomain(new URL(tab.url).hostname.toLowerCase());
  const autoSites = matchesAutoSite(new URL(tab.url).hostname, settings.autoSites)
    ? settings.autoSites.filter(
        (s) => ruleDomain(s.trim().toLowerCase()) !== domain,
      )
    : [...settings.autoSites, domain];

  await chrome.storage.sync.set({ autoSites });
  await syncAutoRules();
  await chrome.tabs.reload(tab.id, { bypassCache: true });
}

// -------------------------------------------------------------------- the page

/**
 * Run the reader pass over a loaded page.
 *
 * Content scripts execute in an isolated world, which the page's content
 * security policy does not govern — so this still runs on a document where we
 * have just forbidden every script.
 */
async function cleanPage(tabId) {
  const { cleanOverlays } = await loadSettings();
  if (!cleanOverlays) return;

  try {
    await chrome.scripting.insertCSS({ target: { tabId }, files: ['reader.css'] });
    await chrome.scripting.executeScript({ target: { tabId }, files: ['reader.js'] });
  } catch (e) {
    // Injection is refused on chrome:// pages, the Web Store, and PDFs. None of
    // those are articles, so there is nothing to recover from.
    console.debug('67ft: no reader pass for tab', tabId, e.message);
  }
}

chrome.tabs.onUpdated.addListener(async (tabId, info, tab) => {
  if (info.status !== 'complete') return;
  if (!(await activeFor(tab))) {
    await chrome.action.setBadgeText({ tabId, text: '' });
    return;
  }

  await chrome.action.setBadgeText({ tabId, text: 'ON' });
  await chrome.action.setBadgeBackgroundColor({ tabId, color: '#7c6af7' });
  await cleanPage(tabId);
});

// A recycled tab id would otherwise inherit the previous tab's disguise.
chrome.tabs.onRemoved.addListener((tabId) => {
  chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: [SESSION_ID(tabId)] });
});

// ------------------------------------------------------------------- the wiring

chrome.action.onClicked.addListener(toggleTab);

chrome.commands.onCommand.addListener(async (command) => {
  if (command !== 'toggle-tab') return;
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  await toggleTab(tab);
});

chrome.runtime.onInstalled.addListener(async () => {
  // An update or a reload re-runs this, and create() rejects a duplicate id.
  await chrome.contextMenus.removeAll();
  chrome.contextMenus.create({
    id: 'toggle-tab',
    title: '67ft this page',
    contexts: ['page', 'link'],
  });
  chrome.contextMenus.create({
    id: 'toggle-auto',
    title: 'Always 67ft this site',
    contexts: ['page', 'link'],
  });
  await syncAutoRules();
});

// Dynamic rules outlive the browser session but the settings behind them can be
// edited from another synced machine, so they are reconciled on every startup.
chrome.runtime.onStartup.addListener(syncAutoRules);

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (!tab?.url || !/^https?:/.test(tab.url)) return;
  if (info.menuItemId === 'toggle-tab') await toggleTab(tab);
  if (info.menuItemId === 'toggle-auto') await toggleAutoSite(tab);
});

// Changing the profile or the site list has to reach the rules already live.
chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== 'sync') return;
  if (['autoSites', 'profile', 'customUserAgent', 'blockScripts'].some((k) => k in changes)) {
    syncAutoRules();
  }
});

// Header profiles. Each one is the whole disguise: who we claim to be, plus
// everything about the real browser session that has to be withheld for the
// claim to be believable.
//
// Stripping `cookie` is not optional. A logged-out-but-metered reader is a
// state the publisher tracks in a cookie, and sending it alongside a Googlebot
// user agent is a contradiction the paywall resolves against you.

// Googlebot has rendered with an evergreen Chrome engine since 2019, and the
// version it reports tracks stable Chrome. An implausibly old one is itself a
// signal, so this is worth bumping occasionally.
const GOOGLEBOT_UA =
  'Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; Googlebot/2.1; ' +
  '+http://www.google.com/bot.html) Chrome/129.0.0.0 Safari/537.36';

const BINGBOT_UA =
  'Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; bingbot/2.0; ' +
  '+http://www.bing.com/bingbot.htm) Chrome/116.0.1938.76 Safari/537.36';

export const PROFILES = {
  googlebot: {
    label: 'Googlebot',
    blurb: 'Crawler user agent, no cookies, no referrer. The default.',
    headers: [
      { header: 'user-agent', operation: 'set', value: GOOGLEBOT_UA },
      { header: 'cookie', operation: 'remove' },
      { header: 'referer', operation: 'remove' },
      // Almost always overwritten by the publisher's CDN with your real
      // address. Kept for parity with the server build, where it is equally
      // decorative.
      { header: 'x-forwarded-for', operation: 'set', value: '66.249.66.1' },
    ],
  },

  bingbot: {
    label: 'Bingbot',
    blurb: 'For sites that allow Bing to index what they hide from Google.',
    headers: [
      { header: 'user-agent', operation: 'set', value: BINGBOT_UA },
      { header: 'cookie', operation: 'remove' },
      { header: 'referer', operation: 'remove' },
    ],
  },

  'google-referral': {
    label: 'Arrived from Google',
    blurb:
      'Your real browser, no cookies, referred by Google search. Works on ' +
      'metered sites that grant free reads to search traffic but ignore crawlers.',
    headers: [
      { header: 'cookie', operation: 'remove' },
      { header: 'referer', operation: 'set', value: 'https://www.google.com/' },
    ],
  },
};

export const DEFAULT_SETTINGS = {
  // 'local'  — headers and CSP rewritten here, nothing leaves this browser
  // 'server' — the tab is sent to a 67ft server, which fetches from its own
  //            address. The only mode that hides which machine is asking.
  mode: 'local',
  // Base URL of that server. Empty until set or discovered.
  serverUrl: '',

  profile: 'googlebot',
  // Replaces the profile's user agent when non-empty.
  customUserAgent: '',
  // The equivalent of the server build stripping every <script> element.
  blockScripts: true,
  // DOM cleanup pass: unwrap <noscript>, drop overlays, unclamp the article.
  cleanOverlays: true,
  // Hostnames that switch themselves on, one per line in the options page.
  autoSites: [],
};

/** Settings as stored, with defaults filled in for anything never set. */
export async function loadSettings() {
  const stored = await chrome.storage.sync.get(DEFAULT_SETTINGS);
  return { ...DEFAULT_SETTINGS, ...stored };
}

/**
 * The declarativeNetRequest `modifyHeaders` action a profile implies.
 *
 * `set` rather than `append` on the CSP: appending would intersect with the
 * site's own policy, which is stricter but relies on append being honoured for
 * response headers. `set` is documented for response headers and, since we are
 * replacing a policy that exists to protect scripts we are about to forbid
 * outright, loses nothing.
 */
export function actionFor(settings) {
  const profile = PROFILES[settings.profile] ?? PROFILES.googlebot;

  const requestHeaders = profile.headers.map((h) =>
    h.header === 'user-agent' && settings.customUserAgent
      ? { ...h, value: settings.customUserAgent }
      : h,
  );

  if (settings.customUserAgent && !requestHeaders.some((h) => h.header === 'user-agent')) {
    requestHeaders.push({
      header: 'user-agent',
      operation: 'set',
      value: settings.customUserAgent,
    });
  }

  // An empty array is not the same as an absent one here: declarativeNetRequest
  // rejects a modifyHeaders rule that carries a header list with nothing in it,
  // so the whole rule would fail to register whenever script blocking is off.
  const action = { type: 'modifyHeaders', requestHeaders };

  if (settings.blockScripts) {
    action.responseHeaders = [
      { header: 'content-security-policy', operation: 'set', value: "script-src 'none'" },
    ];
  }

  return action;
}

/**
 * The domain a rule should cover.
 *
 * `requestDomains` already matches subdomains, so dropping a leading `www.`
 * makes one rule cover both `www.site.com` and `site.com`. Nothing further is
 * stripped: walking up to a registrable domain needs a public suffix list, and
 * guessing wrong turns one rule into a rule for every site under `.co.uk`.
 */
export function ruleDomain(hostname) {
  return hostname.replace(/^www\./, '');
}

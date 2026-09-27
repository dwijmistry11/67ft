// The reader pass: everything that has to happen in the DOM rather than in a
// header. Runs in an isolated world, so the `script-src 'none'` policy we just
// imposed on the document does not apply to it.
(() => {
  if (window.__sixtysevenft) return; // re-injected on every load; do it once
  window.__sixtysevenft = true;

  // Text long enough that clamping it is a paywall truncating an article, not
  // a card component sized to its design.
  const ARTICLE_CHARS = 800;
  // Banners are short. A "gate" wrapping more text than this is the article.
  const BANNER_CHARS = 2000;

  const PAYWALL = new RegExp(
    [
      'paywall', 'pay-wall', 'piano-', 'tp-modal', 'tp-backdrop',
      'regwall', 'reg-wall', 'regi-?gate',
      'subscri\\w*-?(wall|modal|gate|overlay)',
      'meter(ed)?-?(wall|modal|limit)',
      'premium-?gate', 'article-?gate', 'content-?gate',
      'onetrust', 'gdpr', 'consent-?(banner|modal)', 'cookie-?(banner|consent|notice)',
      'newsletter-?(modal|popup)', 'interstitial', 'lightbox-?overlay',
    ].join('|'),
    'i',
  );

  /** Class, id and test-id together — paywall vendors label themselves in all three. */
  function labelOf(el) {
    const cls = typeof el.className === 'string' ? el.className : '';
    return `${el.id} ${cls} ${el.getAttribute('data-testid') || ''}`;
  }

  /**
   * Replace every <noscript> with the markup it holds.
   *
   * Blocking scripts via CSP is not the same as disabling scripting: the page
   * still counts as script-enabled, so <noscript> stays inert and its lazy-load
   * <img> fallbacks never become real images. This is what the server build's
   * noscript unwrapping does, for the same reason.
   */
  function unwrapNoscript() {
    for (const ns of document.querySelectorAll('noscript')) {
      const tpl = document.createElement('template');
      tpl.innerHTML = ns.textContent; // parsed inert, nothing runs
      // CSP would refuse these anyway; not carrying them across means the pass
      // is still safe if someone turns script blocking off.
      tpl.content.querySelectorAll('script').forEach((s) => s.remove());
      ns.replaceWith(tpl.content);
    }
  }

  /**
   * One walk of the document, deciding per element whether it is part of the
   * article or part of what is covering it.
   */
  function sweep() {
    if (!document.body) return 0;

    const vw = window.innerWidth;
    const vh = window.innerHeight;
    let removed = 0;

    for (const el of document.body.querySelectorAll('*')) {
      // A previous iteration may have removed this element's ancestor, and a
      // detached element has no computed style worth reading.
      if (!el.isConnected) continue;

      const style = getComputedStyle(el);
      if (style.display === 'none' || style.visibility === 'hidden') continue;

      const label = labelOf(el);
      const named = PAYWALL.test(label);

      // 1. Overlays. Either it covers the viewport, or it names itself.
      if (style.position === 'fixed' || style.position === 'sticky') {
        const r = el.getBoundingClientRect();
        const covering = r.width >= vw * 0.85 && r.height >= vh * 0.6;

        if (covering || (named && style.position === 'fixed')) {
          if (el.textContent.length < BANNER_CHARS) {
            el.remove();
            removed++;
            continue;
          }
          // It holds the article, so it is not a cover over the page — it is
          // the page, pinned to the viewport to stop you scrolling past the
          // cut. Unpin it rather than deleting the thing we came to read.
          el.style.setProperty('position', 'static', 'important');
          el.style.setProperty('height', 'auto', 'important');
          el.style.setProperty('overflow', 'visible', 'important');
        }
      }

      // 2. Named banners that sit in the flow rather than over it — the
      //    "subscribe to keep reading" block spliced into the article. Short,
      //    or it is the article itself and must be left alone.
      if (named && el.textContent.length < BANNER_CHARS) {
        el.remove();
        removed++;
        continue;
      }

      // 3. The article, clamped. A max-height with the overflow hidden and a
      //    fade mask over the cut is the most common paywall that never
      //    bothers to remove the text it is hiding.
      if (style.overflow !== 'visible' && el.textContent.length > ARTICLE_CHARS) {
        const maxHeight = parseFloat(style.maxHeight);
        const height = parseFloat(style.height);
        const clamped =
          (!Number.isNaN(maxHeight) && maxHeight < el.scrollHeight) ||
          (!Number.isNaN(height) && height + 1 < el.scrollHeight);

        if (clamped) {
          el.style.setProperty('max-height', 'none', 'important');
          el.style.setProperty('height', 'auto', 'important');
          el.style.setProperty('overflow', 'visible', 'important');
          el.style.setProperty('mask-image', 'none', 'important');
          el.style.setProperty('-webkit-mask-image', 'none', 'important');
        }
      }

      // 4. The other way of hiding text in place.
      if (style.filter.includes('blur')) {
        el.style.setProperty('filter', 'none', 'important');
        el.style.setProperty('-webkit-filter', 'none', 'important');
      }
    }

    return removed;
  }

  unwrapNoscript();
  const removed = sweep();

  // A second pass once layout has settled: web fonts and late images change
  // element heights, which is exactly what the clamp test measures. Nothing
  // else will move the DOM — every script on the page is dead.
  setTimeout(sweep, 600);

  console.debug(`67ft: reader pass complete, ${removed} overlay(s) removed`);
})();

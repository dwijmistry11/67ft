#!/usr/bin/env python3
"""Fetch a list of sites through a 67ft server and classify what came back.

    ./tools/check-sites.py --server http://67ft.lan:8080 --list sites.txt

HTTP 200 is not the question. A paywall answers 200 with a teaser, a bot wall
answers 200 with a challenge page, and a single-page app answers 200 with an
empty shell — so each response is judged on what is actually in the body.
"""
import argparse, concurrent.futures as cf, json, re, sys, time
import urllib.parse, urllib.request, urllib.error

TAGS = re.compile(r'(?is)<(script|style|noscript)[^>]*>.*?</\1>|<[^>]+>')
WS = re.compile(r'\s+')

# Matched against the body, lowercased. Order matters: the first hit wins, so
# the most specific diagnosis is listed first.
SIGNALS = [
    ('challenge', ['just a moment', 'cf-browser-verification', 'checking your browser',
                   '__cf_chl', 'attention required', 'enable javascript and cookies to continue',
                   'ddos protection by', 'ray id']),
    ('captcha',   ['captcha', 'unusual traffic', 'are you a robot', 'verify you are human',
                   'press and hold']),
    ('login',     ['sign in to continue', 'log in to continue', 'please log in',
                   'create an account to continue']),
    ('paywall',   ['subscribe to continue', 'already a subscriber', 'subscribers only',
                   'to continue reading', 'this article is for subscribers',
                   'you have reached your limit', 'free articles remaining']),
    ('needs_js',  ['you need to enable javascript', 'please enable javascript',
                   'javascript is required', "doesn't work properly without javascript"]),
]


def visible_text(html):
    return WS.sub(' ', TAGS.sub(' ', html)).strip()


def classify(status, body, err):
    if err:
        return err, 0
    text = visible_text(body)
    n = len(text)
    low = body[:200000].lower()

    for name, needles in SIGNALS:
        if any(s in low for s in needles):
            return name, n

    if status in (401, 403):
        return 'blocked', n
    if status == 404:
        return 'not_found', n
    if status == 429:
        return 'rate_limited', n
    if status >= 500:
        return 'server_error', n
    if status != 200:
        return f'http_{status}', n

    # 200 with nothing to read is a client-rendered page: the crawler's copy
    # held no article, so there was never anything for the proxy to deliver.
    if n < 500:
        return 'empty_shell', n
    if n < 1500:
        return 'thin', n
    return 'ok', n


# A plain Chrome, for the control run. Without a baseline a failure list says
# nothing: most of what a proxy "breaks" was never reachable to begin with.
CHROME_UA = ('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 '
             '(KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36')


def fetch(server, target, timeout, direct=False):
    if direct:
        url = target
        headers = {'Accept-Encoding': 'identity', 'User-Agent': CHROME_UA,
                   'Accept': 'text/html,application/xhtml+xml,*/*;q=0.8'}
    else:
        url = f"{server.rstrip('/')}/{urllib.parse.quote(target, safe='')}"
        headers = {'Accept-Encoding': 'identity'}

    started = time.time()
    req = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode('utf-8', 'replace')
            status, cache = r.status, r.headers.get('x-67ft-cache', '-')
            err = None
    except urllib.error.HTTPError as e:
        body = e.read().decode('utf-8', 'replace')
        status, cache, err = e.code, e.headers.get('x-67ft-cache', '-'), None
    except urllib.error.URLError as e:
        reason = str(e.reason)
        body, status, cache = '', 0, '-'
        err = 'timeout' if 'timed out' in reason.lower() else 'unreachable'
    except Exception as e:
        body, status, cache, err = '', 0, '-', type(e).__name__.lower()

    verdict, chars = classify(status, body, err)
    return {'target': target, 'status': status, 'verdict': verdict, 'chars': chars,
            'bytes': len(body), 'cache': cache, 'ms': int((time.time() - started) * 1000)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--server', default='http://67ft.lan:8080')
    ap.add_argument('--list', required=True, help='one URL or bare domain per line')
    ap.add_argument('--workers', type=int, default=8)
    ap.add_argument('--timeout', type=int, default=30)
    ap.add_argument('--out', help='write full JSON results here')
    ap.add_argument('--direct', action='store_true',
                    help='bypass the server and fetch as a plain browser, for comparison')
    a = ap.parse_args()

    targets = []
    for line in open(a.list):
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        targets.append(line if line.startswith('http') else f'https://{line}')

    via = 'DIRECT (control, plain Chrome)' if a.direct else a.server
    print(f'{len(targets)} targets via {via}, {a.workers} at a time\n', file=sys.stderr)
    results = []
    with cf.ThreadPoolExecutor(a.workers) as pool:
        futures = {pool.submit(fetch, a.server, t, a.timeout, a.direct): t for t in targets}
        for i, fut in enumerate(cf.as_completed(futures), 1):
            r = fut.result()
            results.append(r)
            if i % 25 == 0:
                print(f'  {i}/{len(targets)}', file=sys.stderr)

    counts = {}
    for r in results:
        counts[r['verdict']] = counts.get(r['verdict'], 0) + 1

    total = len(results)
    print('\n=== verdicts ===')
    for v, c in sorted(counts.items(), key=lambda kv: -kv[1]):
        print(f'  {v:<14} {c:>4}  {c / total * 100:>5.1f}%')

    ok = counts.get('ok', 0) + counts.get('thin', 0)
    print(f'\n  readable       {ok:>4}  {ok / total * 100:>5.1f}%')

    if a.out:
        json.dump(results, open(a.out, 'w'), indent=1)
        print(f'\nfull results -> {a.out}')


if __name__ == '__main__':
    main()

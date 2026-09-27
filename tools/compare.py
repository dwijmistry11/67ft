#!/usr/bin/env python3
"""Compare a proxied run against the direct control run.

    ./tools/compare.py r-proxy.json r-direct.json

The number that matters is not how many sites fail through the proxy, but how
many fail through it that a plain browser could have read.
"""
import json, sys
from collections import Counter

READABLE = {'ok', 'thin'}

proxy = {r['target']: r for r in json.load(open(sys.argv[1]))}
direct = {r['target']: r for r in json.load(open(sys.argv[2]))}
shared = sorted(set(proxy) & set(direct))

buckets = Counter()
regressions, rescues = [], []

for t in shared:
    p, d = proxy[t]['verdict'] in READABLE, direct[t]['verdict'] in READABLE
    if p and d:
        buckets['both readable'] += 1
    elif p and not d:
        buckets['proxy rescued it'] += 1
        rescues.append((t, direct[t]['verdict'], proxy[t]['verdict']))
    elif d and not p:
        buckets['PROXY BROKE IT'] += 1
        regressions.append((t, direct[t]['verdict'], proxy[t]['verdict']))
    else:
        buckets['neither readable'] += 1

n = len(shared)
print(f'=== {n} sites, proxy vs plain browser ===')
for k, v in buckets.most_common():
    print(f'  {k:<20} {v:>4}  {v/n*100:>5.1f}%')

def show(title, rows, limit=40):
    if not rows:
        return
    print(f'\n=== {title} ({len(rows)}) ===')
    print(f'  {"site":<38} {"direct":<14} {"via 67ft"}')
    for t, dv, pv in rows[:limit]:
        print(f'  {t.replace("https://",""):<38} {dv:<14} {pv}')
    if len(rows) > limit:
        print(f'  ... and {len(rows)-limit} more')

show('regressions: readable directly, not through 67ft', regressions)
show('rescues: blocked directly, readable through 67ft', rescues)

# What the failures actually are, for the sites nobody can read.
both_fail = Counter(proxy[t]['verdict'] for t in shared
                    if proxy[t]['verdict'] not in READABLE
                    and direct[t]['verdict'] not in READABLE)
if both_fail:
    print('\n=== why the unreadable-either-way sites fail ===')
    for k, v in both_fail.most_common():
        print(f'  {k:<16} {v:>4}')

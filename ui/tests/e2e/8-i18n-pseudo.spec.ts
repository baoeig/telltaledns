// REQ: API-012 (ADR-189) — the pseudo-locale on the pages that show the files on the translation
// layer, in the browser against the real server: no English those files had is left on the page,
// and no pseudo text reaches a request or the browser's storage. Expects the state 1-ui.spec.ts
// leaves (the admin, some queries), as 8-i18n-pages.spec.ts does.
//
// While part A moves files to the layer one stage at a time, a page holds text of both kinds, so a
// piece of text is held to the rule when it is English that a file on the layer had and no file off
// the layer has (the English ledger says which file had what). A piece passes when it carries a
// pseudo character, or sits under translate="no". Text nodes, the attributes a reader sees or hears,
// and canvas text (fillText/strokeText, hooked) are read. Once every file is on the layer this is
// the design's whole-page rule.
import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import * as lib from '../../scripts/i18n-lib.mjs';
import { LOWER, UPPER } from '../../src/lib/i18n/pseudo';
import { UI } from './i18n-fixture';

const ADMIN = { user: 'admin', pass: 'correct horse battery' };
const PSEUDO = new RegExp(`[${[...LOWER, ...UPPER].join('')}]`);
const LEDGER: Record<string, { markup: string[]; script: string[] }> = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/english-ledger.json'), 'utf8'));
const LAYER = new Set(lib.check(UI, { ratchets: false }).files.filter((f: { usesLayer: boolean }) => f.usesLayer).map((f: { rel: string }) => f.rel));
// The help glossary is English data outside the layer (ADR-189: help stays English in part A).
const HELP: string[] = JSON.parse(readFileSync(join(UI, '../docs/help/topics.json'), 'utf8')).topics.flatMap((t: Record<string, unknown>) =>
  Object.values(t).filter((v): v is string => typeof v === 'string'),
);
const OFF = new Set([...Object.entries(LEDGER).filter(([rel]) => !LAYER.has(rel)).flatMap(([, e]) => [...e.markup, ...e.script]), ...HELP]);
// Values the test server holds that read like English a file on the layer had: the note
// 1-ui.spec.ts adds (the note form's example text), the support manifest's "unknown" and the health
// level "severe" the wall shows as it is (words the layer only compares with), and (A1d) the tooltip
// DashboardLegacy.svelte, a file off the layer, joins at run time from two of its own sentences (a
// path's, and " Over " plus a protocol's): no literal of that file is the whole text, and its pieces
// read like the pattern of SimulationCard.svelte's old "{} quer{}, {} device{}{}". They are data on
// the page, not text of the layer.
const DATA = new Set([
  'moved the access point to the hallway',
  'unknown',
  'severe',
  "Answered without looking anything up: blocked names, refused or rate-limited queries, malformed ones, and special names (like Firefox's DNS-over-HTTPS canary). Over plain DNS over UDP, what most devices use.",
]);
const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const letters = (s: string) => (s.replace(/\{\}/g, '').match(/[A-Za-z]/g) ?? []).length;
/** A path, a key, a type or a format: one token with a separator in it, no space. */
const CODE = /^[^\s]*[./:_=-][^\s]*$/;
/** The English only files on the layer had, each as a whole-text pattern (`{}`: anything). A
 * pattern with a placeholder needs words of its own (eight letters), or it would match any text. */
const OWN = Object.entries(LEDGER)
  .filter(([rel]) => LAYER.has(rel))
  .flatMap(([rel, e]) => [...e.markup, ...e.script].map((s) => ({ s, rel })))
  .filter(({ s }) => !OFF.has(s) && !CODE.test(s) && /[A-Za-z]{2}/.test(s.replace(/\{\}/g, '')) && (!s.includes('{}') || letters(s) >= 8))
  .map(({ s, rel }) => ({ re: new RegExp(`^${s.split('{}').map(escape).join('[\\s\\S]*?')}$`), from: `${rel}: ${JSON.stringify(s)}` }));
/** The English of files off the layer, as the same patterns: a piece that is that English belongs to
 * those files, whatever pattern of a layer file it also matches. */
const OFF_PATTERNS = [...OFF]
  .filter((s) => s.includes('{}') && !CODE.test(s) && letters(s) >= 8)
  .map((s) => new RegExp(`^${s.split('{}').map(escape).join('[\\s\\S]*?')}$`));

async function signIn(page: Page) {
  await page.goto('/');
  await page.getByLabel('Username', { exact: true }).fill(ADMIN.user);
  await page.getByLabel('Password', { exact: true }).fill(ADMIN.pass);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Dashboard' }).first()).toBeVisible({ timeout: 30_000 });
}

/** Records what the page sends and stores, and the canvas text it draws (before any page script). */
async function watch(page: Page) {
  const sent: string[] = [];
  page.on('request', (r) => sent.push(`${r.method()} ${r.url()} ${r.postData() ?? ''}`));
  await page.addInitScript(() => {
    const w = window as unknown as { __i18aCanvas: string[]; __i18aStored: string[] };
    w.__i18aCanvas = [];
    w.__i18aStored = [];
    for (const f of ['fillText', 'strokeText'] as const) {
      const orig = CanvasRenderingContext2D.prototype[f];
      CanvasRenderingContext2D.prototype[f] = function (this: CanvasRenderingContext2D, text: string, ...rest: [number, number, number?]) {
        w.__i18aCanvas.push(String(text));
        return orig.call(this, text, ...rest);
      };
    }
    const set = Storage.prototype.setItem;
    Storage.prototype.setItem = function (this: Storage, k: string, v: string) {
      w.__i18aStored.push(`${k}=${v}`);
      return set.call(this, k, v);
    };
  });
  return sent;
}

/** Every text the page shows that is English only files on the layer had (empty: the rule holds). */
async function leftovers(page: Page): Promise<string[]> {
  const pieces: string[] = await page.evaluate(() => {
    const ATTRS = ['aria-label', 'title', 'placeholder', 'alt', 'aria-roledescription', 'aria-description', 'aria-valuetext', 'aria-placeholder'];
    const out: string[] = [];
    const free = (el: Element) => !el.closest('[translate="no"], script, style');
    const tidy = (s: string) => s.replace(/\s+/g, ' ').trim();
    const walk = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    for (let n: Node | null = walk.currentNode; n; n = walk.nextNode()) {
      if (n.nodeType === Node.TEXT_NODE) {
        if (n.parentElement && free(n.parentElement) && tidy(n.textContent ?? '')) out.push(tidy(n.textContent ?? ''));
      } else if (n instanceof Element && free(n)) {
        for (const a of ATTRS) if (n.hasAttribute(a)) out.push(tidy(n.getAttribute(a) ?? ''));
        if (n.localName === 'optgroup' && n.hasAttribute('label')) out.push(tidy(n.getAttribute('label') ?? ''));
      }
    }
    return [...out, ...(window as unknown as { __i18aCanvas: string[] }).__i18aCanvas.map(tidy)];
  });
  const found: string[] = [];
  for (const p of new Set(pieces)) {
    // A piece an off-layer file has belongs to that file, whatever pattern of a layer file it also
    // matches (`{}query log {}` matched whole sentences of the Settings and Heatmap pages, and
    // `{}% of the budget left` the wall's "{}: {} % of the budget left").
    if (PSEUDO.test(p) || OFF.has(p) || DATA.has(p)) continue;
    const m = OWN.find((o) => o.re.test(p));
    if (m && !OFF_PATTERNS.some((re) => re.test(p))) found.push(`${JSON.stringify(p)} (${m.from})`);
  }
  return found;
}

/** Nothing the page sent or stored holds pseudo text. */
async function clean(page: Page, sent: string[]) {
  const stored: string[] = await page.evaluate(() => (window as unknown as { __i18aStored: string[] }).__i18aStored);
  const decoded = (s: string) => {
    try {
      return decodeURIComponent(s);
    } catch {
      return s;
    }
  };
  expect(sent.filter((s) => PSEUDO.test(s) || PSEUDO.test(decoded(s))), 'requests').toEqual([]);
  expect(stored.filter((s) => PSEUDO.test(s)), 'storage').toEqual([]);
}

async function open(page: Page, hash: string, ready: string) {
  await page.goto(`/?lang=en-XA#${hash}`);
  await expect(page.locator('html')).toHaveAttribute('lang', 'en-XA');
  await expect(page.locator(ready).first()).toBeVisible();
}

// A cluster of three, and anomaly findings: what a one-node test server cannot show.
const T0 = '2026-11-24T22:00:00Z';
const node = (o: Record<string, unknown>) => ({
  behindSeconds: null, cacheEntries: 1200, cacheHitPercent: 61.5, certExpiresAt: null, configLag: 0, configSeq: 42, configSource: 'file', connected: true,
  eligible: true, ephemeral: false, host: null, kubeNode: null, lastSeenSecondsAgo: 1, link: 'direct', maintenance: null, pod: null, protocol: 3, qps: 4.2,
  querySharePercent: 33, ready: true, restarts: 0, rttMs: 1.4, servfailPercent: 0.2, sourceCommit: null, startedAt: T0, thisNode: false, up: true,
  upstreamP90Ms: 21, uptimeSeconds: 86_400, version: '0.9.0', witness: false, ...o,
});
const CLUSTER = {
  authority: null, checks: [], clusterId: 'c-home', conflicts: [], enabled: true, events: [], healthy: true, host: null, name: 'home', newestConfigSeq: 42,
  rollout: null, source: null, thisNode: 'n-aaaa1111',
  failover: { active: true, leaseHeld: true, leaseSecondsLeft: 8, mode: 'automatic', reachableVoters: 3, votedEpoch: 4, votedFor: 'n-aaaa1111', voters: 3 },
  nodes: [node({ nodeId: 'n-aaaa1111', site: 'homelab', role: 'primary', thisNode: true, rttMs: null }), node({ nodeId: 'n-bbbb2222', site: 'pi', role: 'secondary' })],
};
const FINDINGS = {
  items: ['f-1', 'f-2'].map((id) => ({
    acknowledged: null, baseline: 40, client: '127.0.1.11', clientName: null, detail: 'Many more queries than usual in this hour.', domain: null, id,
    informational: false, kind: 'rate_spike', mutedBy: null, nodes: [], observed: 900, rhythm: null, spread: 12, stormMute: null, threshold: 200,
    windowSeconds: 3600, windowStart: T0,
  })),
};

test('api_012 pseudo locale leaves no literal text: dashboard', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/', '[data-testid="stat-queries"]');
  expect(await leftovers(page), 'dashboard').toEqual([]);
  await page.locator('ol.tl button.n').first().click();
  await expect(page.locator('ol.tl .more').first()).toBeVisible();
  await page.locator('figure.sc .bar button.link').first().click();
  await expect(page.locator('figure.sc .table-view').first()).toBeVisible();
  expect(await leftovers(page), 'a top list opened, a chart as a table').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: query log', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/queries', 'table.log tbody tr');
  await page.locator('table.log button.chip-button').first().click();
  await expect(page.locator('.chip .menu').first()).toBeVisible();
  expect(await leftovers(page), 'a device menu').toEqual([]);
  await page.locator('.chip .menu [role="menuitem"]').nth(1).click();
  await expect(page.locator('.drawer').first()).toBeVisible();
  expect(await leftovers(page), 'a device drawer').toEqual([]);
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Why?', exact: true }).first().click();
  await expect(page.getByTestId('quick-rule-form')).toBeVisible();
  expect(await leftovers(page), 'the why drawer and its quick rule').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: quick rules', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/rules', '[data-testid="quick-rule-form"]');
  await page.locator('[data-testid="quick-rule-form"] input.mono').fill('i18n-pseudo.example.com');
  await page.locator('[data-testid="quick-rule-form"] select').first().selectOption('everyone');
  await page.locator('[data-testid="quick-rule-form"] button.primary').click();
  await expect(page.getByTestId('quick-rule-saved')).toBeVisible();
  expect(await leftovers(page), 'the quick rule form, saved').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: groups and upstreams', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/upstreams', '[data-testid="entry-row"]');
  expect(await leftovers(page), 'upstreams').toEqual([]);
  await open(page, '/groups', '[data-testid="editor-group"]');
  await page.locator('[data-testid="editor-group"] .head > button').click();
  await page.locator('form.form input.mono').first().fill('i18n-pseudo');
  await page.locator('form.form button.primary').click();
  await expect(page.getByTestId('entry-preview')).toBeVisible();
  expect(await leftovers(page), 'a group checked').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: cluster', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/cluster', (r) => r.fulfill({ json: CLUSTER }));
  await page.route('**/api/v1/cluster/nodes/**', (r) =>
    r.fulfill({ json: { automaticFailoverAfter: false, eligible: true, ephemeral: false, heldClusterKey: false, nodeId: 'n-bbbb2222', notes: [], removed: false, site: 'pi', up: true, votersAfter: 1, witness: false } }),
  );
  await page.route('**/api/v1/nodes/*/drill**', (r) =>
    r.fulfill({
      json: { baseline: 6, dryRun: true, handover: 'none', id: 'd-1', node: 'n-bbbb2222', note: null, since: T0, until: T0, targets: 'clients', targetsResolved: 1, preview: 'Would stop answering 1 device.', checks: [{ name: 'others', ok: true, detail: 'One other node serves.' }], devices: [{ client: '192.168.1.20', name: 'laptop' }] },
    }),
  );
  await signIn(page);
  await open(page, '/cluster', '[data-testid="cluster-node"]');
  await page.getByTestId('drill-open-form').first().click();
  await page.getByTestId('drill-clients').fill('192.168.1.20');
  await page.getByTestId('drill-reason').fill('i18n');
  await page.getByTestId('drill-preview-button').click();
  await expect(page.getByTestId('drill-preview')).toBeVisible();
  await page.getByTestId('node-remove-open').first().click();
  await expect(page.getByTestId('node-remove-preview')).toBeVisible();
  await page.getByTestId('maintenance-open').first().click();
  await expect(page.getByTestId('maintenance-form')).toBeVisible();
  expect(await leftovers(page), 'drill, removal and maintenance forms').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: anomalies', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/analytics/anomalies?**', (r) => r.fulfill({ json: FINDINGS }));
  await signIn(page);
  await open(page, '/anomalies', '[data-testid="mute"]');
  await page.getByTestId('mute').first().click();
  await expect(page.getByTestId('mute-similar')).toBeVisible();
  expect(await leftovers(page), 'the mute form').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: settings', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/settings?tab=system', '[data-testid="support-package"]');
  await page.getByTestId('support-preview').click();
  await expect(page.getByTestId('support-manifest')).toBeVisible();
  expect(await leftovers(page), 'the support package preview').toEqual([]);
  await open(page, '/settings?tab=users', '[data-testid="wall-displays"]');
  await page.getByTestId('display-name').fill('i18n pseudo');
  await page.getByTestId('display-create').click();
  await expect(page.getByTestId('display-link')).toBeVisible();
  expect(await leftovers(page), 'a wall display link').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: heatmap flow and wall', async ({ page }) => {
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/heatmap', 'canvas');
  expect(await leftovers(page), 'heatmap (canvas text included)').toEqual([]);
  await open(page, '/flow', 'h1');
  expect(await leftovers(page), 'flow').toEqual([]);
  await open(page, '/wall', '[data-testid="wall-area"]');
  expect(await leftovers(page), 'wall').toEqual([]);
  await clean(page, sent);
});

// A1c: what the header, the cluster, the cache, the error budget and a device's views show only with
// data a test server does not have (health reasons, plans waiting, drills, a machine, an identity, a
// day, a configuration kept in Git).
const HEALTH = {
  checkedAt: T0, level: 'degraded', missingNodes: ['attic'],
  maintenance: [{ by: 'admin', node: 'pi', reason: 'SD card swap', secondsLeft: 1800, since: T0, until: T0 }],
  reasons: [{ code: 'upstream_down', level: 'degraded', link: '#/upstreams', node: 'pi', summary: 'Upstream quad9 does not answer.' }],
};
const PLANS = { items: [1, 2].map((i) => ({ id: `p-${i}`, state: 'pending', summary: 's', tool: 't', method: 'POST', path: '/api/v1/rules', reason: 'r', requestedBy: 'agent', needsApproval: true, preview: {}, configVersion: 1, createdUnixSeconds: 0, expiresUnixSeconds: 0 })) };
const row = (key: string, bad: number, exact = true) => ({ key, bad, sharePct: 50, exact, errorMax: exact ? null : 5, name: null });
const BUDGET = {
  objective: 'availability', step: '1h', from: '2026-11-24T10:00:00Z', to: T0, note: null, missingNodes: ['attic'],
  budget: { allowedBad: 500, answered: 100_000, bad: 560, remainingPct: -12, targetPct: 99.5 },
  byClient: [row('192.168.1.20', 300, false)], byName: [row('ads.example.com', 200, false)], byNode: [], byRcode: [row('SERVFAIL', 560)], byStatus: [row('servfail', 560)], byUpstream: [row('quad9', 400)],
  coverage: { approximate: true, hidden: ['names_hidden'], partialNodes: ['pi'], topListsFromHours: 24 },
  timeline: ['homelab', 'pi'].flatMap((node) => [0, 1, 2].map((i) => ({ answered: 1000, bad: 10, budgetSpentPct: 2, node, start: T0, startUnixSeconds: 1_764_021_600 + i * 3600 }))),
};
const DRILL = { by: 'admin', coveragePercent: 80, elsewhere: 0, ended: T0, followed: 1, id: 'd-1', medianFailoverMs: 2300, missingNodes: [], node: 'pi', p90FailoverMs: 4100, quiet: 1, reason: 'monthly', since: T0, status: 'complete', stuck: 1, targets: 'everyone', until: T0, viaBalancer: true };
const DRILLS = { items: [{ ...DRILL, id: 'd-r', status: 'running' }, DRILL, { ...DRILL, id: 'd-c', status: 'cancelled' }], missingNodes: ['attic'] };
const REPORT = {
  ...DRILL, balancerSwitchMs: 3400, baseline: 6, overflow: 0, missingNodes: ['attic'], nodes: [{ node: 'pi', queries: 3, devices: 1 }],
  devices: [
    { client: '192.168.1.20', dropped: 4, group: 'default', name: null, nodes: [], queriesElsewhere: 1, returned: false, timeToFailoverMs: 2100, verdict: 'followed' },
    { client: '192.168.1.21', dropped: 9, group: null, name: null, nodes: [], queriesElsewhere: 0, returned: null, timeToFailoverMs: null, verdict: 'stuck' },
  ],
};
const HOST = {
  history: [],
  latest: {
    arch: 'aarch64', cgroupCpuQuotaCores: 2, cgroupMemLimitBytes: 536_870_912, cgroupMemUsedBytes: 314_572_800, cgroupMemUsedPercent: 58.6, clockOffsetMs: 3200, cpuPercent: 93, cpus: 4,
    diskFreeBytes: 2e9, diskTotalBytes: 3e10, diskUsedPercent: 93, hostUptimeSeconds: 864_000, kernel: '6.6.31', load1: 3.2, load5: 2.1, load15: 1.4, maxFds: 1024, memAvailableBytes: 1e9,
    memTotalBytes: 4e9, memUsedPercent: 75, oomKills: 2, openFds: 96, os: 'Debian GNU/Linux 12', processRssBytes: 8e7, qlogBytes: 9e8, sampledAt: T0, snapshotBytes: 1.2e7, swapTotalBytes: 1e9,
    swapUsedBytes: 1e8, temperatureC: 81.5, threads: 12, throttledPercent: 12.5, unavailable: ['process', 'thermal'], warnings: ['The data disk is nearly full.'], writeBytesPerSecond: 20_480,
  },
};
const IDENTITY = { available: true, class: 'speaker', client: '127.0.0.1', level: 'likely', product: 'Echo Dot', score: 0.8, source: 'fingerprint', suggestedGroup: 'IoT', vendor: 'Amazon' };
const EVIDENCE = { domains: [{ name: 'amazon.com', weight: 1 }], macPrefix: 'a4:5e:60', matchedName: 'Echo-Kitchen', names: [], runnerUp: { product: 'Echo Show', productId: 'x', score: 0.4 }, seenDomains: [] };
const DAY = (() => {
  const t0 = Math.floor(Date.now() / 1000 / 3600) * 3600 - 23 * 3600;
  const bins = Array.from({ length: 24 }, (_, i) => t0 + i * 3600);
  const iso = (u: number) => new Date(u * 1000).toISOString();
  const lane = (domain: string | null, other = false) => ({ domain, other, counts: bins.map((_, i) => i % 3), blocked: bins.map((_, i) => (i % 5 ? 0 : 1)), slow: bins.map((_, i) => (i % 2 ? 1 : 0)), total: 24, blockedTotal: 5, firstSeenTime: domain ? iso(t0 + 3600) : null });
  return {
    client: '127.0.0.1', from: iso(t0), to: iso(t0 + 24 * 3600), binSeconds: 3600, bins, source: 'querylog', truncated: true, truncatedReason: 'row limit', coverageFrom: iso(t0 + 7200),
    lanes: [lane('example.com'), lane(null), lane(null, true)], queries: bins.map(() => 5), p99Ms: bins.map((_, i) => (i % 4 ? 12 : null)),
    events: [{ time: iso(t0 + 3600), timeUnixSeconds: t0 + 3600, kind: 'firstSeen', summary: 'first query to example.com', href: '#/queries', domain: 'example.com' }],
    markers: [],
  };
})();

test('api_012 pseudo locale leaves no literal text: health, agent inbox, help and notes', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/system/health', (r) => r.fulfill({ json: HEALTH }));
  await page.route('**/api/v1/plans', (r) => r.fulfill({ json: PLANS }));
  await signIn(page);
  await open(page, '/', '[data-testid="agent-inbox"]');
  await page.getByTestId('health-icon').click();
  await expect(page.getByTestId('health-maintenance')).toBeVisible();
  expect(await leftovers(page), 'the header pill and the health panel').toEqual([]);
  await page.keyboard.press('Escape');
  await page.locator('button[data-help="how-it-works"]').first().click();
  await expect(page.getByTestId('help-panel')).toBeVisible();
  expect(await leftovers(page), 'a help panel').toEqual([]);
  await page.keyboard.press('Escape');
  await page.getByTestId('note-open').click();
  await expect(page.getByTestId('note-form')).toBeVisible();
  expect(await leftovers(page), 'the note form').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: cache explain and the error budget', async ({ page }) => {
  const sent = await watch(page);
  const entry = (answers: number, ttl: number) => ({ ageSeconds: 1, answers, authentic: answers === 1, bytes: 90, checkingDisabled: false, dnssecOk: true, hits: 2, node: null, qtype: 'A', rcode: 'NOERROR', ttlLeftSeconds: ttl });
  await page.route('**/api/v1/cache/lookup?**', (r) => r.fulfill({ json: { name: 'i18n.example', entries: [entry(1, 30), entry(2, -5)] } }));
  await page.route('**/api/v1/cache/flush', (r) => r.fulfill({ json: { totalRemoved: 2, nodes: [{ node: 'pi', removed: null, error: 'not reachable' }] } }));
  await page.route('**/api/v1/stats/slo/attribution?**', (r) => r.fulfill({ json: BUDGET }));
  await signIn(page);
  await open(page, '/cache', '[data-testid="cache-card"]');
  await page.locator('[data-testid="cache-card"] form input').fill('i18n.example');
  await page.locator('[data-testid="cache-card"] form button').click();
  await expect(page.getByTestId('cache-entries')).toBeVisible();
  await page.locator('[data-testid="cache-card"] div.row button').first().click();
  await expect(page.getByTestId('cache-flushed')).toBeVisible();
  expect(await leftovers(page), 'a cache lookup and flush').toEqual([]);
  await open(page, '/explain?name=ads.e2e.test&client=127.0.0.1', '[data-testid="explain-flow"]');
  expect(await leftovers(page), 'an explanation and its diagram').toEqual([]);
  await open(page, '/analyze?tab=budget', '[data-testid="budget-summary"]');
  await page.locator('[data-testid="by-client"] button.link').click();
  await page.locator('[data-testid="budget-attribution"] section.chart .card-head button.link').first().click();
  expect(await leftovers(page), 'the error budget, a list and a chart as tables').toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: drills and a machine', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/cluster', (r) => r.fulfill({ json: CLUSTER }));
  await page.route('**/api/v1/drills?**', (r) => r.fulfill({ json: DRILLS }));
  await page.route('**/api/v1/drills/d-1', (r) => r.fulfill({ json: REPORT }));
  await page.route('**/api/v1/nodes/pi/drill', (r) => r.fulfill({ json: { by: 'admin', devicesDropped: 2, dropped: 40, id: 'd-r', node: 'pi', reason: 'monthly', secondsLeft: 90, since: T0, targets: 'everyone', until: T0 } }));
  await signIn(page);
  await open(page, '/cluster?tab=drills', '[data-testid="drill-counters"]');
  await page.getByTestId('drill-open').first().click();
  await expect(page.getByTestId('drill-report')).toBeVisible();
  expect(await leftovers(page), 'running drills, the list and a report').toEqual([]);
  await page.unroute('**/api/v1/cluster');
  await page.route('**/api/v1/cluster', (r) => r.fulfill({ json: { ...CLUSTER, enabled: false, host: HOST } }));
  await open(page, '/cluster', '[data-testid="host-card"]');
  expect(await leftovers(page), "a node's machine").toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo locale leaves no literal text: a device and a change kept in git', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/clients/identities', (r) => r.fulfill({ json: { items: [IDENTITY] } }));
  await page.route('**/api/v1/clients/127.0.0.1/identity', (r) => r.fulfill({ json: { ...IDENTITY, evidence: EVIDENCE } }));
  await page.route('**/api/v1/clients/*/timeline*', (r) => r.fulfill({ json: DAY }));
  await page.route('**/api/v1/groups/i18n-git*', (r) =>
    r.request().method() === 'PUT' && !new URL(r.request().url()).searchParams.has('dryRun')
      ? r.fulfill({ json: { applied: true, keepInGit: '[[groups]]\nname = "i18n-git"\n', warnings: [] } })
      : r.fallback(),
  );
  await signIn(page);
  await open(page, '/clients', '[data-testid="identity-line"]');
  await page.locator('[data-testid="identity-line"] button.link').first().click();
  await expect(page.locator('[data-testid="identity-line"] ul.why')).toBeVisible();
  expect(await leftovers(page), "a device's identity and why").toEqual([]);
  await open(page, '/clients?device=127.0.0.1&view=timeline', '[data-testid="timeline-canvas"]');
  const box = (await page.getByTestId('timeline-canvas').boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + 9);
  await expect(page.getByTestId('timeline-tip')).toBeVisible();
  expect(await leftovers(page), "a device's day and a lane's tip").toEqual([]);
  await page.mouse.move(box.x + box.width / 2, box.y + 3 * 18 + 15);
  await expect(page.getByTestId('timeline-tip')).toBeVisible();
  expect(await leftovers(page), 'the latency lane tip').toEqual([]);
  await page.getByTestId('timeline-table-toggle').click();
  await expect(page.getByTestId('timeline-table')).toBeVisible();
  expect(await leftovers(page), "a device's day as a table").toEqual([]);
  await open(page, '/groups', '[data-testid="editor-group"]');
  await page.locator('[data-testid="editor-group"] .head > button').click();
  await page.locator('form.form input.mono').first().fill('i18n-git');
  await page.locator('form.form button.primary').click();
  await expect(page.getByTestId('entry-preview')).toBeVisible();
  await page.locator('form.form button.primary').click();
  await expect(page.getByTestId('keep-in-git')).toBeVisible();
  expect(await leftovers(page), 'a change to keep in Git').toEqual([]);
  await clean(page, sent);
});

// A1d: what the service level, the latency heatmap, the wall's panels, the cluster's topology,
// comparison and rollouts, the pause control, the simulation card and the rhythm strip show only with
// data a test server does not have.
const NOW = Math.floor(Date.now() / 1000);
const BOUNDS = [250, 500, 1000, 2000, 4000, 8000, 16_000, 32_000, 64_000, 125_000, 250_000, 500_000, 1_000_000, 2_000_000, 3_000_000, 5_000_000];
const HEAT_STARTS = Array.from({ length: 30 }, (_, i) => NOW - (30 - i) * 60);
const HEAT = (by: string, names: string[]) => ({
  boundsUs: BOUNDS, by, from: T0, missingNodes: ['attic'], partial: true, step: 'minute', to: T0, starts: HEAT_STARTS,
  series: Object.fromEntries(names.map((n, k) => [n, HEAT_STARTS.map((_, c) => Array.from({ length: 17 }, (__, r) => Math.max(0, Math.round(40 * Math.exp(-((r - 3 - 2 * k) ** 2) / 5) * (1.2 + Math.sin(c / 4)))))) ])),
});
const EXEMPLARS = { missingNodes: [], items: [{ latencyUs: 120_000, node: null, path: 'upstream', time: T0, traceId: 'a'.repeat(32), tsUnixMicros: (NOW - 600) * 1e6, upstream: 'quad9' }, { latencyUs: 900, node: null, path: 'cache', time: T0, traceId: 'b'.repeat(32), tsUnixMicros: (NOW - 300) * 1e6, upstream: null }] };
const burn = (window: string, rate: number | null) => ({ window, rate, bad: 12, total: rate == null ? 0 : 1000 });
const objective = (o: Record<string, unknown>) => ({ alert: null, budgetRemainingPercent: 62.4, name: 'availability', good: 99_900, goodMeans: "answers that aren't SERVFAIL", latencyMs: null, sliPercent: 99.912, summary: 'x', targetPercent: 99.9, total: 100_000, burnRates: [burn('5m', 0.4), burn('1h', 1.2), burn('3d', null)], ...o });
const SLO = {
  enabled: true, windowDays: 30, missingNodes: ['attic'],
  objectives: [objective({ alert: 'fast', budgetRemainingPercent: 12.5 }), objective({ name: 'latency', alert: 'ticket', budgetRemainingPercent: -3, latencyMs: 50 }), objective({ name: 'other', alert: 'slow', budgetRemainingPercent: null, sliPercent: null, total: 0 })],
};
const SUMMARY = (to: boolean) => ({ activeClients: 12, blocked: 5000, blockedPercent: 20, cacheHitPercent: 62, cached: 12_000, forwarded: 8000, fromUnixSeconds: NOW - 3600, latency: [], missingNodes: [], nxdomain: 300, queries: to ? 10 : 25_000, refreshed: 100, servfail: 40, toUnixSeconds: NOW });
const ATTRIBUTION = {
  objective: 'availability', step: '1h', from: T0, to: T0, note: null, missingNodes: [], budget: { allowedBad: 500, answered: 100_000, bad: 300, remainingPct: 40, targetPct: 99.5 },
  byClient: [{ key: '192.168.1.20', bad: 3000, sharePct: 53.6, exact: true, errorMax: null, name: 'laptop' }], byName: [], byNode: [], byRcode: [], byStatus: [], byUpstream: [],
  coverage: { approximate: false, hidden: [], partialNodes: [], topListsFromHours: 24 }, timeline: [],
};
const fnode = (stage: string, name: string, count: number) => ({ count, deadEnd: 0, id: `${stage}:${name}`, name, stage });
const flink = (a: string, b: string, count: number, failure = false) => ({ count, failure, source: a, target: b });
const FLOW = {
  from: T0, to: T0, total: 1200, missingNodes: [],
  nodes: [fnode('group', 'default', 1000), fnode('group', 'tv', 2), fnode('status', 'cached', 500), fnode('status', 'forwarded', 600), fnode('upstream', 'quad9', 400), fnode('rcode', 'NOERROR', 1100), fnode('rcode', 'SERVFAIL', 20)],
  links: [flink('group:default', 'status:cached', 400), flink('group:default', 'status:forwarded', 600), flink('group:tv', 'status:cached', 2), flink('status:forwarded', 'upstream:quad9', 400), flink('upstream:quad9', 'rcode:NOERROR', 380), flink('upstream:quad9', 'rcode:SERVFAIL', 20, true), flink('status:cached', 'rcode:NOERROR', 500)],
};

test('api_012 pseudo locale leaves no literal text: service level, latency heatmap and the wall', async ({ page }) => {
  const sent = await watch(page);
  await page.addInitScript(() => localStorage.setItem('telltale.mode.admin', 'advanced'));
  await page.route(/\/api\/v1\/stats\/slo(\?.*)?$/, (r) => r.fulfill({ json: SLO }));
  await page.route('**/api/v1/stats/slo/attribution?**', (r) => r.fulfill({ json: ATTRIBUTION }));
  await page.route('**/api/v1/stats/summary?**', (r) => r.fulfill({ json: SUMMARY(new URL(r.request().url()).searchParams.has('to')) }));
  await page.route('**/api/v1/stats/latency/heatmap?**', (r) => r.fulfill({ json: HEAT('path', ['cache', 'upstream', 'local', 'synthesized']) }));
  await page.route('**/api/v1/stats/latency/exemplars?**', (r) => r.fulfill({ json: EXEMPLARS }));
  await page.route('**/api/v1/stats/flow?**', (r) => r.fulfill({ json: FLOW }));
  await signIn(page);
  const drawn = () => page.evaluate(() => (window as unknown as { __i18aCanvas: string[] }).__i18aCanvas.length);
  await open(page, '/?view=legacy', '[data-testid="slo-card"]');
  await expect(page.getByTestId('slo-availability')).toBeVisible();
  expect(await leftovers(page), 'the tiles with a change, and the service level card').toEqual([]);
  const before = await drawn();
  await page.getByTestId('time-heatmap-tab').click();
  await expect(page.getByTestId('exemplar').first()).toBeVisible();
  await expect.poll(drawn).toBeGreaterThan(before);
  await page.getByTestId('latency-heatmap-canvas').hover({ position: { x: 300, y: 40 } });
  await expect(page.locator('[data-testid="latency-heatmap"] .tip')).toBeVisible();
  expect(await leftovers(page), 'the latency heatmap, its canvas text, a tip and the dots').toEqual([]);
  await page.locator('[data-testid="latency-heatmap"] button.link').click();
  await expect(page.getByTestId('latency-heatmap-table')).toBeVisible();
  expect(await leftovers(page), 'the latency heatmap as a table').toEqual([]);
  const ready: Record<string, string> = {
    flow: '[data-testid="wall-flow"] svg text',
    slo: '[data-testid="wall-slo"] .who',
    kpi: '[data-testid="wall-kpi"] .tile',
    latency: '[data-testid="wall-latency-canvas"]',
  };
  for (const p of ['flow', 'slo', 'kpi', 'latency']) {
    const had = await drawn();
    await open(page, `/wall?layout=single&panels=${p}`, `[data-testid="wall-panel-${p}"]`);
    await expect(page.locator(ready[p]).first()).toBeVisible();
    if (p === 'latency') await expect.poll(drawn).toBeGreaterThan(had);
    if (p === 'kpi') await expect(page.locator('[data-testid="wall-kpi"] .value').first()).not.toHaveText('–');
    expect(await leftovers(page), `the wall's ${p} panel`).toEqual([]);
  }
  await clean(page, sent);
});

const pnode = (o: Record<string, unknown>) => ({ ...node({}), ...o });
const CLUSTER3 = {
  ...CLUSTER,
  rollout: { canaries: ['n-bbbb2222'], stage: 'canary', version: '3.131', pinned: null },
  nodes: [
    pnode({ nodeId: 'n-aaaa1111', site: 'homelab', role: 'primary', thisNode: true, rttMs: null }),
    pnode({ nodeId: 'n-bbbb2222', site: 'homelab', role: 'secondary', ready: false, maintenance: { by: 'admin', node: 'pi', reason: 'SD card swap', secondsLeft: 1800, since: T0, until: T0 } }),
    pnode({ nodeId: 'n-cccc3333', site: 'attic', role: 'secondary', up: false, connected: false, restarts: 3, configLag: 2 }),
    pnode({ nodeId: 'n-dddd4444', site: 'cloud', role: 'secondary', witness: true }),
    pnode({ nodeId: 'n-ffff6666', site: 'cloud', role: 'emergency primary' }),
    ...Array.from({ length: 8 }, (_, i) => pnode({ nodeId: `n-pod${i}`, site: 'k8s', role: 'secondary', ephemeral: true, pod: `telltaledns-756965cdc7-p${i}`, kubeNode: i < 7 ? 'kube-1' : null })),
  ],
};
const series = (f: (i: number) => number | null) => Array.from({ length: 6 }, (_, i) => f(i));
const compareNode = (o: Record<string, unknown>) => ({
  answered: 120_000, fromCache: 70_000, fromCachePct: 58.3, intervals: { servfailPct: [0.1, 0.4], slowPct: [0.02, 0.2], fromCachePct: [58, 58.6], upstreamFailPct: [0.5, 2.1] },
  label: 'homelab', nodeId: 'n-aaaa1111', p50Us: 800, p99Us: 45_000, qps: 33.4, servfail: 240, servfailPct: 0.2, slow: 120, slowPct: 0.1, thisNode: true, upstreamFailPct: 1.2, upstreamFailures: 12, upstreamQueries: 1000,
  series: { fromCachePct: series((i) => 55 + i), p99Us: series((i) => (i === 2 ? null : 40_000 + i)), qps: series((i) => 30 + i), servfailPct: series(() => 0.2), slowPct: series(() => 0.1), start: series((i) => NOW - i * 300), upstreamFailPct: series((i) => i) },
  ...o,
});
const COMPARE = {
  findings: [
    { detail: 'SERVFAIL 4.2 % against 0.2 % on the other nodes, since 21:05', label: 'pi', level: 'odd', metric: 'servfailPct', nodeId: 'n-bbbb2222', observed: 4.2, others: 0.2, spread: 0.5, oddSince: T0 },
    { detail: 'Upstream failures 12 % looks worse only because it asked upstream 25 times', label: 'attic', level: 'note', metric: 'upstreamFailPct', nodeId: 'n-cccc3333', observed: 12, others: 1.2, spread: 1, oddSince: null },
  ],
  from: T0, to: T0, missingNodes: ['cloud'], partial: true, step: 'hour',
  nodes: [compareNode({}), compareNode({ label: 'pi', nodeId: 'n-bbbb2222', thisNode: false, servfailPct: 4.2, p50Us: null, p99Us: null }), compareNode({ label: 'attic', nodeId: 'n-cccc3333', thisNode: false, answered: 0, intervals: {}, upstreamFailPct: 12 })],
};
const rnode = (n: string, o: Record<string, unknown>) => ({ appliedSeq: 131, canary: false, connected: true, node: n, ready: true, servfailPercent: 0.3, site: 'x', ...o });
const ROLLOUT = {
  bakeEnds: null, bakeSecs: 600, canaries: ['site:pi'], fromPrimary: true, pinned: null, readings: { afterPercent: 0.6, answers: 1200, beforePercent: null, reason: null }, rolloutCanaries: ['n-bbbb2222'],
  secondsLeft: 420, skipped: 'no canary node was online', stable: '3.130', stage: 'canary', started: T0, version: '3.131', waitingSince: null,
  nodes: [rnode('n-aaaa1111', {}), rnode('n-bbbb2222', { canary: true }), rnode('n-cccc3333', { canary: true, ready: false, appliedSeq: 130 }), rnode('n-dddd4444', { connected: false, ready: false })],
};
const version = (v: string, outcome: string, o: Record<string, unknown>) => ({ by: 'this node', created: T0, current: false, filterVersion: 4, outcome, pinnable: true, pinnedTo: null, readings: null, reason: null, version: v, ...o });
const VERSIONS = { history: 8, items: [version('3.131', 'canary', { current: true, pinnable: false }), version('3.130', 'stable'), version('3.129', 'canary_failed', { readings: { afterPercent: 6, answers: 90, beforePercent: 0.4, reason: 'SERVFAIL rose on the canaries' } }), version('3.128', 'pinned_to', { pinnedTo: '3.124', reason: 'by hand', pinnable: false }), version('3.127', 'new-outcome', {})] };

test('api_012 pseudo locale leaves no literal text: topology, comparison and rollouts', async ({ page }) => {
  const sent = await watch(page);
  let pinned = false;
  await page.route('**/api/v1/cluster', (r) => r.fulfill({ json: CLUSTER3 }));
  await page.route('**/api/v1/cluster/compare?**', (r) => r.fulfill({ json: COMPARE }));
  await page.route('**/api/v1/cluster/rollout', (r) =>
    r.fulfill({
      json: pinned
        ? { ...ROLLOUT, stage: 'pinned', version: null, pinned: { by: 'admin', changes: ['lists: changed'], diff: [], reason: 'broke banking', since: T0, to: '3.124' }, readings: { afterPercent: null, answers: 0, beforePercent: null, reason: 'SERVFAIL rose' } }
        : ROLLOUT,
    }),
  );
  await page.route('**/api/v1/cluster/versions', (r) => r.fulfill({ json: VERSIONS }));
  await page.route('**/api/v1/cluster/versions/*/pin?**', (r) => r.fulfill({ json: { changes: ['lists: changed'], configVersion: 1, done: 'would pin', dryRun: true } }));
  await page.route('**/api/v1/cluster/pin?**', (r) => r.fulfill({ json: { changes: [], configVersion: 1, done: 'would unpin', dryRun: true } }));
  await signIn(page);
  await open(page, '/cluster', '[data-testid="cluster-topology"]');
  expect(await leftovers(page), 'the topology, with pods, a witness, a node in maintenance and a node that is down').toEqual([]);
  await open(page, '/cluster?tab=compare', '[data-testid="compare-grid"]');
  expect(await leftovers(page), 'the comparison').toEqual([]);
  await page.getByTestId('compare-table-toggle').click();
  await expect(page.getByTestId('compare-table')).toBeVisible();
  expect(await leftovers(page), 'the comparison as a table').toEqual([]);
  await open(page, '/cluster?tab=rollouts', '[data-testid="rollout-baking"]');
  expect(await leftovers(page), 'a version baking').toEqual([]);
  await page.getByTestId('pin-open').first().click();
  await page.getByTestId('pin-form').locator('input').fill('i18n pseudo');
  await page.getByTestId('pin-check').click();
  await expect(page.getByTestId('pin-preview')).toBeVisible();
  expect(await leftovers(page), 'a pin checked').toEqual([]);
  pinned = true;
  await open(page, '/cluster', '[data-testid="cluster-topology"]');
  await open(page, '/cluster?tab=rollouts', '[data-testid="pin-banner"]');
  await page.getByTestId('unpin').click();
  await expect(page.getByTestId('unpin-confirm')).toBeVisible();
  expect(await leftovers(page), 'the pin banner and an unpin checked').toEqual([]);
  await clean(page, sent);
});

const SIMULATION = (reason: string | null, available = true) => ({
  applicable: true, available, from: T0, to: T0, rows: 12_345, partial: true, unchanged: 12_000, undetermined: 1, missingNodes: ['attic'], reason, notes: ['identity_from_event', 'something_new'],
  newlyBlocked: { devices: 1, queries: 300, topNames: [{ devices: 1, list: 'ads', name: 'ads.example.com', queries: 200 }, { devices: 1, list: null, name: 'one.example.net', queries: 1 }] },
  newlyAllowed: { devices: 3, queries: 40, topNames: [] }, changedRoute: { devices: 2, queries: 2, topNames: [] }, changedAnswer: { devices: 0, queries: 0, topNames: [] },
  byDevice: [{ changedAnswer: 0, changedRoute: 1, client: '192.168.1.20', name: 'laptop', newlyAllowed: 20, newlyBlocked: 100 }, { changedAnswer: 3, changedRoute: 0, client: '192.168.1.21', name: null, newlyAllowed: 0, newlyBlocked: 0 }], byGroup: [],
});
const BEACON = (id: string, kind: string) => ({
  acknowledged: null, baseline: 40, client: '127.0.1.11', clientName: null, detail: 'Asks for beacon.example.net every 38 seconds.', domain: 'beacon.example.net', id, informational: false, kind,
  mutedBy: null, nodes: [], observed: 900, rhythm: Array.from({ length: 24 }, (_, i) => Math.round(200 * Math.exp(-((i - 9) ** 2)))), spread: 12, stormMute: null, threshold: 200, windowSeconds: 3600, windowStart: T0,
});
const rdom = (domain: string, o: Record<string, unknown>) => ({
  domain, dominantBin: 9, dominantSharePct: 62, hour: Array.from({ length: 24 }, (_, i) => Math.round(30 * Math.exp(-((i - 9) ** 2)))), knownPeriodic: false, meanPerHour: 60, nodes: [], period: 'every 38 s',
  recent: Array.from({ length: 24 }, (_, i) => Math.round(300 * Math.exp(-((i - 9) ** 2)))), regularHours: 0, since: T0, ...o,
});
const RHYTHM = {
  available: true, client: '127.0.1.11', missingNodes: [], reason: null,
  binsS: [1, 1.5, 2.2, 3.3, 5, 7.5, 11, 17, 25, 38, 56, 84, 126, 190, 285, 427, 640, 960, 1440, 2160, 3240, 4860, 7290],
  domains: [rdom('time.example.com', { knownPeriodic: true }), rdom('beacon.example.net', { regularHours: 5 }), rdom('www.example.org', { period: null })],
};

test('api_012 pseudo locale leaves no literal text: pause, simulation and rhythm', async ({ page }) => {
  const sent = await watch(page);
  await page.route('**/api/v1/analytics/anomalies?**', (r) => r.fulfill({ json: { items: [BEACON('f-b', 'beacon'), BEACON('f-c', 'chatty_loop')] } }));
  await page.route('**/api/v1/clients/*/rhythm', (r) => r.fulfill({ json: RHYTHM }));
  const reasons = ['disabled', 'privacy_level', 'no_query_log', 'busy', 'local_file', 'download_limit', 'unheard_of', null];
  let call = 0;
  await page.route('**/api/v1/rules/*?**', (r) => {
    const url = new URL(r.request().url());
    if (r.request().method() !== 'PUT' || !url.searchParams.has('simulate')) return r.fallback();
    const n = call++;
    return r.fulfill({ json: { applied: false, preview: null, simulation: n === 0 ? SIMULATION(null) : SIMULATION(reasons[(n - 1) % reasons.length], false), warnings: [] } });
  });
  await signIn(page);
  await open(page, '/', '[data-testid="stat-queries"]');
  await page.locator('.pause-menu button.icon-btn').click();
  await expect(page.locator('.pause-menu [role="menu"]')).toBeVisible();
  expect(await leftovers(page), 'the pause menu').toEqual([]);
  // The control reads the pauses when it starts (and every 15 s): each state is a new page load.
  const paused = async (pauses: { group: string | null; secondsLeft: number }[], ready: string, what: string) => {
    await page.unroute('**/api/v1/blocking');
    await page.route('**/api/v1/blocking', (r) => r.fulfill({ json: { items: [{ node: 'pi', pauses: pauses.map((p) => ({ ...p, until: T0 })) }] } }));
    await page.reload();
    await expect(page.locator(ready)).toBeVisible();
    expect(await leftovers(page), what).toEqual([]);
  };
  await paused([{ group: 'kids', secondsLeft: 900 }, { group: 'guests', secondsLeft: 900 }], '[data-testid="groups-paused"]', 'two groups paused');
  await paused([{ group: 'kids', secondsLeft: 900 }], '[data-testid="groups-paused"]', 'one group paused');
  for (const secs of [30, 1500, 5400]) await paused([{ group: null, secondsLeft: secs }], '[data-testid="blocking-paused"]', `everyone paused for ${secs} s`);
  await page.unroute('**/api/v1/blocking');
  await page.reload();
  await expect(page.locator('.pause-menu button.icon-btn')).toBeVisible();
  await open(page, '/rules', '[data-testid="quick-rule-form"]');
  await page.locator('[data-testid="quick-rule-form"] input.mono').fill('i18n-sim.example.com');
  await page.getByTestId('simulate-button').click();
  await expect(page.getByTestId('simulation-card')).toBeVisible();
  expect(await leftovers(page), 'a simulation').toEqual([]);
  for (let i = 0; i < reasons.length; i++) {
    await page.getByTestId('simulate-button').click();
    await expect(page.getByTestId('simulation-unavailable')).toBeVisible();
    expect(await leftovers(page), `a simulation that could not run (${reasons[i]})`).toEqual([]);
  }
  await open(page, '/anomalies', '[data-testid="rhythm"]');
  expect(await leftovers(page), "a beacon's rhythm").toEqual([]);
  await open(page, '/clients?device=127.0.1.11', '[data-testid="rhythm-row"]');
  expect(await leftovers(page), "a device's rhythm").toEqual([]);
  await page.locator('[data-testid="rhythm"] .seg button').last().click();
  await page.getByTestId('rhythm-table-toggle').click();
  await expect(page.getByTestId('rhythm-table')).toBeVisible();
  expect(await leftovers(page), "a device's rhythm as a table").toEqual([]);
  await clean(page, sent);
});

test('api_012 pseudo text never reaches a request or storage: the shared forms', async ({ page }) => {
  // The forms above that write (a quick rule, a group checked, a wall display, a drill preview, a
  // mute preview) each ran under the pseudo-locale with their requests and storage watched; this one
  // names a device, whose body carries the name and kind typed in, never their labels.
  const sent = await watch(page);
  await signIn(page);
  await open(page, '/queries', 'table.log tbody tr');
  await page.locator('table.log button.chip-button').first().click();
  await page.locator('.chip .menu [role="menuitem"]').first().click();
  await expect(page.locator('.drawer').first()).toBeVisible();
  const form = page.locator('.drawer form.stack');
  if (await form.count()) {
    await form.locator('select').first().selectOption('phone');
    await form.locator('input[name="device-name"]').fill('i18n pseudo device');
    await form.locator('button.primary[type="submit"]').click();
    await expect(page.locator('.drawer .notice.ok')).toBeVisible();
  }
  expect(await leftovers(page), 'the device drawer').toEqual([]);
  await clean(page, sent);
  expect(sent.some((s) => s.startsWith('PUT ') && s.includes('/api/v1/clients/')) || !(await form.count()), 'a device was saved').toBe(true);
});

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
    if (PSEUDO.test(p)) continue;
    const m = OWN.find((o) => o.re.test(p));
    if (m) found.push(`${JSON.stringify(p)} (${m.from})`);
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

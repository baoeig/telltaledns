// REQ: API-012 (ADR-189) — the attribution rule of the pseudo-locale page tests (i18n-pseudo-rule.ts)
// held to fixtures, without a browser: a site reverted to its literal English fails it, a word that
// ends in a full stop or a colon is text and not code, and server data excuses text nodes only.
// The fixture is a small component with the shape of ClientChip's buttons, rendered on the server
// under the pseudo-locale.
import { expect, test } from '@playwright/test';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { compile } from 'svelte/compiler';
import { render } from 'svelte/server';
import * as lib from '../../scripts/i18n-lib.mjs';
import { install, tr } from '../../src/lib/i18n';
import { DATA, PSEUDO, isCode, isEnglish, makeRule, type Ledger } from './i18n-pseudo-rule';
import { UI, checkUi, cleanup } from './i18n-fixture';

test.afterAll(cleanup);
const temps: string[] = [];
test.afterAll(() => {
  for (const d of temps.splice(0)) rmSync(d, { recursive: true, force: true });
});

const REL = 'lib/components/Chip.svelte';
/** The component as it was before its migration: the English the ledger records. */
const BEFORE = [
  '<script lang="ts">',
  '  let { busy = false }: { busy?: boolean } = $props();',
  "  const label = busy ? 'Saving…' : 'Save';",
  '</script>',
  '<button type="button">{label}</button>',
  '<p>Saved.</p>',
  '<p>Canaries:</p>',
  '<input placeholder="moved the access point to the hallway" />',
  '',
].join('\n');
/** The same after it: every text through `t()`. */
const AFTER = [
  '<script lang="ts">',
  "  import { tr } from '../lib/i18n';",
  "  const { t } = tr('chip');",
  '  let { busy = false }: { busy?: boolean } = $props();',
  '</script>',
  "<button type=\"button\">{busy ? t('Saving…') : t('Save')}</button>",
  "<p>{t('Saved.')}</p>",
  "<p>{t('Canaries:')}</p>",
  "<input placeholder={t('moved the access point to the hallway')} />",
  '',
].join('\n');

/** The page's text nodes, and its attributes a reader sees or hears, as the pseudo-locale draws it. */
async function drawn(source: string): Promise<{ text: string[]; other: string[] }> {
  const js = compile(source, { generate: 'server', filename: 'Chip.svelte' }).js.code
    .replace(/from ['"](svelte(?:\/[^'"]*)?)['"]/g, (_, spec) => `from ${JSON.stringify(import.meta.resolve(spec))}`)
    .replace(/import \{ tr \} from ['"][^'"]+['"];/, 'const { tr } = globalThis.__i18aRule;');
  const dir = mkdtempSync(join(tmpdir(), 'tt-rule-'));
  temps.push(dir);
  writeFileSync(join(dir, 'Chip.mjs'), js);
  (globalThis as { __i18aRule?: unknown }).__i18aRule = { tr };
  const mod = await import(pathToFileURL(join(dir, 'Chip.mjs')).href);
  install({ tag: 'en-XA' });
  try {
    const html = render(mod.default, { props: {} }).body.replace(/<!--[\s\S]*?-->/g, '');
    const tidy = (s: string) => s.replace(/\s+/g, ' ').trim();
    const text = html.split(/<[^>]*>/).map(tidy).filter(Boolean);
    const other = [...html.matchAll(/\b(?:aria-label|title|placeholder|alt)="([^"]*)"/g)].map((m) => tidy(m[1]));
    return { text, other };
  } finally {
    install({ tag: 'en' });
  }
}

const entry = (source: string, rel: string) => {
  const a = lib.analyzeFile(rel, source, 'chip');
  return { markup: [...a.ledger.markup].sort(), script: [...a.ledger.script].sort() };
};
/** The rule over a ledger with the component's English, and another file off the layer that has two
 * of its words too (`Save`, `Saving…`: words every page has). */
const RULE = makeRule({
  ledger: { [REL]: entry(BEFORE, REL), 'pages/Other.svelte': { markup: [], script: ['Save', 'Saving…'] } } as Ledger,
  layer: new Set([REL]),
  help: [],
  data: { 'moved the access point to the hallway': 'pages/Notes.svelte' },
});

test('api_012 pseudo rule: a word that ends in a full stop or a colon is text, a token with a separator is code', () => {
  for (const word of ['Saved.', 'Canaries:', 'left.', 'Partial:', 'node:', 'wait:', '/annotations', '/auth/']) expect(isCode(word), word).toBe(false);
  for (const token of ['data-theme', '2-digit', 'x-csrf-token', 'a/b', 'quick-rule-form', 'application/json', 'telltale-support.tar.gz', '--surface-2', 'neighbor_mac', 'node:{}', '/cache/stats', 'Queries/s']) expect(isCode(token), token).toBe(true);
  // The ledger of the UI itself: strings the rule newly holds the page to.
  const ledger: Ledger = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/english-ledger.json'), 'utf8'));
  const layer = new Set<string>(checkUi().files.filter((f) => f.usesLayer).map((f) => f.rel));
  const own = new Set(makeRule({ ledger, layer, help: [], data: {} }).own.map((o) => o.s));
  for (const s of ['Saved.', 'Canaries:', 'Partial:', 'left.']) expect(own.has(s), s).toBe(true);
  for (const s of ['data-theme', '2-digit', 'x-csrf-token']) expect(own.has(s), s).toBe(false);
});

test('api_012 pseudo rule: a site reverted to its literal english fails it', async () => {
  const clean = await drawn(AFTER);
  expect(clean.text.every((s) => PSEUDO.test(s))).toBe(true);
  expect(RULE.found(clean.text, clean.other)).toEqual([]);
  // `Saved.` and `Canaries:` back as literals: English a file on the layer had, left on the page.
  for (const word of ['Saved.', 'Canaries:']) {
    const reverted = await drawn(AFTER.replace(`t('${word}')`, `'${word}'`));
    expect(reverted.text, word).toContain(word);
    expect(RULE.found(reverted.text, reverted.other), word).toEqual([expect.stringContaining(JSON.stringify(word))]);
  }
  // The placeholder back as a literal attribute.
  const placeholder = await drawn(AFTER.replace("{t('moved the access point to the hallway')}", '"moved the access point to the hallway"'));
  expect(RULE.found(placeholder.text, placeholder.other)).toEqual([expect.stringContaining('moved the access point to the hallway')]);
  // `Saving…` and `Save` back as literals: not caught while another file off the layer has the
  // same words (the attribution rule's limit; the whole-page rule of the last stage closes it).
  const shadowed = await drawn(AFTER.replace("{busy ? t('Saving…') : t('Save')}", "{busy ? 'Saving…' : 'Save'}"));
  expect(shadowed.text).toContain('Save');
  expect(RULE.found(shadowed.text, shadowed.other)).toEqual([]);
});

test('api_012 pseudo rule: server data excuses a text node and never an attribute', () => {
  const note = 'moved the access point to the hallway';
  expect(RULE.found([note], [])).toEqual([]);
  // The same words as an attribute, or as an attribute and a text node: the layer's own.
  expect(RULE.found([], [note])).toEqual([expect.stringContaining(note)]);
  expect(RULE.found([note], [note])).toEqual([expect.stringContaining(note)]);
  // Every entry of the page test's list names a file of the ledger, and is data in a text node only.
  const ledger: Ledger = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/english-ledger.json'), 'utf8'));
  for (const [text, file] of Object.entries(DATA)) expect(Object.hasOwn(ledger, file), `${text}: ${file}`).toBe(true);
  const real = makeRule({ ledger, layer: new Set(checkUi().files.filter((f) => f.usesLayer).map((f) => f.rel)), help: [], data: DATA });
  for (const text of Object.keys(DATA)) {
    expect(real.found([text], []), text).toEqual([]);
    if (real.own.some((o) => o.s === text)) expect(real.found([], [text]).length, text).toBeGreaterThan(0);
  }
});

test('api_012 pseudo rule: a long text is judged in linear time', () => {
  // A pattern with adjacent placeholders, and a text of many short pieces: as a regular expression of lazy
  // gaps it took seconds on the few thousand characters of the dashboard's list of changes, and never ends
  // on this one.
  const hot: Ledger = { 'lib/Hot.svelte': { markup: ['Open {}{}{}{} at {}: open this query now'], script: [] } };
  const rule = makeRule({ ledger: hot, layer: new Set(['lib/Hot.svelte']), help: [], data: {} });
  const long = `Open ${Array.from({ length: 3000 }, (_, i) => `at ${i}: a`).join(' ')}`;
  expect(long.length).toBeGreaterThan(20_000);
  expect(rule.found([long], [])).toEqual([]);
  expect(rule.found([`${long}: open this query now`], [])).toHaveLength(1);
});

test('api_012 pseudo rule: a text fits an English with placeholders as the lazy pattern said', () => {
  const escape = (x: string) => x.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const lazy = (s: string) => new RegExp(`^${s.split('{}').map(escape).join('[\\s\\S]*?')}$`);
  const english = ['Saved.', '{} items', 'Open {}', '{}{} x {}', 'a {} b {} c', '{}', 'a{}b', '{}a{}', 'x {}{}', 'a{}a', 'Hello {} and {}!'];
  const texts = ['', 'Saved.', 'Saved. ', ' 5 items', 'items', 'Open', 'Open ', 'Open it', ' x ', 'q x z', 'x  x ', 'a b c', 'a  b  c', 'a b b c', 'ab', 'aXb', 'a', 'aa', 'aXa', 'Hello 1 and 2!', 'Hello and !', 'Hello 1 and 2', 'Hello and  and !', 'line\nbreak items', 'items items'];
  for (const s of english) for (const t of texts) expect(isEnglish(s, t), `${JSON.stringify(s)} / ${JSON.stringify(t)}`).toBe(lazy(s).test(t));
});

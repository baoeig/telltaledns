// REQ: API-012 (ADR-189) — the text ratchets (literal text, Intl outside lib/format.ts, physical CSS)
// and the English ledger, without a browser: the UI holds to its baseline (i18n-baseline.json), a
// file on the translation layer still shows the English its ledger entry recorded, and each rule
// fails on a fixture made to trip it.
import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import * as lib from '../../scripts/i18n-lib.mjs';
import { UI, checkUi, cleanup, edit, failRules, fails, project, put, run } from './i18n-fixture';

test.afterAll(cleanup);

test('api_012 only format.ts formats', () => {
  const r = checkUi();
  expect(fails(r).filter((f) => f.rule === 'intl')).toEqual([]);
  const base = JSON.parse(readFileSync(join(UI, 'i18n-baseline.json'), 'utf8'));
  const now = lib.ratchetCounts(UI, r.files).intl as Record<string, number>;
  const total = (o: Record<string, number>) => Object.values(o).reduce((a, b) => a + b, 0);
  console.log(`Intl/toLocale call sites outside format.ts: ${total(now)} in ${Object.values(now).filter((n) => n > 0).length} files (baseline ${total(base.intl)})`);
  expect(total(now)).toBeLessThanOrEqual(total(base.intl));
  // Growth fails; a file on the layer is at zero; format.ts itself is the place for it.
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('</script>', '  const n = (1).toLocaleString();\n</script>')))))).toEqual(['intl']);
  expect(failRules(run(project((d) => edit(d, 'src/lib/labels.ts', (s) => `${s}\nexport const nf = new Intl.NumberFormat();\n`))))).toEqual(['intl']);
  expect(failRules(run(project((d) => put(d, 'src/lib/format.ts', 'export const nf = new Intl.NumberFormat();\n'))))).toEqual([]);
  // A file whose baseline allows its call moves to the layer: it must be at zero, growth or not.
  const old = run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('<script lang="ts">\n', "<script lang=\"ts\">\n  import { tr } from '../lib/i18n';\n  const { t } = tr('old');\n"))));
  expect(failRules(old)).toEqual(['intl']);
  expect(fails(old).map((f) => f.msg)).toEqual(['a file on the translation layer formats through lib/format.ts only']);
});

test('api_012 migrated files hold no literal text', () => {
  const r = checkUi();
  expect(fails(r).filter((f) => f.rule === 'literal-text')).toEqual([]);
  const counts = lib.ratchetCounts(UI, r.files).literal as Record<string, number>;
  const above = Object.entries(counts).filter(([, n]) => n > 0);
  console.log(`files above 0 (moved to the layer in their own batch): ${above.length}\n${above.map(([f, n]) => `  ${f} ${n}`).join('\n')}`);
  // The baseline lists every source file, zeros too.
  const base = JSON.parse(readFileSync(join(UI, 'i18n-baseline.json'), 'utf8'));
  expect(Object.keys(base.literal).sort()).toEqual(r.files.map((f) => f.rel).sort());
  // A file at zero fails on any literal text; a file above zero is only printed.
  expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => `${s}<p>Plain words</p>\n`))))).toEqual(['literal-text']);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => `${s}<input placeholder="Search" />\n`))))).toEqual(['literal-text']);
  // Until part A ends, a file the baseline does not list (new since) is reported, not failed.
  const fresh = run(project((d) => put(d, 'src/pages/New.svelte', '<h1>A new page</h1>\n')));
  expect(failRules(fresh)).toEqual([]);
  expect(fresh.findings.filter((f) => f.rule === 'literal-text' && f.file === 'src/pages/New.svelte').map((f) => f.level)).toEqual(['report']);
  expect(lib.NEW_FILES_STRICT).toBe(false);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => `${s}\n<p>More words</p>\n`))))).toEqual([]);
  // Data marked translate="no", and text without letters, are not literal text.
  expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => `${s}<p translate="no">example.com</p>\n<p>· — (42)</p>\n`))))).toEqual([]);
});

test('api_012 migrated files conserve their english strings', () => {
  const ledger = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/english-ledger.json'), 'utf8'));
  const exceptions = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/ledger-exceptions.json'), 'utf8'));
  const r = checkUi();
  const migrated = (r.files as { rel: string; usesLayer: boolean }[]).filter((f) => f.usesLayer);
  expect(migrated.map((f) => f.rel)).toContain('lib/format.ts');
  expect(ledger['lib/format.ts']).toBeTruthy();
  // A file with no ledger entry is new since the ledger was written: it had no English to keep.
  for (const f of migrated) if (Object.hasOwn(ledger, f.rel)) expect(lib.conserve(f, ledger[f.rel], exceptions[f.rel] ?? []), f.rel).toEqual([]);
  for (const rel of Object.keys(exceptions)) expect(migrated.map((f) => f.rel), `exception for ${rel}`).toContain(rel);
  // The rule can fail: a page before and after a migration.
  const before = lib.analyzeFile('pages/X.svelte', '<h1>Holds</h1>\n<p title="Close">Muted by <b>{who}</b>: not counted</p>\n', 'x');
  const entry = { markup: [...before.ledger.markup].sort(), script: [...before.ledger.script].sort() };
  expect(entry.markup).toEqual([': not counted', 'Close', 'Holds', 'Muted by']);
  const head = "<script lang=\"ts\">\n  import { tr } from '../lib/i18n';\n  import T from '../lib/i18n/T.svelte';\n  const { t } = tr('x');\n</script>\n";
  const ok = `${head}<h1>{t('Holds')}</h1>\n<p title={t('Close')}><T ctx="x" msg={'Muted by <b>{who}</b>: not counted'} params={{ who }}>{#snippet b(s)}<b>{s}</b>{/snippet}</T></p>\n`;
  const after = (src: string) => lib.analyzeFile('pages/X.svelte', src, 'x');
  const merged = { sentence: 'Muted by <b>{who}</b>: not counted', fragments: ['Muted by', ': not counted'] };
  expect(lib.conserve(after(ok), entry, [merged])).toEqual([]);
  expect(lib.conserve(after(ok), entry, []).join('\n')).toContain('is not English this file had');
  expect(lib.conserve(after(ok.replace("t('Holds')", "t('Holds!')")), entry, [merged]).join('\n')).toContain('"Holds!"');
  expect(lib.conserve(after(ok.replace("<h1>{t('Holds')}</h1>\n", '')), entry, [merged])).toEqual(['template text "Holds" is gone']);
  expect(lib.conserve(after(ok.replace("<h1>{t('Holds')}</h1>", '<h1>Holds now</h1>')), entry, [merged]).join('\n')).toContain('"Holds now" is not in the ledger');
  expect(lib.conserve(after(ok), entry, [{ ...merged, fragments: ['Muted by', ': counted'] }]).join('\n')).toContain("not the sentence's pieces");
  expect(lib.conserve(after(ok), entry, [{ ...merged, fragments: ['Muted by', ': not counted', 'Elsewhere'] }]).join('\n')).toContain('"Elsewhere" is not in this file\'s ledger entry');
});

test('api_012 no new physical css properties', () => {
  const r = checkUi();
  expect(fails(r).filter((f) => f.rule === 'physical-css')).toEqual([]);
  const base = JSON.parse(readFileSync(join(UI, 'i18n-baseline.json'), 'utf8'));
  const now = lib.ratchetCounts(UI, r.files).physical as Record<string, number>;
  const total = (o: Record<string, number>) => Object.values(o).reduce((a, b) => a + b, 0);
  console.log(`physical CSS properties: ${total(now)} in ${Object.values(now).filter((n) => n > 0).length} files (baseline ${total(base.physical)})`);
  expect(lib.physical('margin-left', '1px')).toBe(true);
  expect(lib.physical('border-top-right-radius', '2px')).toBe(true);
  expect(lib.physical('text-align', 'right')).toBe(true);
  expect(lib.physical('float', 'left')).toBe(true);
  expect(lib.physical('right', '0')).toBe(true);
  expect(lib.physical('text-align', 'start')).toBe(false);
  expect(lib.physical('margin-inline-start', '1px')).toBe(false);
  expect(lib.physical('margin', '0 4px')).toBe(false);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('margin-left: 4px;', 'margin-left: 4px;\n    text-align: right;')))))).toEqual(['physical-css']);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => s.replace('<h2>', '<h2 style:margin-right="2px">')))))).toEqual(['physical-css']);
  expect(failRules(run(project((d) => put(d, 'src/app.css', '.x {\n  text-align: left;\n}\n'))))).toEqual(['physical-css']);
  expect(failRules(run(project((d) => put(d, 'src/app.css', '.x {\n  margin-inline-start: 1px;\n  text-align: start;\n}\n'))))).toEqual([]);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('margin-left: 4px;', 'margin-inline-start: 4px;')))))).toEqual([]);
});

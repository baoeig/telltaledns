// REQ: API-012 (ADR-189) — the text ratchets (literal text, Intl outside lib/format.ts, physical CSS)
// and the English ledger, without a browser: the UI holds to its baseline (i18n-baseline.json), a
// file on the translation layer still shows the English its ledger entry recorded, and each rule
// fails on a fixture made to trip it.
import { expect, test } from '@playwright/test';
import { mkdirSync, readFileSync, renameSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import * as lib from '../../scripts/i18n-lib.mjs';
import { UI, checkUi, cleanup, edit, failRules, fails, project, put, run, uiCopy, type Result } from './i18n-fixture';

test.afterAll(cleanup);

const LEDGER = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/english-ledger.json'), 'utf8'));
const EXCEPTIONS = JSON.parse(readFileSync(join(UI, 'tests/i18n-fixtures/ledger-exceptions.json'), 'utf8'));
const total = (o: Record<string, number>) => Object.values(o).reduce((a, b) => a + b, 0);

/** What these tests require of a tree, as a list of problems (empty: green). */
function ratchetProblems(root: string, r: Result): string[] {
  const out = fails(r)
    .filter((f) => ['literal-text', 'intl', 'physical-css', 'malformed'].includes(f.rule))
    .map((f) => `${f.rule} ${f.file}: ${f.msg}`);
  const base = JSON.parse(readFileSync(join(root, 'i18n-baseline.json'), 'utf8'));
  // Every file the ledger lists (the files of A0) is in the baseline while it exists. A file added,
  // removed or moved since needs no baseline change (one with text is reported until part A ends).
  for (const f of r.files) if (Object.hasOwn(LEDGER, f.rel) && !Object.hasOwn(base.literal, f.rel)) out.push(`i18n-baseline.json does not list ${f.rel}`);
  // Over the files the baseline lists, Intl calls and physical properties never grow.
  const now = lib.ratchetCounts(root, r.files);
  for (const kind of ['intl', 'physical'] as const) {
    const listed = Object.fromEntries(Object.entries(now[kind] as Record<string, number>).filter(([rel]) => Object.hasOwn(base[kind], rel)));
    if (total(listed) > total(base[kind])) out.push(`${kind}: ${total(listed)}, the baseline's ${total(base[kind])}`);
  }
  out.push(...lib.ledgerProblems(r.files, LEDGER, EXCEPTIONS));
  return out;
}

test('api_012 only format.ts formats', () => {
  const r = checkUi();
  expect(fails(r).filter((f) => f.rule === 'intl')).toEqual([]);
  expect(ratchetProblems(UI, r)).toEqual([]);
  const base = JSON.parse(readFileSync(join(UI, 'i18n-baseline.json'), 'utf8'));
  const now = lib.ratchetCounts(UI, r.files).intl as Record<string, number>;
  console.log(`Intl/toLocale call sites outside format.ts: ${total(now)} in ${Object.values(now).filter((n) => n > 0).length} files (baseline ${total(base.intl)})`);
  // Growth fails; a file on the layer is at zero; format.ts itself is the place for it.
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('</script>', '  const n = (1).toLocaleString();\n</script>')))))).toEqual(['intl']);
  expect(failRules(run(project((d) => edit(d, 'src/lib/labels.ts', (s) => `${s}\nexport const nf = new Intl.NumberFormat();\n`))))).toEqual(['intl']);
  expect(failRules(run(project((d) => put(d, 'src/lib/format.ts', 'export const nf = new Intl.NumberFormat();\n'))))).toEqual([]);
  // A file the baseline does not list (new or moved since) is reported until part A ends.
  const fresh = run(project((d) => put(d, 'src/lib/fresh.ts', 'export const nf = new Intl.NumberFormat();\n')));
  expect(failRules(fresh)).toEqual([]);
  expect(fresh.findings.filter((f) => f.file === 'src/lib/fresh.ts').map((f) => `${f.level} ${f.rule}`)).toEqual(['report intl']);
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
  // Every file the ledger lists is in the baseline (zeros too); other files need not be.
  expect(ratchetProblems(UI, r)).toEqual([]);
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

test('api_012 a new removed or renamed file leaves the ratchets green', () => {
  const r0 = checkUi();
  expect(ratchetProblems(UI, r0)).toEqual([]);
  // Files that hold text and an Intl call or a physical property, not on the layer.
  const pick = (p: (f: Result['files'][number]) => boolean, not = '') => {
    const f = r0.files.find((x) => !x.usesLayer && x.rel !== not && x.literal.length > 0 && p(x));
    if (!f) throw new Error('no such file in the UI');
    return f.rel;
  };
  const removed = pick((f) => f.physical > 0);
  const moved = pick((f) => f.intl > 0, removed);
  const cases: [string, (d: string) => void][] = [
    ['a new file without text', (d) => put(d, 'src/lib/components/Spark.svelte', '<svg viewBox="0 0 8 8"><path d="M0 8 L8 0" /></svg>\n')],
    [`${removed} removed`, (d) => rmSync(join(d, 'src', removed))],
    [
      `${moved} moved to another folder`,
      (d) => {
        mkdirSync(dirname(join(d, 'src/moved', moved)), { recursive: true });
        renameSync(join(d, 'src', moved), join(d, 'src/moved', moved));
      },
    ],
  ];
  for (const [name, change] of cases) {
    const d = uiCopy(change);
    expect(ratchetProblems(d, run(d)), name).toEqual([]);
  }
  // A new file with text is reported, never failed, until part A ends.
  const d = uiCopy((x) => put(x, 'src/pages/Spare.svelte', '<h1>A new page</h1>\n<p>With words.</p>\n'));
  const r = run(d);
  expect(ratchetProblems(d, r)).toEqual([]);
  expect(r.findings.filter((f) => f.file === 'src/pages/Spare.svelte').map((f) => `${f.level} ${f.rule}`)).toEqual(['report literal-text']);
});

test('api_012 the baseline command lowers counts and never raises one', () => {
  const was = JSON.parse(readFileSync(join(project(), 'i18n-baseline.json'), 'utf8'));
  const d = project((x) => {
    edit(x, 'src/pages/Old.svelte', (s) => s.replace('margin-left: 4px;', 'margin-inline-start: 4px;'));
    edit(x, 'src/pages/Cache.svelte', (s) => `${s}<p>Plain words</p>\n`);
    put(x, 'src/pages/Blank.svelte', '<hr />\n');
    put(x, 'src/pages/Wordy.svelte', '<h1>A new page</h1>\n');
    rmSync(join(x, 'src/lib/labels.ts'));
  });
  const b = lib.baselineOf(d, run(d).files, was);
  // Lowered (Old's physical property), never raised (Cache), a new file at zero listed (Blank), a
  // new file above zero not listed (Wordy), a removed file dropped (labels.ts).
  expect(b.physical).toEqual({ 'pages/Blank.svelte': 0, 'pages/Cache.svelte': 0, 'pages/Old.svelte': 0, 'pages/Wordy.svelte': 0 });
  expect(b.literal).toEqual({ 'pages/Blank.svelte': 0, 'pages/Cache.svelte': 0, 'pages/Old.svelte': 2 });
  // With no baseline before it, every count is written.
  expect(lib.baselineOf(d, run(d).files, null).literal).toEqual({ 'pages/Blank.svelte': 0, 'pages/Cache.svelte': 1, 'pages/Old.svelte': 2, 'pages/Wordy.svelte': 1 });
});

test('api_012 the attributes a reader sees or hears are text', () => {
  expect(lib.TEXT_ATTRS).toEqual(['aria-label', 'title', 'placeholder', 'alt', 'aria-roledescription', 'aria-description', 'aria-valuetext', 'aria-placeholder']);
  for (const text of ['<div aria-roledescription="heatmap"></div>', '<div aria-description="Busy hours"></div>', '<div role="slider" aria-valuetext="Ten minutes"></div>', '<div aria-placeholder="Type a name"></div>', '<select><optgroup label="Lists"></optgroup></select>']) {
    expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => `${s}${text}\n`)))), text).toEqual(['literal-text']);
  }
  // `label` is text on an <optgroup> only.
  expect(failRules(run(project((d) => edit(d, 'src/pages/Cache.svelte', (s) => `${s}<option label="x"></option>\n`))))).toEqual([]);
  // The ledger reads them too.
  expect(lib.analyzeFile('pages/X.svelte', '<div aria-roledescription="heatmap"></div>\n', 'x').ledger.markup).toEqual(['heatmap']);
});

test('api_012 migrated files conserve their english strings', () => {
  const r = checkUi();
  expect(r.files.filter((f) => f.usesLayer).map((f) => f.rel)).toContain('lib/format.ts');
  expect(LEDGER['lib/format.ts']).toBeTruthy();
  expect(lib.ledgerProblems(r.files, LEDGER, EXCEPTIONS)).toEqual([]);
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
  // Over the tree: a file on the layer moved to another folder carries its entry, renamed.
  const at = (rel: string, src: string) => lib.analyzeFile(rel, src, lib.contextName(rel));
  const ledger = { 'pages/X.svelte': entry };
  expect(lib.ledgerProblems([at('pages/hosts/X.svelte', ok)], ledger, { 'pages/hosts/X.svelte': [merged] })).toEqual([
    'pages/hosts/X.svelte: the ledger entry of "pages/X.svelte", which is no longer a source file, has its name: rename that entry to "pages/hosts/X.svelte"',
  ]);
  expect(lib.ledgerProblems([at('pages/hosts/X.svelte', ok)], { 'pages/hosts/X.svelte': entry }, { 'pages/hosts/X.svelte': [merged] })).toEqual([]);
  // A removed file, or a moved file not on the layer, needs nothing.
  expect(lib.ledgerProblems([], ledger, {})).toEqual([]);
  expect(lib.ledgerProblems([at('pages/hosts/X.svelte', '<h1>Holds</h1>\n')], ledger, {})).toEqual([]);
  // A new file on the layer: each message is English some entry had, or listed under "new".
  const mod = (e: string) => at('lib/rates.ts', `import { tr } from './i18n';\n\nconst { m } = tr('rates');\nexport const RATE = m('${e}');\n`);
  expect(lib.ledgerProblems([mod('Holds')], ledger, {})).toEqual([]);
  expect(lib.ledgerProblems([mod('Rate-limited')], ledger, {})).toEqual(['lib/rates.ts: message "Rate-limited" is not English any ledger entry had: list it under "new" in ledger-exceptions.json']);
  expect(lib.ledgerProblems([mod('Rate-limited')], ledger, { new: { 'lib/rates.ts': ['Rate-limited'] } })).toEqual([]);
  expect(lib.ledgerProblems([mod('Holds')], ledger, { new: { 'lib/other.ts': ['x'] } })).toEqual(['"new" English for "lib/other.ts": only for a file on the layer without a ledger entry']);
});

test('api_012 a whole sentence per variant conserves from the file own fragments', () => {
  const head = "<script lang=\"ts\">\n  import { tr } from '../lib/i18n';\n  import T from '../lib/i18n/T.svelte';\n  const { t, tn } = tr('x');\n</script>\n";
  const entryOf = (src: string) => {
    const a = lib.analyzeFile('pages/X.svelte', src, 'x');
    return { markup: [...a.ledger.markup].sort(), script: [...a.ledger.script].sort() };
  };
  const after = (src: string) => lib.analyzeFile('pages/X.svelte', src, 'x');
  // A sentence with an {#if} in the middle becomes one message per variant; the variants share the
  // fragments around the block.
  const before = '<script lang="ts">\n  let { mac, ip } = $props();\n</script>\n<p>Queries from {#if mac}this device (<code>{mac}</code>, now at <code>{ip}</code>){:else}<code>{ip}</code>{/if} will show this name.</p>\n';
  const entry = entryOf(before);
  expect(entry.markup).toEqual([')', ', now at', 'Queries from', 'this device (', 'will show this name.']);
  const v1 = 'Queries from this device (<code>{mac}</code>, now at <code>{ip}</code>) will show this name.';
  const v2 = 'Queries from <code>{ip}</code> will show this name.';
  const ok = `${head}<p>{#if mac}<T ctx="x" msg={'${v1}'} params={{ mac, ip }}>{#snippet code(s)}<code>{s}</code>{/snippet}</T>{:else}<T ctx="x" msg={'${v2}'} params={{ ip }}>{#snippet code(s)}<code>{s}</code>{/snippet}</T>{/if}</p>\n`;
  const variants = [
    { sentence: v1, fragments: ['Queries from', 'this device (', ', now at', ')', 'will show this name.'] },
    { sentence: v2, fragments: ['Queries from', 'will show this name.'] },
  ];
  expect(lib.conserve(after(ok), entry, variants)).toEqual([]);
  // A plural made with a suffix: the other form is built from pieces of the file's own strings. A
  // piece belongs to a string the sentence (or an earlier one) already uses, so another string with
  // the same piece ("{} left") is still there for its own message.
  const p0 =
    "<script lang=\"ts\">\n  let { n } = $props();\n  const left = $derived(`${n} voter${n === 1 ? '' : 's'} left`);\n  const spare = $derived(`${n} left`);\n</script>\n<p>{left}</p>\n<p>{spare}</p>\n";
  const pe = entryOf(p0);
  expect(pe.script).toEqual(['s', '{} left', '{} voter{} left']);
  const p1 = `${head}<p>{tn('{n} voter left', '{n} voters left', n, { n })}</p>\n<p>{t('{n} left', { n })}</p>\n`;
  const forms = [
    { sentence: '{n} voter left', fragments: ['{} voter{} left'] },
    { sentence: '{n} voters left', fragments: ['voter', 's', 'left'] },
  ];
  expect(lib.conserve(after(p1), pe, forms)).toEqual([]);
  // What still fails: a fragment the file never had, the fragments out of order, a word added, and
  // a fragment used twice in one sentence (only another exception may share what one took).
  expect(lib.conserve(after(ok), entry, [variants[0], { sentence: v2, fragments: ['Queries from', 'will show that name.'] }]).join('\n')).toContain('"will show that name." is not in this file\'s ledger entry');
  expect(lib.conserve(after(ok), entry, [{ ...variants[0], fragments: ['this device (', 'Queries from', ', now at', ')', 'will show this name.'] }, variants[1]]).join('\n')).toContain("not the sentence's pieces");
  const v3 = v1.replace('this device', 'this very device');
  expect(lib.conserve(after(ok.replace(v1, v3)), entry, [{ ...variants[0], sentence: v3 }, variants[1]]).join('\n')).toContain("not the sentence's pieces");
  expect(lib.conserve(after(p1.replace("'{n} voters left'", "'{n} voterss left'")), pe, [forms[0], { sentence: '{n} voterss left', fragments: ['voter', 's', 's', 'left'] }]).join('\n')).toContain('"s" is not in this file\'s ledger entry');
});

test('api_012 no new physical css properties', () => {
  const r = checkUi();
  expect(fails(r).filter((f) => f.rule === 'physical-css')).toEqual([]);
  expect(ratchetProblems(UI, r)).toEqual([]);
  const base = JSON.parse(readFileSync(join(UI, 'i18n-baseline.json'), 'utf8'));
  const now = lib.ratchetCounts(UI, r.files).physical as Record<string, number>;
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
  // (The fixture's baseline lists app.css at zero.)
  expect(failRules(run(project((d) => put(d, 'src/app.css', '.x {\n  text-align: left;\n}\n'))))).toEqual(['physical-css']);
  // A file the baseline does not list is reported until part A ends.
  const fresh = run(project((d) => put(d, 'src/pages/Fresh.svelte', '<hr />\n<style>\n  hr {\n    margin-left: 1px;\n  }\n</style>\n')));
  expect(failRules(fresh)).toEqual([]);
  expect(fresh.findings.filter((f) => f.file === 'src/pages/Fresh.svelte').map((f) => `${f.level} ${f.rule}`)).toEqual(['report physical-css']);
  expect(failRules(run(project((d) => put(d, 'src/app.css', '.x {\n  margin-inline-start: 1px;\n  text-align: start;\n}\n'))))).toEqual([]);
  expect(failRules(run(project((d) => edit(d, 'src/pages/Old.svelte', (s) => s.replace('margin-left: 4px;', 'margin-inline-start: 4px;')))))).toEqual([]);
});

// REQ: API-012 (ADR-189) — the attribution rule of the pseudo-locale page tests (8-i18n-pseudo.spec.ts),
// as a function of data, so a test without a browser can hold it to fixtures (7-i18n-pseudo-rule.spec.ts).
//
// While part A moves files to the layer one stage at a time, a page holds text of both kinds, so a
// piece of text is held to the rule when it is English that a file on the layer had and no file off
// the layer has (the English ledger says which file had what). A piece passes when it carries a
// pseudo character, or sits under translate="no" (the page test reads only what is not under one).
// Once every file is on the layer this is the design's whole-page rule.
import { LOWER, UPPER } from '../../src/lib/i18n/pseudo';

export type Ledger = Record<string, { markup: string[]; script: string[] }>;

export const PSEUDO = new RegExp(`[${[...LOWER, ...UPPER].join('')}]`);

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const letters = (s: string) => (s.replace(/\{\}/g, '').match(/[A-Za-z]/g) ?? []).length;

/** A path, a key, a type or a format: one token with a separator between two other characters
 * (`a/b`, `data-theme`, `2-digit`). A word with a full stop or a colon after it (`Saved.`,
 * `Canaries:`) is text, so the stop and the colon at the end do not count. */
const CODE = /^\S*[^\s.:][./:_=-][^\s.:]\S*$/;
export const isCode = (s: string): boolean => CODE.test(s.replace(/[.:]+$/, ''));

/** A pattern for English with `{}` for a placeholder: the whole text, a placeholder is any text. */
const whole = (s: string) => new RegExp(`^${s.split('{}').map(escape).join('[\\s\\S]*?')}$`);

/** Values the test server holds that read like English a file on the layer had: data on the page,
 * not text of the layer. Each, by its text, names the file that shows it. Text nodes only: an
 * attribute with the same text is the layer's own. */
export const DATA: Readonly<Record<string, string>> = {
  // The note 1-ui.spec.ts adds, which the dashboard's chart markers list. ChangesControls.svelte
  // has the same words as the note field's placeholder (an attribute: held to the rule).
  'moved the access point to the hallway': 'lib/components/ChangesControls.svelte',
  // The support manifest's value for a field the server could not read, which SupportPackage.svelte
  // shows as it is. ClientChip.svelte and IdentityLine.svelte compare with the same word.
  unknown: 'lib/components/SupportPackage.svelte',
  // The health level the wall's tile shows as it is (WallKpi.svelte). HealthIcon.svelte compares with
  // the same word.
  severe: 'lib/components/wall/WallKpi.svelte',
};

/** A1e: words of the server's own vocabulary (a status, a response code) that a shared file shows as
 * it is: the wall's flow panel (WallFlow.svelte, A1d) writes a node's name in its SVG, which the
 * mocked flow of the wall test names `cached`, `forwarded` and `NOERROR`. The same words are the
 * Monitor pages' English since those pages moved to the layer. Text nodes only. */
export const SERVER_WORDS: ReadonlySet<string> = new Set(['cached', 'forwarded', 'NOERROR']);

export type RuleInput = {
  /** The English ledger: per file, the template text and script strings it had before part A. */
  ledger: Ledger;
  /** The files on the layer. */
  layer: ReadonlySet<string>;
  /** English data outside the layer (the help glossary). */
  help: readonly string[];
  /** Values the test server holds that read like English a file on the layer had: each, by its text,
   * names the file it stands for. They are data in text nodes only, never in an attribute. */
  data: Readonly<Record<string, string>>;
  /** Words of the server's own vocabulary a shared file shows as it is (text nodes only). */
  serverWords: ReadonlySet<string>;
};

export type Rule = {
  /** The English only files on the layer had that the rule holds the page to. */
  own: { s: string; rel: string }[];
  /** The English of the files off the layer. */
  off: ReadonlySet<string>;
  /** The pieces of the page that are English only files on the layer had (empty: the rule holds):
   * `text` the text nodes, `other` the attributes a reader sees or hears and the canvas text. */
  found(text: readonly string[], other: readonly string[]): string[];
};

export function makeRule({ ledger, layer, help, data, serverWords }: RuleInput): Rule {
  const off = new Set([...Object.entries(ledger).filter(([rel]) => !layer.has(rel)).flatMap(([, e]) => [...e.markup, ...e.script]), ...help]);
  // Each as a whole-text pattern. A pattern with a placeholder needs words of its own (eight
  // letters), or it would match any text.
  const own = Object.entries(ledger)
    .filter(([rel]) => layer.has(rel))
    .flatMap(([rel, e]) => [...e.markup, ...e.script].map((s) => ({ s, rel })))
    .filter(({ s }) => !off.has(s) && !isCode(s) && /[A-Za-z]{2}/.test(s.replace(/\{\}/g, '')) && (!s.includes('{}') || letters(s) >= 8));
  const patterns = own.map(({ s, rel }) => ({ re: whole(s), from: `${rel}: ${JSON.stringify(s)}` }));
  // The English of files off the layer, as the same patterns: a piece that is that English belongs
  // to those files, whatever pattern of a layer file it also matches.
  const offPatterns = [...off].filter((s) => s.includes('{}') && !isCode(s) && letters(s) >= 8).map(whole);
  return {
    own,
    off,
    found(text, other) {
      const found: string[] = [];
      for (const p of new Set([...text, ...other])) {
        // A piece an off-layer file has belongs to that file, whatever pattern of a layer file it
        // also matches (`{}query log {}` matched whole sentences of the Settings and Heatmap pages,
        // and `{}% of the budget left` the wall's "{}: {} % of the budget left").
        if (PSEUDO.test(p) || off.has(p)) continue;
        // Data, and a server's word the page shows as it is: as a text node only (an attribute or
        // canvas text is the page's own).
        if ((Object.hasOwn(data, p) || serverWords.has(p)) && text.includes(p) && !other.includes(p)) continue;
        const m = patterns.find((o) => o.re.test(p));
        if (m && !offPatterns.some((re) => re.test(p))) found.push(`${JSON.stringify(p)} (${m.from})`);
      }
      return found;
    },
  };
}

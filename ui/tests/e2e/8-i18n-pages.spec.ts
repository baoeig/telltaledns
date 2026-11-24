// REQ: API-012 (ADR-189) — languages in the web UI, in the browser against the real server: an
// English reader downloads nothing but the core, and `?lang=` applies to one page load and stores
// nothing. Expects the state 1-ui.spec.ts leaves (the admin `admin`, some queries in the log), as
// 2-shots.spec.ts does. (The parts that need no browser are 7-i18n.spec.ts.)
import { expect, test, type Page } from '@playwright/test';

const ADMIN = { user: 'admin', pass: 'correct horse battery' };
// The core: files directly under /assets/, of these kinds. No subfolder (assets/locales/), no JSON.
const CORE = /^\/assets\/[^/]+\.(js|css|svg|png|woff2)$/;

async function signIn(page: Page) {
  await page.goto('/');
  await page.getByLabel('Username', { exact: true }).fill(ADMIN.user);
  await page.getByLabel('Password', { exact: true }).fill(ADMIN.pass);
  await page.getByRole('button', { name: 'Sign in' }).click();
  // As 1-ui.spec.ts finds it: the heading's accessible name holds more than the word (and Argon2 is slow).
  await expect(page.getByRole('heading', { name: 'Dashboard' }).first()).toBeVisible({ timeout: 30_000 });
}

test('api_012 an english session requests only core assets', async ({ page }) => {
  const assets: string[] = [];
  page.on('request', (r) => {
    const path = new URL(r.url()).pathname;
    if (path.startsWith('/assets/')) assets.push(path);
  });
  await signIn(page);
  for (const [path, heading] of [
    ['/#/', 'Dashboard'],
    ['/#/queries', 'Query log'],
    ['/#/settings', 'Settings'],
  ]) {
    await page.goto(path);
    await expect(page.getByRole('heading', { name: heading }).first()).toBeVisible();
  }
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  expect(assets.length, 'the page loaded its assets').toBeGreaterThan(0);
  expect(assets.filter((p) => !CORE.test(p))).toEqual([]);
});

test('api_012 lang parameter applies to one page load and stores nothing', async ({ page }) => {
  await signIn(page);
  const rows = page.locator('table.log tbody tr');
  const storage = () => page.evaluate(() => ({ local: { ...localStorage }, session: { ...sessionStorage } }));
  await page.goto('/#/queries');
  await expect(rows.first()).toBeVisible();
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  const before = await storage();
  // This page load is the pseudo-locale: the formats' messages are accented ("ms" -> "ɱš").
  await page.goto('/?lang=en-XA#/queries');
  await expect(page.locator('html')).toHaveAttribute('lang', 'en-XA');
  await expect(page.locator('html')).toHaveAttribute('data-i18n-early', '0');
  await expect(rows.first()).toContainText('š');
  expect(await storage()).toEqual(before);
  // The next load without it is English again, and nothing was kept.
  await page.goto('/#/queries');
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  await expect(rows.first()).toBeVisible();
  expect(await page.locator('table.log').innerText()).not.toContain('š');
  expect(await storage()).toEqual(before);
  // A language that has no catalog: English, and nothing stored either.
  await page.goto('/?lang=zh-Hans#/queries');
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  await expect(rows.first()).toBeVisible();
  expect(await storage()).toEqual(before);
});

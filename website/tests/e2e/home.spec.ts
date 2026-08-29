import { test, expect } from '@playwright/test';

test.describe('homepage', () => {
  test('loads wordmark, headline, download, and copy control', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveTitle(/MarkRust/);
    await expect(page.getByRole('link', { name: 'MarkRust home' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'MarkRust home' })).toContainText('MarkRust');
    await expect(page.locator('.hero__title')).toContainText('Markdown');
    await expect(page.locator('.hero__title')).toContainText('without the machinery');
    await expect(page.locator('#download')).toBeVisible();
    await expect(page.locator('#download h2')).toHaveText('Download');
    const copy = page.locator('.hero__copy');
    await expect(copy).toBeVisible();
    await expect(copy).toHaveAttribute(
      'data-copy',
      /cargo install --git https:\/\/github.com\/alexey-a-abramov\/markrust markrust/,
    );
  });

  test('primary nav links Features, Download, Docs, GitHub', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveTitle(/MarkRust/);
    const nav = page.locator('nav[aria-label="Primary"]');
    await expect(nav.getByRole('link', { name: 'Features' })).toHaveAttribute('href', '/#features');
    await expect(nav.getByRole('link', { name: 'Download' })).toHaveAttribute('href', '/#download');
    await expect(nav.getByRole('link', { name: 'Docs' })).toHaveAttribute('href', '/docs');
    await expect(nav.getByRole('link', { name: 'GitHub' })).toHaveAttribute(
      'href',
      'https://github.com/alexey-a-abramov/markrust',
    );
  });

  test('install copy button writes the cargo command', async ({ page, context }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    await page.goto('/');
    await expect(page).toHaveTitle(/MarkRust/);
    const copy = page.locator('.hero__copy');
    await copy.click();
    await expect(copy).toHaveClass(/is-copied/);
    const text = await page.evaluate(() => navigator.clipboard.readText());
    expect(text).toContain('cargo install --git https://github.com/alexey-a-abramov/markrust markrust');
  });

  test('theme toggle persists in localStorage', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveTitle(/MarkRust/);
    const html = page.locator('html');
    const toggle = page.locator('.theme-toggle');
    await expect(toggle).toBeVisible();

    const before = await html.getAttribute('data-theme');
    await toggle.click();
    const after = await html.getAttribute('data-theme');
    expect(after).toMatch(/^(light|dark)$/);
    expect(after).not.toEqual(before ?? null);

    const stored = await page.evaluate(() => localStorage.getItem('markrust-theme'));
    expect(stored).toBe(after);

    await page.reload();
    await expect(html).toHaveAttribute('data-theme', after);
    const storedAfterReload = await page.evaluate(() => localStorage.getItem('markrust-theme'));
    expect(storedAfterReload).toBe(after);
  });

  test('skip-to-content moves focus to main', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveTitle(/MarkRust/);
    const skip = page.locator('.skip-link');
    await expect(skip).toHaveAttribute('href', '#main-content');
    await page.keyboard.press('Tab');
    await expect(skip).toBeFocused();
    await skip.press('Enter');
    await expect(page.locator('#main-content')).toBeVisible();
    await expect(page).toHaveURL(/#main-content/);
  });
});

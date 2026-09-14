import { test, expect } from '@playwright/test';

test.describe('docs', () => {
  test('docs index renders introduction', async ({ page }) => {
    await page.goto('/docs');
    await expect(page).toHaveTitle(/MarkRust/);
    await expect(page.locator('h1')).toContainText('Introduction');
    await expect(page.getByRole('link', { name: 'Install MarkRust' })).toBeVisible();
    await expect(page.locator('.docs-nav')).toContainText('Installation');
  });

  test('install, markdown, and cli pages load', async ({ page }) => {
    await page.goto('/docs/install');
    await expect(page.locator('h1')).toHaveText('Installation');
    await expect(page.locator('#requirements')).toBeVisible();

    await page.goto('/docs/markdown');
    await expect(page.locator('h1')).toHaveText('Markdown');
    await expect(page.locator('#gfm')).toBeVisible();

    await page.goto('/docs/cli');
    await expect(page.locator('h1')).toHaveText('CLI');
    await expect(page.locator('code').filter({ hasText: 'markrust export README.md' })).toBeVisible();
  });

  test('docs nav current page is marked', async ({ page }) => {
    await page.goto('/docs/install');
    await expect(page.locator('.docs-nav a[href="/docs/install"]')).toHaveAttribute(
      'aria-current',
      'page',
    );
    await expect(page.locator('.docs-nav a[href="/docs/install"]')).toHaveText('Installation');
  });

  test('roadmap and architecture reflect the current editor', async ({ page }) => {
    await page.goto('/docs/roadmap');
    await expect(page.getByRole('heading', { name: 'Current v0.1 alpha release gate' })).toBeVisible();
    await expect(page.getByText('Typora-style WYSIWYG editing, plus Source and Split modes')).toBeVisible();
    await expect(page.getByText('Manual macOS CJK IME validation', { exact: false })).toBeVisible();

    await page.goto('/docs/architecture');
    const article = page.locator('.docs-article');
    await expect(article).toContainText('Comrak is the shared Markdown grammar');
    await expect(article).not.toContainText('Tree-sitter provides incremental CST parsing on the hot path');
  });

  test('image documentation makes remote loading an explicit choice', async ({ page }) => {
    await page.goto('/docs/images');
    const article = page.locator('.docs-article');
    await expect(article.getByRole('heading', { name: 'Remote images' })).toBeVisible();
    await expect(article).toContainText('Opening a Markdown file never sends remote-image requests automatically');
    await expect(article).toContainText('Load remote images');
    await expect(article).toContainText('public https:// image URLs');
  });

  test('command palette documents the remote-image action', async ({ page }) => {
    await page.goto('/docs/command-palette');
    const article = page.locator('.docs-article');
    await expect(article).toContainText('Load remote images for the current tab');
    await expect(article).not.toContainText('Toggle light/dark theme');
  });
});

test.describe('docs mobile menu', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('menu toggle opens the documentation sidebar', async ({ page }) => {
    await page.goto('/docs/install');
    const toggle = page.locator('#docs-nav-toggle');
    await expect(toggle).toBeVisible();
    const sidebar = page.locator('.docs-sidebar');
    await expect(sidebar).not.toHaveClass(/is-open/);
    await toggle.click();
    await expect(sidebar).toHaveClass(/is-open/);
    await expect(sidebar.getByRole('link', { name: 'CLI' })).toBeVisible();
  });
});

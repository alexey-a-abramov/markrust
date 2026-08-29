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
    await expect(page.getByText('markrust export')).toBeVisible();
  });

  test('docs nav current page is marked', async ({ page }) => {
    await page.goto('/docs/install');
    await expect(page.locator('.docs-nav a[href="/docs/install"]')).toHaveAttribute(
      'aria-current',
      'page',
    );
    await expect(page.locator('.docs-nav a[href="/docs/install"]')).toHaveText('Installation');
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

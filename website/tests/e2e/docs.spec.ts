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
    await expect(page.getByRole('heading', { name: 'Current v0.7.0 release gates' })).toBeVisible();
    await expect(page.getByText('Typora-style WYSIWYG editing, plus Source and Split modes')).toBeVisible();
    await expect(page.getByText('Manual macOS CJK IME validation', { exact: false })).toBeVisible();

    await page.goto('/docs/architecture');
    const article = page.locator('.docs-article');
    await expect(article).toContainText('Comrak is the shared Markdown grammar');
    await expect(article).not.toContainText('Tree-sitter provides incremental CST parsing on the hot path');
  });

  test('distribution status links verified published downloads and retains platform limits', async ({ page }) => {
    await page.goto('/docs/install');
    const article = page.locator('.docs-article');
    await expect(article).toContainText('Confirm available downloads on the releases page');
    await expect(article).toContainText('Download and restart require your confirmation');
    await expect(article).toContainText('In-app replacement is not supported on Windows or Linux yet');
    await expect(article).toContainText('Windows: experimental x86_64 download');
    await expect(article).toContainText('recovery-directory privacy/locking acceptance remain pending');
    await expect(article).toContainText('not Developer-ID signed or notarized');
    await expect(article).not.toContainText('planned for v0.2');

    await page.goto('/');
    const download = page.locator('#download');
    await expect(download).toContainText('v0.8.1 alpha');
    await expect(download).toContainText('v0.8.1 binary archives are published');
    await expect(download.getByRole('link', { name: 'ARM', exact: true })).toHaveAttribute('href', 'https://github.com/alexey-a-abramov/markrust/releases/download/v0.8.1/markrust-macos-aarch64.tar.gz');
    await expect(download.getByRole('link', { name: 'Intel', exact: true })).toHaveAttribute('href', 'https://github.com/alexey-a-abramov/markrust/releases/download/v0.8.1/markrust-macos-x86_64.tar.gz');
    await expect(download.getByRole('row').filter({ hasText: 'Linux' }).getByRole('link')).toHaveAttribute('href', 'https://github.com/alexey-a-abramov/markrust/releases/download/v0.8.1/markrust-linux-x86_64.tar.gz');
    await expect(download.getByRole('row').filter({ hasText: 'Windows' }).getByRole('link')).toHaveAttribute('href', 'https://github.com/alexey-a-abramov/markrust/releases/download/v0.8.1/markrust-windows-x86_64-experimental.zip');
    await expect(download.getByRole('row').filter({ hasText: 'Windows' })).toContainText('GUI/privacy QA pending');
    await expect(download).not.toContainText('not yet supported');
  });

  test('find, open location, and theme shortcuts match the editor commands', async ({ page }) => {
    await page.goto('/docs/search');
    const search = page.locator('.docs-article');
    await expect(search).toContainText('In-document find is implemented in v0.7.0');
    await expect(search).toContainText('⌘F');
    await expect(search).toContainText('Ctrl+F');
    await expect(search).toContainText('Workspace-wide search and replace are not implemented yet');
    await expect(search).not.toContainText('dedicated find interface and workspace-wide search are not available');

    await page.goto('/docs/shortcuts');
    const shortcuts = page.locator('.docs-article');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Open location' })).toContainText('⌘⇧L');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Find in document' })).toContainText('⌘F');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Next match' })).toContainText('⌘G');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Previous match' })).toContainText('⌘⇧G');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Toggle light / dark appearance' })).toContainText('⌘⌥⇧T');
    await expect(shortcuts.getByRole('row').filter({ hasText: 'Cut / copy / paste' })).toContainText('Ctrl+C');
  });

  test('draft recovery documentation does not promise implicit document-file autosave', async ({ page }) => {
    await page.goto('/docs/first-document');
    await expect(page.locator('.docs-article')).toContainText('Draft checkpoints are separate from the document file');
    await expect(page.locator('.docs-article')).toContainText('Save explicitly writes the file');
    await expect(page.locator('.docs-article')).not.toContainText('default 1 second');
    await page.goto('/docs/configuration');
    await expect(page.locator('.docs-article')).toContainText('autosave_ms = 150');
    await expect(page.locator('.docs-article')).toContainText('does not implicitly save over the original document file');
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

  test('long cross-platform shortcuts do not widen the mobile document', async ({ page }) => {
    await page.goto('/docs/shortcuts');
    await expect(page.getByRole('row').filter({ hasText: 'Toggle light / dark appearance' })).toContainText('Ctrl+Alt+Shift+T');
    const width = await page.evaluate(() => ({
      page: document.documentElement.scrollWidth,
      viewport: window.innerWidth,
    }));
    expect(width.page).toBeLessThanOrEqual(width.viewport);
  });

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

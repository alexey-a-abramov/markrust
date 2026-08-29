import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const websiteRoot = join(dirname(fileURLToPath(import.meta.url)), '../..');
const pagesDir = join(websiteRoot, 'src/pages');
const navSource = readFileSync(join(websiteRoot, 'src/data/docs-nav.ts'), 'utf8');

function hrefToPage(href) {
  const trimmed = href.replace(/\/$/, '');
  if (trimmed === '/docs') {
    return join(pagesDir, 'docs/index.astro');
  }
  return join(pagesDir, `${trimmed}.astro`);
}

test('every docs-nav href resolves to an existing page', () => {
  const hrefs = [...navSource.matchAll(/href:\s*'([^']+)'/g)].map((match) => match[1]);
  assert.ok(hrefs.length >= 10, `expected docs nav links, got ${hrefs.length}`);
  const missing = hrefs.filter((href) => !existsSync(hrefToPage(href)));
  assert.deepEqual(missing, [], `missing pages for ${missing.join(', ')}`);
});

test('docs-nav hrefs are unique', () => {
  const hrefs = [...navSource.matchAll(/href:\s*'([^']+)'/g)].map((match) => match[1]);
  assert.equal(new Set(hrefs).size, hrefs.length);
});

test('every docs page is listed in the nav (except none)', () => {
  const hrefs = new Set(
    [...navSource.matchAll(/href:\s*'([^']+)'/g)].map((match) => match[1]),
  );
  const docsDir = join(pagesDir, 'docs');
  const files = readdirSync(docsDir).filter((name) => name.endsWith('.astro'));
  const unlisted = files.filter((name) => {
    const href = name === 'index.astro' ? '/docs' : `/docs/${name.replace(/\.astro$/, '')}`;
    return !hrefs.has(href);
  });
  assert.deepEqual(unlisted, [], `docs pages missing from nav: ${unlisted.join(', ')}`);
});

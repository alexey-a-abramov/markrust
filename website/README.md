# MarkRust Website

Product landing page and documentation for [markrust.org](https://markrust.org).

Built with [Astro](https://astro.build) — static HTML, minimal JavaScript, excellent performance.

## Development

```bash
pnpm install          # from repo root (installs website deps)
pnpm dev              # http://localhost:4321
```

Or from `website/`:

```bash
cd website
pnpm install
pnpm dev
```

## Build

```bash
pnpm build            # from repo root
```

## Tests

```bash
pnpm test            # unit + Playwright e2e (from repo root or website/)
pnpm test:unit       # docs-nav page resolution
pnpm test:e2e        # Playwright against production preview
```

E2e builds the site, then serves `pnpm preview` on http://127.0.0.1:4371 (Playwright starts its own server; it does not reuse port 4321).

## Structure

```
src/
  components/   Reusable UI (Header, CodeBlock, AppMockup, …)
  layouts/      BaseLayout, DocsLayout
  pages/        Landing (index) + /docs/*
  styles/       Design tokens + global CSS
  data/         Documentation navigation
public/         favicon, theme.js
```

## Design

- Typography-driven technical instrument aesthetic
- IBM Plex Sans + IBM Plex Mono + Source Serif 4
- Restrained rust accent on graphite/paper palette
- Dark mode designed independently (not inverted light)

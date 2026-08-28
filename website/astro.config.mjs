import { defineConfig } from 'astro/config';

export default defineConfig({
  site: 'https://markrust.org',
  compressHTML: true,
  build: {
    inlineStylesheets: 'auto',
  },
});

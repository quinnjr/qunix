// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import tailwindcss from '@tailwindcss/vite';

// GitHub Pages serves a project site under /<repo>, so every absolute asset
// path has to carry that prefix. Getting `base` wrong produces a site that
// works locally and 404s on every stylesheet once deployed.
export default defineConfig({
  site: 'https://quinnjr.github.io/qunix',
  base: '/qunix',
  trailingSlash: 'always',
  integrations: [sitemap()],
  vite: { plugins: [tailwindcss()] },
  build: {
    // Deliberately *not* 'always'. Inlining put the whole Tailwind bundle into
    // every page -- 54-64 KB each, re-downloaded on every navigation. As one
    // external file it is fetched once and cached for the rest of the visit,
    // which matters more here than saving a round trip on first paint, because
    // this is a multi-page site people read front to back.
    inlineStylesheets: 'auto',
  },
  markdown: {
    shikiConfig: { theme: 'github-dark-default', wrap: false },
  },
});

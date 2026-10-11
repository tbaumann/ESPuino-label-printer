import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'

// The build output is served by the Rust process from server/static.
// `npm run dev` proxies /api to a locally running server instead.
export default defineConfig({
  plugins: [svelte()],
  build: {
    // Default target is what the Rust process serves in local use. The Nix
    // build overrides this so the output stays inside the source tree.
    outDir: process.env.ESPUINO_WEB_OUT ?? '../server/static',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/api-docs': 'http://127.0.0.1:8080',
    },
  },
})

import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Served by stemsd under /remote/; in dev, API calls are proxied to it.
export default defineConfig({
  base: '/remote/',
  plugins: [svelte()],
  build: { outDir: 'dist', emptyOutDir: true, target: 'es2022' },
  server: {
    host: true,
    port: 5174,
    fs: { allow: ['..'] },
    proxy: {
      '/api': { target: 'http://127.0.0.1:7373', ws: true, changeOrigin: true }
    }
  }
});

import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  server: { host: '127.0.0.1', port: 1420, strictPort: true },
  build: { target: 'es2022', cssCodeSplit: true, sourcemap: false }
});

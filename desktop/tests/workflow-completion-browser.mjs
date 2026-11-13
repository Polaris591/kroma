// Temporary strict-port driver for the workflow-completion browser suite.
// Never uses the running dev server (1420) or the real Desktop data.
import { createServer } from 'vite';
import { chromium } from 'file:///C:/Users/24524/AppData/Local/npm-cache/_npx/9833c18b2d85bc59/node_modules/playwright/index.mjs';
import path from 'node:path';
import { verifyWorkflowCompletion } from './workflow-completion-ui.mjs';

const root = path.resolve(import.meta.dirname, '..');
const server = await createServer({
  root,
  configFile: path.join(root, 'vite.config.ts'),
  clearScreen: false,
  server: { host: '127.0.0.1', port: 1431, strictPort: true },
});
await server.listen();
const baseURL = 'http://127.0.0.1:1431/';
const browser = await chromium.launch({ channel: 'msedge', headless: true });
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  const passed = await verifyWorkflowCompletion(page, baseURL);
  console.log(`workflow-completion: ${passed.length} checks passed`);
} finally {
  await browser.close();
  await server.close();
}

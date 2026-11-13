import { readFile, writeFile, mkdir } from 'node:fs/promises';
import path from 'node:path';

const fixture = await readFile(new URL('./fixtures/desktop.mjs', import.meta.url), 'utf8');
const output = process.argv[2] || '.playwright-mcp';

async function build(module, name, exported) {
  const checks = await readFile(new URL(module, import.meta.url), 'utf8');
  const source = [
    '(function () {',
    fixture.replace('export async function', 'async function'),
    checks.replace("import { installDesktopFixture } from './fixtures/desktop.mjs';", '')
      .replace('export async function', 'async function'),
    `return ${exported};`,
    '})()',
  ].join('\n');
  const file = path.resolve(output, name);
  await writeFile(file, source);
  console.log(`Playwright Page check driver: ${file}`);
}

await mkdir(output, { recursive: true });
await build('./ui-smoke.mjs', 'desktop-ui-check.js', 'verifyDesktopUi');
await build('./settings-ui.mjs', 'desktop-settings-check.js', 'verifyDesktopSettings');
await build('./capabilities-ui.mjs', 'desktop-capabilities-check.js', 'verifyDesktopCapabilities');
await build('./workflow-completion-ui.mjs', 'desktop-workflow-completion-check.js', 'verifyWorkflowCompletion');

import { installDesktopFixture } from './fixtures/desktop.mjs';

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyFileSuggestions(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page);
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const suggest = page.locator('.file-suggest');
  const row = text => page.locator('.file-suggest-row', { hasText: text });
  const sentTexts = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'send_message').map(call => call.args.text));
  const hold = query => page.evaluate(
    query => window.__KROMA_UI_FIXTURE__.hold('project_files', query), query);
  const release = (query, error) => page.evaluate(
    ({ query, error }) => window.__KROMA_UI_FIXTURE__.release('project_files', error, query),
    { query, error });
  const requested = query => page.waitForFunction(
    query => window.__KROMA_UI_FIXTURE__.calls
      .some(call => call.command === 'project_files' && call.args.query === query),
    query);

  // a -> b -> a: the first "alpha" answer arrives last and must not replace the
  // candidates the second "alpha" query already installed.
  await hold('alpha');
  await editor.fill('@alpha');
  await requested('alpha');
  check(await suggest.count() === 0, 'A held query already showed candidates');
  await editor.fill('@beta');
  await row('src/beta-1.ts').waitFor();
  await editor.fill('@alpha');
  await row('src/alpha-2.ts').waitFor();
  await release('alpha');
  await page.waitForTimeout(300);
  check(await row('src/alpha-2.ts').count() === 1, 'The newest same-query answer was replaced');
  check(await row('src/alpha-1.ts').count() === 0, 'A superseded same-query answer installed stale candidates');
  passed.push('a->b->a keeps only the newest answer');

  // A failure that a newer query already superseded leaves its results alone.
  await hold('gamma');
  await editor.fill('@gamma');
  await requested('gamma');
  await editor.fill('@delta');
  await row('src/delta-1.ts').waitFor();
  await release('gamma', '项目文件读取失败');
  await page.waitForTimeout(300);
  check(await row('src/delta-1.ts').count() === 1, 'A superseded failure cleared the current candidates');
  passed.push('a superseded failure never clears newer candidates');

  // While a replacement query is still debouncing, the previous candidates are
  // neither shown nor insertable.
  await editor.fill('@zeta');
  await row('src/zeta-1.ts').waitFor();
  await hold('zeta9');
  await editor.fill('@zeta9');
  const hidden = await page.waitForFunction(
    () => !document.querySelector('.file-suggest'), null, { timeout: 900 })
    .then(() => true, () => false);
  check(hidden, 'Candidates of the replaced query stayed visible during the debounce window');
  await editor.press('Tab');
  check(await editor.inputValue() === '@zeta9', 'Tab inserted a candidate of the replaced query');
  await release('zeta9');
  await row('src/zeta9-1.ts').waitFor();
  passed.push('debounce of a replacement query hides the previous candidates');

  // IME confirmation inserts and sends nothing; a confirmed Enter still inserts.
  await editor.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true });
  check(await editor.inputValue() === '@zeta9', 'IME confirmation inserted a file reference');
  check(await suggest.count() === 1, 'IME confirmation closed the candidate list');
  check((await sentTexts()).length === 0, 'IME confirmation sent a task');
  await editor.press('Enter');
  check(await editor.inputValue() === '@src/zeta9-1.ts ', 'A confirmed Enter did not insert the selected file');
  passed.push('IME confirmation keeps candidates and a confirmed Enter inserts');

  // Switching sessions must not carry the previous session's candidates over.
  await editor.fill('@omega');
  await row('src/omega-1.ts').waitFor();
  await page.locator('.session-row').filter({ hasText: '整理项目的下一步' })
    .locator('.session-main').click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .some(call => call.command === 'switch_session'));
  await page.waitForFunction(() => document.querySelector('.composer textarea').value === '');
  check(await suggest.count() === 0, 'Switching sessions kept the previous session candidates');
  passed.push('switching sessions drops the previous suggestions');

  // Starting a task replaces the draft, so pending candidates go with it.
  await editor.fill('@sigma');
  await row('src/sigma-1.ts').waitFor();
  await page.getByRole('button', { name: '发送任务', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .some(call => call.command === 'send_message'));
  check(await suggest.count() === 0, 'Starting a task kept the previous suggestion list');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish());
  passed.push('starting a task drops the suggestion list');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

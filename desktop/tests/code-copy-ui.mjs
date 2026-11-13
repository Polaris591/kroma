import { installDesktopFixture } from './fixtures/desktop.mjs';

// Indentation, a blank line, HTML characters and the newline before the closing
// fence are exactly what a naive copy loses.
const TS_BLOCK = 'const pad = (n) =>\n  n < 10 ? "0" + String(n) : String(n);\n\n// 空行与缩进都要保留\nconst longPath = "src/very/long/path/that/keeps/going/and/going/for/the/narrow/window.ts";';
const SH_BLOCK = 'echo "<b>&amp;</b>"';
const TS_TEXT = `${TS_BLOCK}\n`;
const SH_TEXT = `${SH_BLOCK}\n`;

// One answer holding two fenced blocks, an inline code span and the unsafe
// Markdown the existing sanitizer must keep removing.
function answer(code) {
  return [
    '先看下面两段实现。',
    '',
    '```ts',
    code,
    '```',
    '',
    '调用 `pad(3)` 即可。',
    '',
    '```sh',
    SH_BLOCK,
    '```',
    '',
    '<script>window.__UI_TEST_ATTACKED__ = true</script>',
    '',
    '[外链](https://example.invalid/code-copy) 与 [危险](javascript:window.__UI_TEST_ATTACKED__=true)',
  ].join('\n');
}

const same = (left, right) => JSON.stringify(left) === JSON.stringify(right);

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyCodeCopy(page, baseURL = 'http://127.0.0.1:1427/') {
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

  // The clipboard is controllable, so a receipt can arrive after the block it
  // was asked for has been replaced.
  await page.evaluate(() => {
    const clipboard = {
      texts: [], hold: false, failNext: false, pending: [],
      writeText(text) {
        clipboard.texts.push(text);
        if (clipboard.failNext) {
          clipboard.failNext = false;
          return Promise.reject(new Error('合成剪贴板失败'));
        }
        if (!clipboard.hold) return Promise.resolve();
        return new Promise((resolve, reject) => clipboard.pending.push({ resolve, reject }));
      },
    };
    window.__KROMA_CLIPBOARD__ = clipboard;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: clipboard });
  });
  const copied = () => page.evaluate(() => [...window.__KROMA_CLIPBOARD__.texts]);
  const resetClipboard = () => page.evaluate(() => {
    Object.assign(window.__KROMA_CLIPBOARD__, { texts: [], hold: false, failNext: false });
  });
  const holdClipboard = value => page.evaluate(hold => { window.__KROMA_CLIPBOARD__.hold = hold; }, value);
  const answerClipboard = error => page.evaluate(message => {
    const pending = window.__KROMA_CLIPBOARD__.pending.shift();
    if (!pending) throw new Error('No pending clipboard write');
    if (message) pending.reject(new Error(message)); else pending.resolve();
  }, error ?? '');
  // Nothing observable follows a dropped receipt, so wait long enough for the
  // superseded promise to have resolved and then check it changed nothing.
  const settle = () => page.waitForTimeout(150);
  const labels = selector => page.$$eval(selector, nodes => nodes.map(node => (node.textContent ?? '').trim()));
  const waitLabel = (selector, index, text) => page.waitForFunction(({ selector, index, text }) =>
    (document.querySelectorAll(selector)[index]?.textContent ?? '').trim() === text, { selector, index, text });
  const blockTexts = selector => page.$$eval(selector, nodes => nodes.map(node => node.textContent));
  // Wrappers, bars and buttons must stay one-to-one, and no control may sit
  // inside the code it copies.
  const structure = selector => page.evaluate(selector => {
    const root = document.querySelector(selector);
    if (!root) return null;
    return {
      blocks: root.querySelectorAll('.code-block').length,
      bars: root.querySelectorAll('.code-block-bar').length,
      buttons: root.querySelectorAll('.code-copy').length,
      nested: root.querySelectorAll('.code-block .code-block').length,
      blockCode: root.querySelectorAll('.code-block pre > code').length,
      insideCode: root.querySelectorAll('code button').length,
      inlineCode: root.querySelectorAll(':not(pre) > code').length,
    };
  }, selector);
  const setAnswer = content => page.evaluate(value => {
    window.__KROMA_UI_FIXTURE__.state.snapshot.messages.at(-1).content = value;
  }, content);
  const connectCalls = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'connect').length);
  // Changing the model through the existing control re-reads the snapshot, so
  // an edited answer updates in place instead of reloading the window.
  const refresh = async index => {
    const before = await connectCalls();
    await page.getByRole('button', { name: '选择模型与思考强度', exact: true }).click();
    await page.locator('.model-menu-row').nth(index).click();
    await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
      .filter(call => call.command === 'connect').length > before, before);
  };
  const waitAnswerText = text => page.waitForFunction(expected =>
    document.querySelector('.transcript-inner .code-block pre > code')?.textContent === expected, text);
  const buttons = page.locator('.transcript-inner .code-copy');
  const externalCalls = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'open_external').length);

  // 1. Every fenced block gets one native control; inline code gets none, and
  //    each control copies exactly its own block.
  await setAnswer(answer(TS_BLOCK));
  await refresh(1);
  await waitAnswerText(TS_TEXT);
  let transcript = await structure('.transcript-inner');
  check(transcript && transcript.blocks === 2 && transcript.bars === 2 && transcript.buttons === 2,
    'The two fenced blocks did not each get one copy control');
  check(transcript.nested === 0 && transcript.blockCode === 2, 'A code block was wrapped twice or lost its code');
  check(transcript.insideCode === 0, 'A copy control was placed inside the code it copies');
  check(transcript.inlineCode === 1, 'The answer no longer contains exactly one inline code span');
  check(same(await blockTexts('.transcript-inner .code-block pre > code'), [TS_TEXT, SH_TEXT]),
    'The rendered block text is not the fenced source itself');
  check((await buttons.nth(0).boundingBox()).y + 1 <=
    (await page.locator('.transcript-inner .code-block pre').nth(0).boundingBox()).y,
    'The copy control overlaps the code it copies');

  await resetClipboard();
  // Keyboard first, so success is observable on a control that starts idle.
  await buttons.nth(1).focus();
  await page.keyboard.press('Enter');
  await waitLabel('.transcript-inner .code-copy', 1, '已复制');
  await buttons.nth(0).click();
  await waitLabel('.transcript-inner .code-copy', 0, '已复制');
  check(same(await copied(), [SH_TEXT, TS_TEXT]), 'Copy took another block or changed its text');
  check(same(await labels('.transcript-inner .code-copy'), ['已复制', '已复制']),
    'Copying one block changed the feedback of another');
  passed.push('each fenced block copies its own exact text and inline code gets no control');

  // 2. A failure stays retryable and speaks only for its own block.
  await resetClipboard();
  await page.evaluate(() => { window.__KROMA_CLIPBOARD__.failNext = true; });
  await buttons.nth(1).click();
  await waitLabel('.transcript-inner .code-copy', 1, '复制失败，重试');
  check(same(await labels('.transcript-inner .code-copy'), ['已复制', '复制失败，重试']),
    'A copy failure changed the feedback of the other block');
  await buttons.nth(1).click();
  await waitLabel('.transcript-inner .code-copy', 1, '已复制');
  check((await copied()).filter(text => text === SH_TEXT).length === 2, 'The retry did not copy the block again');
  passed.push('a failed copy is retryable and never resets another block');

  // 3. Sanitizing and the HTTP(S) link interception still apply, and a copy
  //    click is not a link click.
  check(await page.evaluate(() => !window.__UI_TEST_ATTACKED__
    && !document.querySelector('.transcript-inner .markdown script')), 'Unsafe Markdown survived sanitizing');
  check(await page.locator('.transcript-inner .markdown a[href^="javascript:"]').count() === 0,
    'A javascript: link kept its href');
  const beforeLinks = await externalCalls();
  const pageUrl = page.url();
  await buttons.nth(0).click();
  await settle();
  check(await externalCalls() === beforeLinks, 'Copying a block opened an external link');
  await page.locator('.transcript-inner .markdown a', { hasText: '外链' }).click();
  await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'open_external').length > before, beforeLinks);
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'open_external').at(-1).args.url) === 'https://example.invalid/code-copy',
    'The HTTP link left with another URL');
  check(page.url() === pageUrl, 'A link navigated the app WebView');
  await page.locator('.transcript-inner .markdown a', { hasText: '危险' }).click();
  await settle();
  check(await externalCalls() === beforeLinks + 1, 'The unsafe link was handed to the system');
  check(await page.evaluate(() => !window.__UI_TEST_ATTACKED__), 'The unsafe link ran');
  passed.push('sanitizing and HTTP(S)-only link interception still work around the new control');

  // 4. Long code keeps scrolling inside its block, and the control stays
  //    visible without widening a narrow window.
  await page.setViewportSize({ width: 380, height: 780 });
  await settle();
  const narrow = await page.evaluate(() => {
    const blocks = [...document.querySelectorAll('.transcript-inner .code-block')];
    return {
      overflow: document.documentElement.scrollWidth <= window.innerWidth,
      blocks: blocks.map(block => {
        const button = block.querySelector('.code-copy');
        const pre = block.querySelector('pre');
        const style = getComputedStyle(button);
        return {
          inside: button.getBoundingClientRect().left >= 0
            && button.getBoundingClientRect().right <= window.innerWidth,
          aboveCode: button.getBoundingClientRect().bottom <= pre.getBoundingClientRect().top + 1,
          visible: style.display !== 'none' && style.visibility === 'visible' && Number(style.opacity) === 1,
          scrolls: pre.scrollWidth > pre.clientWidth,
        };
      }),
    };
  });
  check(narrow.overflow, 'The copy control widened the app on a narrow window');
  check(narrow.blocks.length === 2 && narrow.blocks.every(block => block.inside),
    'A copy control left the narrow window');
  check(narrow.blocks.every(block => block.aboveCode), 'A copy control covered the code on a narrow window');
  check(narrow.blocks.every(block => block.visible), 'A copy control was hidden until hover');
  check(narrow.blocks[0].scrolls, 'Long code stopped scrolling inside its block');
  passed.push('the copy control stays visible and keeps long code inside the narrow window');
  await page.setViewportSize({ width: 1280, height: 850 });
  await settle();

  // 5. A receipt held while the answer is replaced never speaks for the new
  //    block, on either the success or the failure path.
  await setAnswer(answer('const held = true;'));
  await refresh(2);
  await waitAnswerText('const held = true;\n');
  check(same(await labels('.transcript-inner .code-copy'), ['复制代码', '复制代码']),
    'Re-rendering kept the copy feedback of the replaced content');
  await resetClipboard();
  await holdClipboard(true);
  await buttons.nth(0).click();
  check(same(await labels('.transcript-inner .code-copy'), ['复制代码', '复制代码']),
    'A copy reported success before the clipboard answered');
  await setAnswer(answer('const replaced = true;'));
  await refresh(3);
  await waitAnswerText('const replaced = true;\n');
  transcript = await structure('.transcript-inner');
  check(transcript && transcript.blocks === 2 && transcript.buttons === 2 && transcript.nested === 0,
    'Updating the answer duplicated a copy control');
  check(same(await labels('.transcript-inner .code-copy'), ['复制代码', '复制代码']),
    'The updated answer did not start with untouched controls');
  await holdClipboard(false);
  await answerClipboard('');
  await settle();
  check(same(await labels('.transcript-inner .code-copy'), ['复制代码', '复制代码']),
    'A late clipboard success changed the feedback of the replaced block');

  await holdClipboard(true);
  await buttons.nth(1).click();
  await setAnswer(answer('const lateFailure = true;'));
  await refresh(4);
  await waitAnswerText('const lateFailure = true;\n');
  await holdClipboard(false);
  await answerClipboard('合成剪贴板失败');
  await settle();
  check(same(await labels('.transcript-inner .code-copy'), ['复制代码', '复制代码']),
    'A late clipboard failure changed the feedback of the replaced block');
  check((await structure('.transcript-inner')).buttons === 2, 'A late receipt added a control');
  passed.push('a receipt arriving after the answer was replaced leaves the new blocks alone');

  // 6. A streaming re-render keeps one control per block and drops receipts for
  //    the block it replaced.
  await editor.fill('看一下实时输出');
  await editor.press('Enter');
  await page.getByRole('button', { name: '停止', exact: true }).waitFor();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .some(call => call.command === 'send_message'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.stream({ kind: 'batch', events: [
    { kind: 'text_delta', text: '第一版：\n\n```ts\nconst first = 1;\n```\n' },
  ] }));
  await page.waitForFunction(() => document.querySelector('.stream-markdown .code-block pre > code')
    ?.textContent.includes('const first = 1;'));
  await resetClipboard();
  await holdClipboard(true);
  await page.locator('.stream-markdown .code-copy').click();
  check(same(await labels('.stream-markdown .code-copy'), ['复制代码']),
    'The streaming copy reported success before the clipboard answered');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.stream({ kind: 'batch', events: [
    { kind: 'text_delta', text: '\n第二版：\n\n```ts\nconst second = 2;\n```\n' },
  ] }));
  await page.waitForFunction(() => document.querySelectorAll('.stream-markdown .code-block').length === 2);
  const streamed = await structure('.stream-markdown');
  check(streamed && streamed.blocks === 2 && streamed.bars === 2 && streamed.buttons === 2 && streamed.nested === 0,
    'A streaming re-render duplicated a copy control');
  check(same(await labels('.stream-markdown .code-copy'), ['复制代码', '复制代码']),
    'A streaming re-render kept the feedback of the replaced block');
  await holdClipboard(false);
  await answerClipboard('');
  await settle();
  check(same(await labels('.stream-markdown .code-copy'), ['复制代码', '复制代码']),
    'A late clipboard receipt changed the feedback of the streaming block');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.waitForFunction(() => !document.querySelector('.live-timeline'));
  passed.push('a streaming re-render keeps one control per block and ignores a late receipt');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

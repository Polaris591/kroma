import { installDesktopFixture } from './fixtures/desktop.mjs';

// A real 1x1 PNG, so a preview decodes instead of failing the magic check.
const PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';
const PNG_BYTES = [...Buffer.from(PNG_BASE64, 'base64')];
const HISTORY_IMAGE = 'C:/Users/Example/.kroma/attachments/history-1.png';
const HISTORY_FILE = 'C:/Users/Example/.kroma/attachments/history-1.png.pdf';

/** Accepted input restore, read-only image preview and the close confirmation.
 * Every command is answered by the synthetic host; no window is closed and no
 * personal file is read. */
export async function verifyDesktopBasics(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, { historyImage: 'history-1.png' });
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const editorValue = () => editor.inputValue();
  const chips = page.locator('.file-chip');
  const chipNames = () => page.locator('.file-chip-name').allTextContents();
  const clip = page.getByRole('button', { name: '添加图片或文件', exact: true });
  const send = page.getByRole('button', { name: '发送任务', exact: true });
  const errorBanner = page.locator('.error-banner');
  const count = command => page.evaluate(command => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === command).length, command);
  const messages = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.snapshot.messages.length);
  const previews = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'preview_image').map(call => call.args));
  const choose = paths => page.evaluate(paths => {
    window.__KROMA_UI_FIXTURE__.dialog = paths;
  }, paths);
  const serveImage = (path, bytes = PNG_BYTES) => page.evaluate(({ path, bytes }) => {
    window.__KROMA_UI_FIXTURE__.previewImage(path, bytes);
  }, { path, bytes });
  const idle = () => page.waitForFunction(() => !document.querySelector('.stop-button'));
  const attach = async path => {
    await choose([path]);
    await clip.click();
    await page.waitForFunction(() => document.querySelectorAll('.file-chip').length > 0);
  };
  const detachAll = async () => {
    while (await chips.count()) await page.getByRole('button', { name: '移除附件', exact: true }).first().click();
  };

  // --- an input the backend refused comes back; a submitted failure does not.
  await serveImage('C:/影棚/首屏.png');
  await attach('C:/影棚/首屏.png');
  await editor.fill('看看这张图');
  const beforeReject = await messages();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.rejectInput('附件不存在：C:/影棚/首屏.png'));
  await send.click();
  await idle();
  await page.waitForFunction(() => document.querySelector('.composer textarea').value === '看看这张图');
  check(await editorValue() === '看看这张图', 'A refused input did not come back into the composer');
  check(await chips.count() === 1, 'A refused input lost its attachment');
  check(JSON.stringify(await chipNames()) === JSON.stringify(['首屏.png']),
    `A refused input lost its attachment name: ${JSON.stringify(await chipNames())}`);
  check(await messages() === beforeReject, 'A refused input wrote a user turn anyway');
  check((await errorBanner.innerText()).includes('附件不存在'),
    'A refused input did not explain why nothing was sent');
  passed.push('a refused input returns its text and attachment to the draft');
  await detachAll();
  await editor.fill('');

  // A newer draft typed while the submission is in flight owns the composer.
  await attach('C:/影棚/首屏.png');
  await editor.fill('会被拒绝的任务');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('send_message'));
  const beforeHeld = await count('send_message');
  await send.click();
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'send_message').length > count, beforeHeld);
  await editor.fill('我后来写的草稿');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.rejectInput('附件无法读取'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('send_message'));
  await idle();
  await page.waitForFunction(() => (document.querySelector('.error-banner')?.textContent ?? '')
    .includes('附件无法读取'));
  check(await editorValue() === '我后来写的草稿', 'A refused input overwrote the newer draft');
  check(await chips.count() === 0, 'A refused input re-attached a file next to a newer draft');
  passed.push('a refused input never overwrites a draft typed meanwhile');
  await editor.fill('');

  // The same status without `input_rejected` may already be persisted: keep it gone.
  await editor.fill('这次会真的提交');
  await send.click();
  await page.waitForFunction(() => document.querySelector('.stop-button') !== null);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('failed', '模型连接失败'));
  await idle();
  check(await editorValue() === '', 'A submitted failure was put back into the composer');
  check((await errorBanner.innerText()).includes('模型连接失败'), 'A submitted failure was not reported');
  passed.push('a submitted model failure is never restored as an unsubmitted draft');

  // --- read-only image previews: draft chip, retry, history and the old host.
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.failures = {});
  await choose(['C:/影棚/首屏.png', 'C:/影棚/说明.md']);
  await clip.click();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 2);
  const imageChip = page.locator('.file-chip').filter({ hasText: '首屏.png' });
  const fileChip = page.locator('.file-chip').filter({ hasText: '说明.md' });
  await page.waitForFunction(() => document.querySelectorAll('.file-chip .file-thumb').length === 1);
  check(await imageChip.locator('.file-thumb-button').count() === 1, 'A picked image has no preview entry');
  check(await fileChip.locator('.file-thumb-button').count() === 0, 'A plain file got an image preview entry');
  const thumbnail = await imageChip.locator('img.file-thumb').evaluate(img => img.naturalWidth);
  check(thumbnail === 1, `The picked image thumbnail did not decode (${thumbnail})`);
  check((await previews()).some(call => call.origin === 'draft' && call.path === 'C:/影棚/首屏.png'),
    'The draft thumbnail was not read through the bounded preview command');
  await imageChip.locator('.file-thumb-button').click();
  const previewDialog = page.locator('dialog[aria-label^="图片预览"]');
  await previewDialog.waitFor();
  const full = previewDialog.locator('img.attachment-preview-image');
  await full.waitFor();
  check(await full.evaluate(img => img.naturalWidth) === 1, 'The full image did not decode');
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-label^="图片预览"]') === null);
  passed.push('a picked image shows a thumbnail and opens a read-only preview');
  await detachAll();

  // A missing file explains itself and can be retried.
  await attach('C:/影棚/缺失.png');
  await page.locator('.file-chip .file-thumb-button').click();
  await previewDialog.waitFor();
  const previewError = previewDialog.locator('.attachment-preview-error');
  await previewError.waitFor();
  check((await previewError.innerText()).includes('附件不存在'), 'A missing image did not explain the failure');
  check(await previewDialog.locator('button', { hasText: '重试' }).count() === 1, 'A failed preview has no retry');
  await serveImage('C:/影棚/缺失.png');
  await previewDialog.locator('button', { hasText: '重试' }).click();
  await full.waitFor();
  check(await full.evaluate(img => img.naturalWidth) === 1, 'Retrying a missing image did not show it');
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-label^="图片预览"]') === null);
  passed.push('a missing image reports, stays closed to crashes and can be retried');
  await detachAll();

  // A file can pass the magic check and still fail to decode: it must say so,
  // keep the retry, and never start another read on its own.
  const CORRUPT = 'C:/影棚/损坏.png';
  await attach(CORRUPT);
  await page.evaluate(path => window.__KROMA_UI_FIXTURE__.previewImage(path,
    [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4, 5, 6, 7, 8]), CORRUPT);
  const corruptReads = (await previews()).filter(call => call.path === CORRUPT).length;
  await page.locator('.file-chip .file-thumb-button').click();
  await previewDialog.waitFor();
  await previewError.waitFor();
  check((await previewError.innerText()).includes('损坏'),
    `A corrupt image did not explain the decode failure: ${await previewError.innerText()}`);
  await page.waitForTimeout(200);
  check((await previews()).filter(call => call.path === CORRUPT).length === corruptReads + 1,
    'A corrupt image kept reading itself instead of waiting for the person');
  check(await previewDialog.locator('button', { hasText: '重试' }).count() === 1,
    'A corrupt image lost its explicit retry');
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-label^="图片预览"]') === null);
  passed.push('a corrupt image reports a decode failure and never retries on its own');
  await detachAll();

  // History images load only when opened, and history paths are read as history.
  const historyChip = page.getByRole('button', { name: '查看图片 历史的截图.png', exact: true });
  await historyChip.waitFor();
  const beforeHistory = (await previews()).length;
  const historyFile = page.locator('.attachment-chip').filter({ hasText: '报告.pdf' });
  check(await historyFile.evaluate(node => node.tagName) === 'SPAN', 'A plain history file became a preview button');
  check((await previews()).length === beforeHistory, 'A history image was decoded without being opened');
  await serveImage(HISTORY_IMAGE);
  await historyChip.click();
  await previewDialog.waitFor();
  await full.waitFor();
  check((await previews()).at(-1).origin === 'history',
    `A history image was not read as history: ${JSON.stringify((await previews()).at(-1))}`);
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-label^="图片预览"]') === null);

  // An older Desktop without the command says so instead of showing a broken image.
  await page.evaluate(() => {
    window.__KROMA_UI_FIXTURE__.failures.preview_image = 'Command preview_image not found';
  });
  await historyChip.click();
  await previewError.waitFor();
  check((await previewError.innerText()).includes('更新并重启 Desktop'),
    'A missing preview command did not explain the update: ' + await previewError.innerText());
  await previewDialog.locator('button', { hasText: '重试' }).click();
  await full.waitFor();
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-label^="图片预览"]') === null);
  passed.push('history images open on demand and an older host explains the missing command');

  // --- closing the window while a task runs.
  const confirm = page.locator('dialog[aria-labelledby="close-confirm-heading"]');
  const destroyCalls = () => count('plugin:window|destroy');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.closeRequested());
  check(await destroyCalls() === 1, 'An idle window did not close directly');
  check(await confirm.count() === 0, 'An idle window asked a needless question');
  passed.push('an idle window closes without a confirmation');

  await editor.fill('关窗前正在跑的会话');
  await send.click();
  await page.waitForFunction(() => document.querySelector('.stop-button') !== null);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.closeRequested());
  await confirm.waitFor();
  check(await destroyCalls() === 1, 'A running task closed the window without asking');
  const focused = await page.evaluate(() => document.activeElement?.textContent?.trim() ?? '');
  check(focused === '继续执行', `The safe choice does not own the initial focus: ${focused}`);
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('dialog[aria-labelledby="close-confirm-heading"]') === null);
  check(await destroyCalls() === 1, 'Cancelling the confirmation closed the window');
  passed.push('a running window asks, and cancelling keeps it open');

  // Another dialog is no reason to skip the question.
  await page.keyboard.press('F1');
  const shortcuts = page.locator('dialog[aria-labelledby="shortcuts-heading"]');
  await shortcuts.waitFor();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.closeRequested());
  await confirm.waitFor();
  await page.getByRole('button', { name: '继续执行', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('dialog[aria-labelledby="close-confirm-heading"]') === null);
  check(await shortcuts.isVisible(), 'Continuing execution dropped the open dialog');
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('dialog[open]'));
  check(await destroyCalls() === 1, 'Continuing execution closed the window');
  passed.push('the confirmation still appears above another open dialog');

  // A confirmed close stops only the running task and keeps the draft; a queued
  // task must not start in the window between confirming and closing.
  await editor.fill('关窗后不应启动的队列任务');
  await page.locator('.followup-add-button').click();
  await page.waitForFunction(() => document.querySelectorAll('.followup-item').length === 1);
  const queuedSends = await count('send_message');
  await editor.fill('关窗前的草稿');
  // Let the typing debounce settle, then prove the close itself writes it back.
  await page.waitForTimeout(450);
  await page.evaluate(() => {
    for (const key of Object.keys(localStorage)) {
      if (key.startsWith('kroma.desktop.draft.v2:')) localStorage.removeItem(key);
    }
  });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.closeRequested());
  await confirm.waitFor();
  // Hold the close in flight, then let the running task complete inside the gap.
  await page.evaluate(() => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    fixture.hold('cancel_task');
    fixture.hold('plugin:window|close');
  });
  await page.getByRole('button', { name: '关闭并中断', exact: true }).click();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await idle();
  await page.evaluate(() => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    fixture.release('cancel_task');
    fixture.release('plugin:window|close');
  });
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'plugin:window|destroy').length === 2);
  check(await count('send_message') === queuedSends,
    'Closing started the next queued task inside the close gap');
  check(await page.locator('.followup-item').count() === 1,
    'Closing dropped the queue that was never executed');
  check(await count('cancel_task') >= 1, 'Closing during a task did not ask the task to stop');
  const saved = await page.evaluate(() => Object.keys(localStorage)
    .filter(key => key.startsWith('kroma.desktop.draft.v2:'))
    .map(key => JSON.parse(localStorage.getItem(key)).text));
  check(saved.includes('关窗前的草稿'), `Closing did not keep the draft: ${JSON.stringify(saved)}`);
  passed.push('a confirmed close stops the task, keeps the draft and never starts the queue');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

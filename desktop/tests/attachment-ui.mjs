import { installDesktopFixture } from './fixtures/desktop.mjs';

// A real 1x1 PNG, so a pasted preview decodes instead of degrading to the icon.
const PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyAttachments(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page);
  // Preview URLs are counted in the page, so a rejected attachment can be
  // proven to create nothing.
  await page.addInitScript(() => {
    window.__KROMA_URLS__ = { created: 0, revoked: 0 };
    const create = URL.createObjectURL.bind(URL);
    const revoke = URL.revokeObjectURL.bind(URL);
    URL.createObjectURL = blob => { window.__KROMA_URLS__.created += 1; return create(blob); };
    URL.revokeObjectURL = url => { window.__KROMA_URLS__.revoked += 1; return revoke(url); };
  });
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const chips = page.locator('.file-chip');
  const thumbs = page.locator('.file-chip .file-thumb');
  const clip = page.getByRole('button', { name: '添加图片或文件', exact: true });
  const names = () => page.locator('.file-chip-name').allTextContents();
  const urls = () => page.evaluate(() => ({ ...window.__KROMA_URLS__ }));
  const pasted = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'save_pasted_image'));
  const count = (command) => page.evaluate(command => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === command).length, command);
  // Each dispatch is its own paste event; dispatching several in one evaluate
  // is how an overlapping burst reaches the panel.
  const paste = (events = 1) => page.evaluate(({ events, png }) => {
    const bytes = Uint8Array.from(atob(png), character => character.charCodeAt(0));
    for (let index = 0; index < events; index += 1) {
      const data = new DataTransfer();
      data.items.add(new File([bytes], `粘贴-${index + 1}.png`, { type: 'image/png' }));
      document.querySelector('.composer textarea').dispatchEvent(
        new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
    }
  }, { events, png: PNG_BASE64 });
  const choose = paths => page.evaluate(paths => { window.__KROMA_UI_FIXTURE__.dialog = paths; }, paths);
  const settled = () => page.waitForTimeout(250);
  const switchSession = async (name) => {
    const before = await count('switch_session');
    await page.locator('.session-row').filter({ hasText: name }).locator('.session-main').click();
    await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
      .filter(call => call.command === 'switch_session').length > before, before);
    await page.waitForFunction(() => {
      const input = document.querySelector('.composer textarea');
      return input !== null && !input.disabled;
    });
  };
  const detachAll = async () => {
    while (await chips.count()) await page.getByRole('button', { name: '移除附件', exact: true }).first().click();
  };

  // A pasted image is written once, previewed, and sent with the task.
  await paste();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 1);
  const firstPaste = await pasted();
  check(firstPaste.length === 1, `Pasting saved ${firstPaste.length} images`);
  check(firstPaste[0].args.extension === 'png', 'Pasting used a wrong extension');
  check(firstPaste[0].args.bytes.length > 8, 'Pasting sent no image bytes');
  check(await thumbs.count() === 1, 'The accepted image has no preview');
  check((await urls()).created === 1, 'Pasting did not create exactly one preview URL');
  check(await editor.inputValue() === '', 'Pasting inserted text into the prompt');
  await editor.fill('看看这张截图');
  await page.getByRole('button', { name: '发送任务', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .some(call => call.command === 'send_message'));
  const sent = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'send_message').pop().args);
  check(sent.text === '看看这张截图', 'The typed task text was not sent');
  check(sent.paths.length === 1 && sent.paths[0].endsWith('paste-1.png'),
    `The accepted image was not sent with the task: ${JSON.stringify(sent.paths)}`);
  passed.push('a pasted image is written once, previewed and sent');

  // Attachments chosen during a running task belong to the NEXT draft. They
  // must neither steer files into the active task nor disappear when it ends.
  check(await clip.isEnabled(), 'The next draft cannot take attachments during a task');
  const sendsDuringTask = await count('send_message');
  await paste();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 1);
  await choose(['C:/影棚/后续说明.md']);
  await clip.click();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 2);
  const savedDuringTask = (await pasted()).length;
  check(savedDuringTask === 2, 'Pasting into the next draft did not write exactly one new image');
  check((await urls()).created === 2, 'The accepted next-draft image has no preview URL');
  check(await count('send_message') === sendsDuringTask, 'Next-draft attachments were sent into the active task');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish());
  await page.waitForFunction(() => !document.querySelector('.stop-button'));
  check(await chips.count() === 2, 'Finishing the task cleared the next draft attachments');
  await detachAll();
  passed.push('running tasks accept pasted and picked files into the next draft only');

  // A full draft neither writes the image nor creates a preview URL.
  await choose(Array.from({ length: 10 }, (_, index) => `C:/影棚/资料-${index + 1}.png`));
  await clip.click();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 8);
  check(await thumbs.count() === 0, 'Picked files created preview URLs');
  const fullDraftUrls = (await urls()).created;
  await paste();
  await settled();
  check((await pasted()).length === savedDuringTask, 'A full draft still wrote the pasted image');
  check((await urls()).created === fullDraftUrls, 'A full draft still created a preview URL');
  check(await chips.count() === 8, 'The draft exceeded eight attachments');
  passed.push('a full draft keeps eight files and writes no ninth image');
  await detachAll();

  // Overlapping pastes are all in flight at once; the draft still stops at eight
  // and only the images it actually kept own a preview.
  await choose(Array.from({ length: 6 }, (_, index) => `C:/影棚/底图-${index + 1}.png`));
  await clip.click();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 6);
  const beforeBurst = (await pasted()).length;
  const beforeUrls = (await urls()).created;
  await paste(3);
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 8);
  await settled();
  check(await chips.count() === 8, 'Overlapping pastes exceeded eight attachments');
  check(await thumbs.count() === 2, 'Only the two accepted images may get a preview');
  check((await urls()).created === beforeUrls + 2, 'A rejected overlapping paste created a preview URL');
  check((await pasted()).length - beforeBurst <= 3, 'Overlapping pastes wrote more than one image each');
  check(await page.locator('.error-banner').count() === 0, 'Overlapping pastes reported an error');
  passed.push('overlapping pastes end at eight attachments with two previews');
  await detachAll();

  // A -> B -> A returns to a different draft than the one the paste started in.
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('save_pasted_image'));
  const pendingPaste = (await pasted()).length;
  await paste();
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'save_pasted_image').length > count, pendingPaste);
  const beforeReturn = (await urls()).created;
  await switchSession('整理项目的下一步');
  await switchSession('让想法自然发生');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('save_pasted_image'));
  await settled();
  check(await chips.count() === 0, 'A late paste answer came back after A -> B -> A');
  check((await urls()).created === beforeReturn, 'A late paste answer created a preview URL');
  check(await page.locator('.error-banner').count() === 0, 'A late paste reported an error');
  passed.push('a late paste answer does not come back after A -> B -> A');

  // Starting a task invalidates the paste still in flight; its failure is silent.
  const beforeTaskPaste = (await pasted()).length;
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('save_pasted_image'));
  await paste();
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'save_pasted_image').length > count, beforeTaskPaste);
  const beforeTask = (await urls()).created;
  await editor.fill('这次的任务');
  await page.getByRole('button', { name: '发送任务', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .some(call => call.command === 'send_message'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('save_pasted_image', '粘贴图片保存失败'));
  await settled();
  check(await chips.count() === 0, 'A late paste failure attached an image after the task started');
  check(await page.locator('.error-banner').count() === 0, 'A late paste failure reached the new draft');
  check((await urls()).created === beforeTask, 'A late paste failure created a preview URL');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish());
  await page.waitForFunction(() => !document.querySelector('.stop-button'));
  passed.push('a paste still in flight when a task starts neither attaches nor reports');

  // Picked attachments belong to their own draft and come back with it.
  await choose(['C:/影棚/封面.png', 'C:/影棚/说明.md']);
  await clip.click();
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 2);
  await switchSession('整理项目的下一步');
  check(await chips.count() === 0, 'Another session showed the previous attachments');
  await switchSession('让想法自然发生');
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 2);
  check(JSON.stringify(await names()) === JSON.stringify(['封面.png', '说明.md']),
    `Returning to the draft lost its attachments: ${JSON.stringify(await names())}`);
  passed.push('picked attachments follow their draft across a session switch');

  // A plugin rewriting the text keeps the attachments and the paste in flight.
  const beforePlugin = (await urls()).created;
  const beforePluginSaved = (await pasted()).length;
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('save_pasted_image'));
  await paste();
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'save_pasted_image').length > count, beforePluginSaved);
  await page.evaluate(() => {
    const f = window.__KROMA_UI_FIXTURE__;
    f.emit({project: f.state.project, session: f.state.snapshot.session_id,
      event: {kind: 'composer_set', text: '插件改写', mode: 'replace'}});
  });
  await page.waitForFunction(() => document.querySelector('.composer textarea').value === '插件改写');
  check(await chips.count() === 2, 'A plugin text replacement dropped the attachments');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('save_pasted_image'));
  await page.waitForFunction(() => document.querySelectorAll('.file-chip').length === 3);
  check((await urls()).created === beforePlugin + 1, 'A plugin text replacement cancelled the paste');
  check(await editor.inputValue() === '插件改写', 'The plugin text was lost');
  passed.push('plugin text replacement keeps attachments and in-flight pastes');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

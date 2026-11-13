import { installDesktopFixture } from './fixtures/desktop.mjs';

// Synthetic end-to-end checks for this batch: complete drafts, queue
// attachment snapshots, in-conversation/cross-session search, local session
// backup and attachment save-as. Every action goes through the real App
// handlers; the fixture only answers the IPC calls a native host would.

const CODE = 'const pad = (n) =>\n  n < 10 ? "0" + String(n) : String(n);';

function designHistory() {
  return [
    { role: 'system', content: 'synthetic system' },
    { role: 'user', content: '请看看这个功能' },
    {
      role: 'assistant',
      content: `可以，最终实现如下：\n\n\`\`\`ts\n${CODE}\n\`\`\`\n\n复制按钮应保持原文。`,
    },
    {
      role: 'user',
      content: '再看这个文件',
      provider_items: [
        { type: 'kroma_attachment', kind: 'image', name: '历史的截图.png', mime: 'image/png',
          path: 'C:/Users/Example/.kroma/attachments/shot-1.png', bytes: 68 },
        { type: 'kroma_attachment', kind: 'file', name: '报告.pdf',
          path: 'C:/Users/Example/.kroma/attachments/shot-1.pdf', bytes: 2048 },
      ],
    },
    { role: 'assistant', content: '已经看过。' },
  ];
}

function notesHistory() {
  const full = [{ role: 'system', content: 'synthetic system' }];
  for (let index = 1; index < 300; index++) {
    full.push(index % 2
      ? { role: 'user', content: `旧问题 ${index}` }
      : { role: 'assistant', content: `旧回答 ${index}` });
  }
  full[20] = { role: 'assistant', content: '深藏的答案 alpha，旧历史定位。' };
  return full;
}

/** A conversation long enough that locating an older hit must load a few pages
 * without ever loading the whole history. */
function reviewHistory() {
  const full = [{ role: 'system', content: 'synthetic system' }];
  for (let index = 1; index < 3000; index++) {
    full.push(index % 2
      ? { role: 'user', content: `复核问题 ${index}` }
      : { role: 'assistant', content: `复核回答 ${index}` });
  }
  full[2000] = { role: 'assistant', content: '中段的旧回答 review-target' };
  return full;
}

export async function verifyWorkflowCompletion(page, baseURL) {
  const errors = [];
  const passed = [];
  const sessionAction = async name => {
    await page.getByRole('button', {name: '会话操作', exact: true}).click();
    await page.getByRole('button', {name, exact: true}).click();
  };
  const check = (value, message) => { if (!value) throw new Error(message); passed.push(message); };
  page.on('pageerror', error => errors.push(error.message));

  const fixtureOptions = {
    histories: {
      design: { full: designHistory(), windowSize: 120 },
      notes: { full: notesHistory(), windowSize: 120 },
      review: { full: reviewHistory(), windowSize: 120 },
    },
    attachmentStates: { 'C:/tmp/missing.pdf': 'missing' },
    searchHits: [
      {
        project: 'D:/Kroma/Design-Sandbox', id: 'notes', role: 'assistant',
        message_index: 20, snippet: '旧历史里的深藏答案 alpha，旧历史定位。',
      },
      {
        project: 'D:/Kroma/Design-Sandbox', id: 'review', role: 'assistant',
        message_index: 2000, snippet: '中段的旧历史回答 review-target',
      },
      {
        project: 'D:/Kroma/Design-Sandbox', id: 'design', role: 'assistant',
        message_index: 4, snippet: '旧历史里已经看过。',
      },
    ],
    dialog: ['C:/tmp/report.pdf'],
    dialogSave: null,
  };
  await installDesktopFixture(page, fixtureOptions);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const call = (command) => page.evaluate(name => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === name).map(entry => entry.args), command);
  const settle = (ms = 150) => page.waitForTimeout(ms);
  const project = 'D:/Kroma/Design-Sandbox';
  const draftKey = (session) => `kroma.desktop.draft.v2:${encodeURIComponent(project)}:${encodeURIComponent(session)}`;
  const storedQueue = () => page.evaluate(() => {
    const key = Object.keys(localStorage).find(entry => entry.startsWith('kroma.desktop.followups.v1:'));
    return key ? JSON.parse(localStorage.getItem(key) ?? '[]') : [];
  });

  // 1. Draft restores text and attachments after a reload; a missing one stays
  //    labelled and can be removed without blocking editing.
  await editor.fill('还没发出的草稿');
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  await page.waitForFunction(() => document.querySelector('textarea[aria-label="输入任务"]')?.value === '还没发出的草稿');
  check(true, 'draft text survives a reload');

  // Seed the versioned draft once, before the app starts: writing it on the
  // live page would be overwritten by the leaving page's own sync save.
  await page.addInitScript(({ key }) => {
    if (sessionStorage.getItem('draft-seeded')) return;
    sessionStorage.setItem('draft-seeded', '1');
    localStorage.setItem(key, JSON.stringify({ text: '带附件的草稿', paths: ['C:/tmp/missing.pdf'] }));
  }, { key: draftKey('design') });
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => document.querySelector('textarea[aria-label="输入任务"]')?.value === '带附件的草稿');
  await page.waitForSelector('.file-chip-invalid');
  check((await page.locator('.file-chip-invalid').innerText()).includes('已失效'),
    'a restored attachment that no longer resolves is visibly marked');
  await editor.fill('继续编辑不受影响');
  await page.waitForFunction(() => document.querySelector('textarea[aria-label="输入任务"]')?.value === '继续编辑不受影响');
  await page.getByRole('button', { name: '移除附件' }).click();
  await page.waitForFunction(() => !document.querySelector('.file-chip'));
  check(true, 'a stale draft attachment can be removed while editing continues');

  // 2. Queueing copies the files before the task finishes. A draft typed while
  //    the copy is in flight belongs to the person, not to the queued task.
  await editor.fill('第一项');
  await page.getByRole('button', { name: '发送任务' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls.some(entry => entry.command === 'send_message'));

  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('prepare_followup_attachments'));
  await editor.fill('排队项 A');
  await page.getByRole('button', { name: '添加图片或文件' }).click();
  await page.waitForSelector('.file-chip');
  await editor.press('Control+Enter');
  // The copy is still pending; keep typing, then let it finish.
  await editor.fill('任务中继续写的新草稿');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('prepare_followup_attachments'));
  await page.waitForSelector('.followup-item');
  const queueState = await storedQueue();
  check(queueState.length === 1 && queueState[0].paths.length === 1,
    'the queued task stores a private snapshot path, not the source path');
  check(await editor.inputValue() === '任务中继续写的新草稿',
    'a draft typed during the queue copy is preserved');

  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 2);
  const sends = await call('send_message');
  check(sends[1].text === '排队项 A', 'the queued text is executed');
  check(Array.isArray(sends[1].paths) && sends[1].paths.length === 1
    && sends[1].paths[0].includes('.kroma/attachments/snap-1'),
    'the queued task executes with its copied attachment path');
  check(await editor.inputValue() === '任务中继续写的新草稿',
    'finishing a queued task does not clear the new draft');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));

  // Queue another task, then restart: the queue must come back paused and never
  // send an incomplete task when its attachment snapshot is gone.
  await editor.fill('第二项');
  await page.getByRole('button', { name: '发送任务' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 3);
  await editor.fill('重启后暂停项');
  await page.getByRole('button', { name: '添加图片或文件' }).click();
  await editor.press('Control+Enter');
  await page.waitForSelector('.followup-item');
  const beforeReload = (await call('send_message')).length;
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  await page.waitForSelector('.followup-item');
  check((await page.locator('.followup-heading').innerText()).includes('已暂停'),
    'a restored queue is paused instead of replaying tasks');
  await settle(400);
  check((await call('send_message')).length === 0,
    `no task is sent after a restart (before reload: ${beforeReload})`);

  const restored = await storedQueue();
  check(restored.length === 1 && restored[0].paths.length === 1, 'restored queue keeps its snapshot path');
  await page.evaluate(path => { window.__KROMA_UI_FIXTURE__.attachmentStates[path] = 'missing'; }, restored[0].paths[0]);
  await page.locator('.followup-control', { hasText: '继续' }).click();
  await page.waitForFunction(() => document.querySelector('.notice-banner')?.textContent?.includes('附件已失效'));
  check((await call('send_message')).length === 0,
    'a queued task with a lost attachment is not sent');
  await page.getByRole('button', { name: '移除任务 1' }).click();
  await page.waitForFunction(() => !document.querySelector('.followup-item'));

  // A pause during the attachment check must leave the queued task untouched:
  // nothing is removed and nothing is sent with a stale head.
  await editor.fill('第三项');
  await page.getByRole('button', { name: '发送任务' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 1);
  await page.getByRole('button', { name: '添加图片或文件' }).click();
  await editor.fill('检查中暂停项');
  await editor.press('Control+Enter');
  await page.waitForSelector('.followup-item');
  const inspectsBefore = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'inspect_attachments').length);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('inspect_attachments'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'inspect_attachments').length > count, inspectsBefore);
  await page.locator('.followup-control', { hasText: '暂停' }).click();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('inspect_attachments'));
  await settle(400);
  check((await call('send_message')).length === 1,
    'a pause during the attachment check sends nothing');
  check(await page.locator('.followup-item').count() === 1,
    'the paused queue keeps the task it was about to check');

  // Removing the head while its check is in flight must not send the old item.
  await page.locator('.followup-control', { hasText: '继续' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 2);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.waitForFunction(() => !document.querySelector('.followup-item'));
  await editor.fill('第四项');
  await page.getByRole('button', { name: '发送任务' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 3);
  await page.getByRole('button', { name: '添加图片或文件' }).click();
  await editor.fill('检查中删除项');
  await editor.press('Control+Enter');
  await page.waitForSelector('.followup-item');
  const inspectsBeforeDelete = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'inspect_attachments').length);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('inspect_attachments'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'inspect_attachments').length > count, inspectsBeforeDelete);
  await page.getByRole('button', { name: '移除任务 1' }).click();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('inspect_attachments'));
  await settle(400);
  check((await call('send_message')).length === 3,
    'removing the head during its check sends nothing');
  check(await page.locator('.followup-item').count() === 0, 'the removed task stays removed');

  // A task that ends while its attachment copy is still running must not
  // auto-unpause the queue or bind the new item to a finished task.
  await editor.fill('第五项');
  await page.getByRole('button', { name: '发送任务' }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'send_message').length === 4);
  const preparesBefore = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === 'prepare_followup_attachments').length);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('prepare_followup_attachments'));
  await page.getByRole('button', { name: '添加图片或文件' }).click();
  await editor.fill('准备中结束项');
  await editor.press('Control+Enter');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.finish('completed'));
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('prepare_followup_attachments'));
  await page.waitForFunction(() => document.querySelector('.followup-item')?.textContent?.includes('准备中结束项'));
  await page.waitForFunction(() => document.querySelector('.followup-heading')?.textContent?.includes('已暂停'));
  await settle(400);
  check((await call('send_message')).length === 4,
    'a queue entry created after its task ended waits for a deliberate resume');
  await page.getByRole('button', { name: '移除任务 1' }).click();
  await page.waitForFunction(() => !document.querySelector('.followup-item'));

  // 3. Ctrl+F searches the open conversation's visible text (prompts and
  //    answers) and highlights the message without rewriting Markdown.
  await page.evaluate(() => {
    const clipboard = { texts: [] };
    clipboard.writeText = text => { clipboard.texts.push(text); return Promise.resolve(); };
    window.__KROMA_CLIPBOARD__ = clipboard;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: clipboard });
  });
  await page.keyboard.press('Control+f');
  const findInput = page.getByRole('textbox', { name: '在当前会话中查找' });
  await findInput.waitFor();
  await findInput.fill('最终实现');
  await page.waitForFunction(() => document.querySelector('.find-count')?.textContent === '1/1');
  await page.waitForSelector('[data-message-index="2"].search-hit');
  check(true, 'Ctrl+F finds an assistant answer and highlights its message');

  const codeBefore = await page.locator('.code-block code').first().textContent();
  check(codeBefore === CODE + '\n', 'the code block keeps its exact text');
  await page.locator('.code-copy').first().click();
  const copied = await page.evaluate(() => window.__KROMA_CLIPBOARD__.texts);
  check(copied.length === 1 && copied[0] === CODE + '\n',
    'search highlighting does not break the code copy control');
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('.find-bar'));
  check(await page.locator('.search-hit').count() === 0, 'Escape closes the find bar and clears the highlight');

  // A cross-session hit opens the conversation and pages back to the exact
  // absolute message index, not just to the last loaded window. One query
  // matches every seeded answer, so sidebar results are not refetched between
  // clicks and no click can race the previous result set.
  await page.keyboard.press('Control+k');
  const sessionSearch = page.locator('.session-search');
  await sessionSearch.waitFor();
  await sessionSearch.fill('旧历史');
  await page.waitForFunction(() => document.querySelectorAll('.session-search-snippet').length === 3);
  check((await page.locator('.session-search-role').first().innerText()).includes('回答'),
    'a cross-session answer hit shows its role');
  await page.locator('.session-row', { hasText: '一些还没完成的想法' }).locator('.session-main').click();
  await page.waitForFunction(() => document.querySelector('.topbar .title-button')?.textContent?.includes('一些还没完成的想法'));
  await page.waitForSelector('[data-message-index="20"].search-hit', { timeout: 15000 });
  check((await page.locator('[data-message-index="20"].search-hit').innerText()).includes('深藏的答案'),
    'an older answer is located by loading bounded history pages');

  // A hit in the middle of a 3000-message conversation loads only the pages
  // needed to cover it; the oldest history stays unloaded (base still > 0).
  await page.locator('.session-row', { hasText: '整理项目的下一步' }).locator('.session-main').click();
  await page.waitForFunction(() => document.querySelector('.topbar .title-button')?.textContent?.includes('整理项目的下一步'));
  await page.waitForSelector('[data-message-index="120"].search-hit', { timeout: 20000 });
  const pagesAfterReview = await call('history_page');
  check(pagesAfterReview.length === 3,
    `only the needed pages are loaded for a mid-history target (${pagesAfterReview.length})`);
  check(pagesAfterReview[1]?.before === 2880 && pagesAfterReview[2]?.before === 2380,
    'the reveal pages back from the loaded base without jumping to the first page');
  check(await page.locator('.load-earlier').count() > 0,
    'a mid-history hit does not force the whole conversation to load');
  check((await page.locator('[data-message-index="120"].search-hit').innerText()).includes('review-target'),
    'the local DOM index matches the absolute target after a partial load');

  // A hit already in the tail window must not page at all.
  const pagesBeforeTail = (await call('history_page')).length;
  await page.locator('.session-row', { hasText: '让想法自然发生' }).locator('.session-main').click();
  await page.waitForFunction(() => document.querySelector('.topbar .title-button')?.textContent?.includes('让想法自然发生'));
  await page.waitForSelector('[data-message-index="4"].search-hit');
  check((await call('history_page')).length === pagesBeforeTail,
    'a tail-page hit is located without loading older history');

  // 4. Backup is a separate entry from Markdown export, warns before writing,
  //    and import always produces a new conversation.
  await sessionAction('备份会话');
  await page.waitForFunction(() => document.querySelector('dialog[aria-labelledby="backup-export-heading"]'));
  await page.getByRole('button', { name: '取消' }).click();
  check((await call('export_session_backup')).length === 0, 'cancelling the backup writes nothing');

  await sessionAction('备份会话');
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.dialogSave = 'D:/exports/notes.kroma-backup'; });
  await page.getByRole('button', { name: /继续备份/ }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.backups.length === 1);
  const backups = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.backups);
  check(backups[0].id === 'design' && backups[0].path === 'D:/exports/notes.kroma-backup',
    'the backup command receives the picked path');
  await page.waitForFunction(() => document.querySelector('.notice-banner')?.textContent?.includes('已备份'));

  await sessionAction('导入会话备份');
  await page.waitForFunction(() => document.querySelector('dialog[aria-labelledby="backup-import-heading"]')
    ?.textContent?.includes('不可信'));
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.dialog = 'C:/backups/one.kroma-backup'; });
  await page.getByRole('button', { name: /选择备份文件/ }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.imports.length === 1);
  await page.waitForFunction(() => document.querySelector('.topbar .title-button')?.textContent?.includes('导入的会话'));
  await page.waitForFunction(() => document.body.textContent.includes('导入的回答'));
  await page.waitForFunction(() => document.querySelector('.notice-banner')?.textContent?.includes('已导入为新会话'));
  check(true, 'a backup is imported as a new visible conversation');

  await sessionAction('导入会话备份');
  await page.evaluate(() => {
    window.__KROMA_UI_FIXTURE__.dialog = 'C:/backups/broken.kroma-backup';
    window.__KROMA_UI_FIXTURE__.importFailure = '备份中的上下文状态与对话不一致';
  });
  await page.getByRole('button', { name: /选择备份文件/ }).click();
  await page.waitForFunction(() => document.querySelector('.error-banner')?.textContent?.includes('上下文状态与对话不一致'));
  check(true, 'a refused import reports the exact validation failure');
  await page.locator('.error-banner button').click();

  // 5. Every stored history attachment can be saved; cancelling is quiet and a
  //    missing file is a clear, retryable error.
  await page.keyboard.press('Control+k');
  await page.locator('.session-search').fill('让想法');
  await page.locator('.session-row', { hasText: '让想法自然发生' }).locator('.session-main').click();
  await page.waitForFunction(() => document.querySelector('.topbar .title-button')?.textContent?.includes('让想法自然发生'));
  const saveButton = page.getByRole('button', { name: '另存为 报告.pdf' });
  await saveButton.waitFor();
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.dialogSave = null; });
  await saveButton.click();
  await settle(200);
  check((await page.evaluate(() => window.__KROMA_UI_FIXTURE__.savedAs)).length === 0,
    'cancelling the save dialog writes nothing and reports no error');
  check(await page.locator('.error-banner').count() === 0, 'cancelling is not an error');

  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.dialogSave = 'D:/exports/report.pdf'; });
  await saveButton.click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.savedAs.length === 1);
  const saved = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.savedAs[0]);
  check(saved.source.endsWith('shot-1.pdf') && saved.destination === 'D:/exports/report.pdf',
    'save-as copies the stored attachment to the chosen destination');
  await page.waitForFunction(() => document.querySelector('.attachment-save-note')?.textContent?.includes('已保存'));

  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.saveMissing = true; });
  await page.getByRole('button', { name: '另存为 历史的截图.png' }).click();
  await page.waitForFunction(() => document.querySelector('.error-banner')?.textContent?.includes('附件不存在'));
  check(true, 'a missing attachment reports a clear, retryable error');
  await page.locator('.error-banner button').click();

  // 6. Import without a connected runtime still stores the session; it must
  //    say so and never look like a failed import or invite a duplicate.
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.importOffline = true; });
  const importsBefore = (await page.evaluate(() => window.__KROMA_UI_FIXTURE__.imports)).length;
  await sessionAction('导入会话备份');
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.dialog = 'C:/backups/offline.kroma-backup'; });
  await page.getByRole('button', { name: /选择备份文件/ }).click();
  await page.waitForFunction(() => document.querySelector('.notice-banner')?.textContent?.includes('尚未连接模型'));
  const notice = await page.locator('.notice-banner').innerText();
  check(notice.includes('已导入为新会话') && notice.includes('尚未连接模型'),
    'an offline import reports the stored conversation and the unopened reason');
  check((await page.evaluate(() => window.__KROMA_UI_FIXTURE__.imports)).length === importsBefore + 1,
    'an offline import is not retried automatically');
  check(await page.locator('.error-banner').count() === 0,
    'an unopened import is not reported as a storage failure');

  check(errors.length === 0, `no uncaught page errors (${errors.join('; ')})`);
  return passed;
}

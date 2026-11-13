import { installDesktopFixture } from './fixtures/desktop.mjs';

const A = 'src/App.svelte';
const B = 'src/lib/api.ts';
const C = 'docs/notes.md';
const D = 'src/main.ts';

const entry = path => ({ path, status: 'modified', staged: false, untracked: false });
const hunks = (...lines) => `@@ -1 +1 @@\n-old\n${lines.map(line => `+${line}\n`).join('')}`;

export async function verifyWorkspaceReview(page, baseURL = 'http://127.0.0.1:1425/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, {
    changes: true,
    workspace: {
      is_repo: true, message: null,
      entries: [entry(A), entry(B)],
      diffs: { [A]: hunks('first'), [B]: hunks('second-file') },
    },
  });
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  await page.getByRole('textbox', { name: '输入任务', exact: true }).waitFor();

  const dialog = page.getByRole('dialog', { name: '查看工作区变更', exact: true });
  const refresh = dialog.getByRole('button', { name: '刷新工作区变更', exact: true });
  const copy = dialog.locator('.review-diff-title button');
  const activePath = () => page.evaluate(() => document.querySelector('.review-file-row.active')?.title ?? '');
  const diffText = () => page.evaluate(() => document.querySelector('.review-diff-pre')?.textContent ?? '');
  const diffCalls = path => page.evaluate(p => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'workspace_file_diff' && call.args.path === p).length, path);
  const waitDiff = part => page.waitForFunction(
    text => document.querySelector('.review-diff-pre')?.textContent.includes(text), part);
  const waitPath = path => page.waitForFunction(
    p => (document.querySelector('.review-file-row.active')?.title ?? '') === p, path);
  const waitIdle = () => page.waitForFunction(
    () => !document.querySelector('[aria-label="刷新工作区变更"]')?.disabled);
  const waitBusy = () => page.waitForFunction(
    () => !!document.querySelector('[aria-label="刷新工作区变更"]')?.disabled);
  const setWorkspace = changes => page.evaluate(
    patch => Object.assign(window.__KROMA_UI_FIXTURE__.workspace, patch), changes);
  // Nothing observable follows a dropped answer, so wait long enough for a
  // superseded promise to have resolved and then check it changed nothing.
  const settle = () => page.waitForTimeout(150);

  // The clipboard is controllable so a receipt can arrive after its copy is stale.
  await page.evaluate(() => {
    const clipboard = {
      hold: false, text: null, pending: [],
      writeText(text) {
        clipboard.text = text;
        if (!clipboard.hold) return Promise.resolve();
        return new Promise((resolve, reject) => clipboard.pending.push({ resolve, reject }));
      },
    };
    window.__KROMA_CLIPBOARD__ = clipboard;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: clipboard });
  });
  const copiedText = () => page.evaluate(() => window.__KROMA_CLIPBOARD__.text);
  const waitCopied = () => page.waitForFunction(
    () => document.querySelector('.review-diff-title button')?.textContent.includes('已复制'));
  const holdClipboard = value => page.evaluate(hold => { window.__KROMA_CLIPBOARD__.hold = hold; }, value);
  const answerClipboard = error => page.evaluate(message => {
    const pending = window.__KROMA_CLIPBOARD__.pending.shift();
    if (!pending) throw new Error('No pending clipboard write');
    if (message) pending.reject(new Error(message)); else pending.resolve();
  }, error ?? '');

  await page.getByRole('button', { name: /^工作区变更 2$/ }).click();
  await dialog.waitFor();
  await waitPath(A);
  await waitDiff('first');
  passed.push('the workspace view opens on the first reported file');

  // 1. A refresh re-reads the diff of a path that survived it, and the diff
  //    shown before the refresh is neither copied nor kept as the current one.
  await copy.click();
  await waitCopied();
  check((await copiedText()).includes('+first'), 'Copy used another diff');
  await setWorkspace({ diffs: { [A]: hunks('first-updated'), [B]: hunks('second-file') } });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('workspace_changes'));
  await refresh.click();
  await waitBusy();
  check(await copy.isDisabled() && await copy.innerText() === '复制差异', 'A refreshing view still offered the previous diff for copying');
  const pickB = dialog.locator(`.review-file-row[title="${B}"]`);
  check(await pickB.isDisabled(), 'A refreshing view still allowed switching files');
  const beforePick = await diffCalls(B);
  await pickB.evaluate(row => row.click());
  await settle();
  check(await diffCalls(B) === beforePick && await activePath() === A, 'Picking a file during a refresh started another diff');
  check(await diffText() === '', 'The previous diff stayed readable during a refresh');
  check((await dialog.locator('.review-diff').innerText()).includes('正在刷新工作区变更'), 'A refreshing diff pane looked finished');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('workspace_changes'));
  await waitDiff('first-updated');
  await waitIdle();
  check(await activePath() === A, 'Refreshing moved the selection away from a surviving path');
  check(await diffCalls(A) === 2, 'The surviving selection was not re-read after the refresh');
  passed.push('a refresh re-reads the diff of the still selected path and clears the stale copy state');

  // 2. A selected path that disappeared falls back to the first new entry.
  await setWorkspace({
    entries: [entry(C), entry(D)],
    diffs: { [C]: hunks('third'), [D]: hunks('fourth') },
  });
  await refresh.click();
  await waitPath(C);
  await waitDiff('third');
  await waitIdle();
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'workspace_file_diff').at(-1).args.path) === C, 'The vanished selection was replaced by an unexpected path');
  check(await diffCalls(A) === 2, 'The vanished selection was still re-read');
  passed.push('a selected path missing from the refreshed report falls back to the first entry');

  // 3. An empty report clears the selection, the diff and the copy state.
  await setWorkspace({ entries: [], diffs: {} });
  await refresh.click();
  await waitIdle();
  await page.waitForFunction(() => !document.querySelector('.review-file-row'));
  check(await activePath() === '', 'An empty report kept a selected file');
  check(await dialog.locator('.review-diff-title').count() === 0, 'An empty report kept the copyable diff header');
  check((await dialog.locator('.review-diff').innerText()).includes('选择文件查看具体变更'), 'An empty report kept a diff pane');
  check((await dialog.locator('.review-files').innerText()).includes('当前工作区没有 Git 变更'), 'An empty report was not reported as empty');

  // 4. A non-repository report behaves the same, with its own message.
  await setWorkspace({ is_repo: false, message: '此目录不是 Git 仓库。' });
  await refresh.click();
  await waitIdle();
  await page.waitForFunction(() => !document.querySelector('.review-file-row'));
  check((await dialog.locator('.review-files').innerText()).includes('此目录不是 Git 仓库。'), 'A non-repository report lost its message');
  check(await dialog.locator('.review-diff-title').count() === 0, 'A non-repository report kept a copyable diff');
  passed.push('empty and non-repository reports clear selection, diff and copy state');

  // 5. A failed refresh clears the old view and can be recovered from the
  //    existing refresh button.
  await setWorkspace({ is_repo: true, message: null, entries: [entry(A)], diffs: { [A]: hunks('recovered') } });
  await refresh.click();
  await waitDiff('recovered');
  await waitIdle();
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.workspace_changes = '合成刷新失败'; });
  await refresh.click();
  await waitIdle();
  await dialog.getByRole('alert').waitFor();
  check((await dialog.getByRole('alert').innerText()).includes('合成刷新失败'), 'A failed refresh reported another error');
  check(await activePath() === '', 'A failed refresh kept a selected file');
  check(await dialog.locator('.review-diff-title').count() === 0, 'A failed refresh kept a copyable diff');
  await setWorkspace({ diffs: { [A]: hunks('retried') } });
  await refresh.click();
  await waitDiff('retried');
  await waitIdle();
  check(await activePath() === A, 'A successful retry did not restore the selection');
  check(await dialog.getByRole('alert').count() === 0, 'A successful retry kept the refresh error');
  passed.push('a failed refresh clears the old view and the existing button recovers it');

  // 6. A diff answer that arrives after the selection moved on is dropped.
  await setWorkspace({ entries: [entry(A), entry(B)], diffs: { [A]: hunks('fresh-a'), [B]: hunks('stale-candidate') } });
  await refresh.click();
  await waitDiff('fresh-a');
  await waitIdle();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('workspace_file_diff'));
  await dialog.locator(`.review-file-row[title="${B}"]`).click();
  check((await dialog.locator('.review-diff').innerText()).includes('正在读取差异'), 'Switching files did not report progress');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('workspace_changes'));
  await refresh.click();
  await waitBusy();
  check(await dialog.locator('.review-diff-title button').isDisabled(), 'A refresh left a copyable diff while another answer was in flight');
  await setWorkspace({ diffs: { [A]: hunks('fresh-a'), [B]: hunks('fresh-b') } });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('workspace_changes'));
  await waitDiff('fresh-b');
  await waitIdle();
  check(await activePath() === B, 'The refresh did not keep the path that was being read');
  await setWorkspace({ diffs: { [A]: hunks('fresh-a'), [B]: hunks('late-answer') } });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('workspace_file_diff'));
  await settle();
  check((await diffText()).includes('+fresh-b') && !(await diffText()).includes('late-answer'), 'A late diff answer overwrote the current one');
  await copy.click();
  check((await copiedText()).includes('+fresh-b'), 'Copy took a superseded diff');
  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  passed.push('a diff answer that outlived its request never replaces the current view');

  // 7. A clipboard receipt that arrives after a refresh or a file switch only
  //    speaks for the copy it was asked for.
  await setWorkspace({ diffs: { [A]: hunks('copy-a'), [B]: hunks('copy-b') } });
  await refresh.click();
  await waitDiff('copy-b');
  await waitIdle();
  await holdClipboard(true);
  await copy.click();
  check(await copy.innerText() === '复制差异', 'A copy reported success before the clipboard answered');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('workspace_changes'));
  await refresh.click();
  await waitBusy();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('workspace_changes'));
  await waitDiff('copy-b');
  await waitIdle();
  await answerClipboard('');
  await settle();
  check(await copy.innerText() === '复制差异', 'A late clipboard success revived the copy state of a replaced diff');
  await copy.click();
  await dialog.locator(`.review-file-row[title="${A}"]`).click();
  await waitDiff('copy-a');
  await holdClipboard(false);
  await copy.click();
  await waitCopied();
  check((await copiedText()).includes('+copy-a'), 'Copy used another diff after switching files');
  await answerClipboard('合成剪贴板失败');
  await settle();
  check(await copy.innerText() === '已复制', 'A late clipboard failure cleared the copy state of the current diff');
  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  passed.push('clipboard receipts arriving after a refresh or a file switch leave the current copy state alone');

  return { passed, runtimeErrors: errors };
}

import { installDesktopFixture } from './fixtures/desktop.mjs';

const file = path => ({ path, status: 'M', added: 1, removed: 1 });
const A = 'src/design-turn-0.svelte';
const B = 'src/design-turn-1.svelte';

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyTurnReview(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, {
    turnReview: {
      design: {
        checkpoints: {
          turns: [{ turn: 0, anchor: 0 }, { turn: 1, anchor: 2 }],
          diffs: [
            { turn: 0, files: 1, added: 1, removed: 1, truncated: false },
            { turn: 1, files: 1, added: 1, removed: 1, truncated: false },
          ],
        },
        files: { 0: [file(A)], 1: [file(B)] },
      },
      review: { checkpoints: { turns: [], diffs: [] } },
    },
  });
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  await page.getByRole('textbox', { name: '输入任务', exact: true }).waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const count = command => page.evaluate(command => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === command).length, command);
  const setFiles = files => page.evaluate(files => {
    Object.assign(window.__KROMA_UI_FIXTURE__.turnReview.design.files, files);
  }, files);
  const hold = (command, turn) => page.evaluate(({ command, turn }) => {
    window.__KROMA_UI_FIXTURE__.hold(command, turn);
  }, { command, turn });
  const release = (command, error, turn) => page.evaluate(({ command, error, turn }) => {
    window.__KROMA_UI_FIXTURE__.release(command, error, turn);
  }, { command, error, turn });
  // Nothing observable follows a dropped answer, so wait long enough for a
  // superseded promise to have resolved and then check it changed nothing.
  const settle = () => page.waitForTimeout(250);
  const changeButtons = page.getByRole('button', { name: /^本轮改动 \d+ 个/ });
  const turnDialog = turn => page.getByRole('dialog', { name: new RegExp(`本轮改动 · 第 ${turn} 轮`) });
  const anyTurnDialog = () => page.locator('dialog.review-overlay[aria-labelledby="turn-diff-heading"]');
  const errorBanner = page.locator('.error-banner');
  const waitTurnFiles = async before => page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'turn_files').length > before, before);
  const fileRow = (turn, path) => turnDialog(turn).locator('.review-file-row', { hasText: path });
  const closeTurnDialog = async turn => {
    await page.keyboard.press('Escape');
    await turnDialog(turn).waitFor({ state: 'hidden' });
  };
  const row = name => page.locator('.session-row').filter({ hasText: name }).locator('.session-main');
  const switchTo = async name => {
    const before = await count('switch_session');
    await row(name).click();
    await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
      .filter(call => call.command === 'switch_session').length > before, before);
    await page.waitForFunction(() => !document.querySelector('.session-preview-note'));
    await page.waitForFunction(() => {
      const input = document.querySelector('.composer textarea');
      return input !== null && !input.disabled;
    });
  };
  const waitChangeButtons = expected => page.waitForFunction(expected => {
    const visible = [...document.querySelectorAll('.transcript-inner button')]
      .filter(button => /^本轮改动 \d+ 个/.test(button.textContent ?? ''));
    return visible.length === expected;
  }, expected);

  // 1. A normal conversation opens the file list of the chosen turn.
  await turnDialog(1).waitFor({ state: 'hidden' });
  check(await changeButtons.count() === 2, 'The synthetic checkpoint turns did not produce two turn actions');
  await changeButtons.nth(1).click();
  await turnDialog(1).waitFor();
  await fileRow(1, B).waitFor();
  await closeTurnDialog(1);
  passed.push('a normal conversation opens the file list of the chosen turn');

  // 2. An empty file list is a real answer and opens no panel.
  await setFiles({ 0: [] });
  const emptyBefore = await count('turn_files');
  await changeButtons.nth(0).click();
  await waitTurnFiles(emptyBefore);
  await settle();
  check(await anyTurnDialog().count() === 0, 'A turn without files opened an empty panel');
  check(await errorBanner.count() === 0, 'A turn without files was reported as an error');
  passed.push('a legitimate empty file list opens no panel and reports nothing');

  // 3. Two quick picks keep only the newest one, even after the first answer
  //    arrives later.
  await setFiles({ 0: [file(A)], 1: [file(B)] });
  await hold('turn_files', 1);
  const quickBefore = await count('turn_files');
  await changeButtons.nth(1).click();
  await waitTurnFiles(quickBefore);
  await changeButtons.nth(0).click();
  await turnDialog(0).waitFor();
  await fileRow(0, A).waitFor();
  await release('turn_files', '', 1);
  await settle();
  check(await turnDialog(0).isVisible(), 'A slower answer for the previous turn replaced the newest choice');
  check(await fileRow(0, B).count() === 0,
    'The file list of the superseded turn stayed visible');
  await closeTurnDialog(0);
  passed.push('two quick turn picks keep the newest one');

  // 4. A failing request for the visible conversation still reports.
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.turn_files = '合成轮次文件读取失败'; });
  await changeButtons.nth(1).click();
  await errorBanner.waitFor();
  check((await errorBanner.innerText()).includes('合成轮次文件读取失败'), 'A current failure lost its message');
  check(await anyTurnDialog().count() === 0, 'A failing turn file list opened a panel');
  await errorBanner.getByRole('button', {name: '关闭错误提示', exact: true}).click();
  await errorBanner.waitFor({ state: 'hidden' });
  passed.push('a failing request for the visible conversation reports its own message');

  // 5. A file list held across A -> B -> A must not open the panel after the
  //    first A is shown again.
  await hold('turn_files', 1);
  const awayBefore = await count('turn_files');
  await changeButtons.nth(1).click();
  await waitTurnFiles(awayBefore);
  await switchTo('整理项目的下一步');
  await switchTo('让想法自然发生');
  await waitChangeButtons(2);
  await release('turn_files', '', 1);
  await settle();
  check(await anyTurnDialog().count() === 0, 'A held success from the first visit opened a panel on the returned conversation');
  check(await errorBanner.count() === 0, 'A held success from the first visit reported an error');
  const reopenBefore = await count('turn_files');
  await changeButtons.nth(1).click();
  await waitTurnFiles(reopenBefore);
  await turnDialog(1).waitFor();
  await fileRow(1, B).waitFor();
  await closeTurnDialog(1);
  passed.push('a held success across A -> B -> A never revives and a new pick still works');

  // 6. The same for a held failure: no error may surface on the wrong visit.
  await hold('turn_files', 1);
  const failBefore = await count('turn_files');
  await changeButtons.nth(1).click();
  await waitTurnFiles(failBefore);
  await switchTo('整理项目的下一步');
  await switchTo('让想法自然发生');
  await waitChangeButtons(2);
  await release('turn_files', '合成迟到的轮次失败', 1);
  await settle();
  check(await anyTurnDialog().count() === 0, 'A held failure opened a panel on the returned conversation');
  check(await errorBanner.count() === 0, 'A held failure from the first visit reported on the returned conversation');
  passed.push('a held failure across A -> B -> A never surfaces');

  // 7. A checkpoint answer held while the previous conversation is shown must
  //    not attach its turns to the preview of another conversation.
  //    Leave review for design with the design lookup held open.
  await switchTo('整理项目的下一步');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('checkpoint_turns'));
  const checkpointBefore = await count('checkpoint_turns');
  await switchTo('让想法自然发生');
  await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'checkpoint_turns').length > before, checkpointBefore);
  // Review is cached from the visits above, so its preview appears before the
  // Agent has switched, and the held design lookup is still in flight.
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('switch_session'));
  const previewBefore = await count('switch_session');
  await row('整理项目的下一步').click();
  await page.waitForFunction(before => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'switch_session').length > before, previewBefore);
  await page.locator('.session-preview-note').waitFor();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('checkpoint_turns'));
  await settle();
  check(await changeButtons.count() === 0,
    "A held checkpoint answer of the previous conversation attached turn changes to the preview");
  check(await errorBanner.count() === 0, 'A superseded checkpoint answer reported an error');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('switch_session'));
  await page.waitForFunction(() => !document.querySelector('.session-preview-note'));
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  await settle();
  check(await changeButtons.count() === 0,
    "The review conversation showed turn changes that belong to another conversation");
  await switchTo('让想法自然发生');
  await waitChangeButtons(2);
  passed.push('a held checkpoint answer never attaches turns to the preview of another conversation');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

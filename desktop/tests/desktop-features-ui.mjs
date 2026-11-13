import { installDesktopFixture } from './fixtures/desktop.mjs';

// A workspace with more conversations than the default fold, a second
// workspace holding the same session id, and answers that exercise the fence
// language tag without claiming syntax highlighting.
const LONG_LANGUAGE = 'x'.repeat(120);
const XSS_LANGUAGE = 'js<img/src=x/onerror=window.__LANG_XSS__=1>';
const TS_BLOCK = 'const pad = (n) =>\n  n < 10 ? "0" + String(n) : String(n);';
const LONG_BLOCK = 'const long = 1;';
const ANSWER = [
  '先看代码块。',
  '',
  '```ts',
  TS_BLOCK,
  '```',
  '',
  '没有声明语言：',
  '',
  '```',
  'plain block',
  '```',
  '',
  `\`\`\`${LONG_LANGUAGE}`,
  LONG_BLOCK,
  '```',
  '',
  `\`\`\`${XSS_LANGUAGE}`,
  'const unsafe = 1;',
  '```',
  '',
  '行内 `pad(1)` 不算代码块。',
].join('\n');

const SESSIONS = Array.from({ length: 9 }, (_, index) => ({
  id: `row-${index}`, name: `会话 ${index}`, recent: true, updated_at: 1794416400 - index * 3600,
}));

const same = (left, right) => JSON.stringify(left) === JSON.stringify(right);

/** Synthetic catalog plus Markdown content, installed on every navigation so a
 * reload models an app restart against the same real conversations. */
async function installBatchCatalog(page) {
  await installDesktopFixture(page);
  await page.addInitScript(({ sessions, answer }) => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    if (!fixture) return;
    fixture.state.sessions = sessions;
    fixture.state.session_groups = [
      { project: fixture.state.project, is_default: false, sessions },
      { project: 'D:/Kroma/Personal', is_default: true, sessions: [
        { id: 'row-0', name: '个人同 id', recent: true, updated_at: 1794000000 },
        { id: 'personal', name: '随手记下', recent: true },
      ] },
    ];
    fixture.state.snapshot.messages.at(-1).content = answer;
  }, { sessions: SESSIONS, answer: ANSWER });
}

export async function verifyDesktopFeatures(page, baseURL = 'http://127.0.0.1:1429/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installBatchCatalog(page);
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  // ---------- 1. keyboard shortcut reference ----------
  const shortcutButton = page.getByRole('button', { name: '快捷键', exact: true });
  const dialog = page.locator('dialog[aria-labelledby="shortcuts-heading"]');
  await shortcutButton.click();
  await dialog.waitFor();
  check(await dialog.evaluate(el => el.matches(':modal')), '快捷键说明不是真正的模态对话框');
  const text = await dialog.locator('.dialog-panel').innerText();
  for (const expected of ['Ctrl/Cmd', '搜索', '新建会话', '隐藏侧栏', '切换工作区', '设置', 'F1',
    'Enter', '换行', '排队', '不支持 Cmd', '@', '引用工作区文件', 'Tab', 'Esc']) {
    check(text.includes(expected), `快捷键说明缺少实际存在的行为：${expected}`);
  }
  check((await dialog.locator('kbd').allTextContents()).filter(key => key === 'Ctrl').length >= 1,
    '快捷键说明没有列出输入框的 Ctrl 组合');
  // Escape is owned by the existing modal action, including returning focus.
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'hidden' });
  check(await shortcutButton.evaluate(el => el === document.activeElement), '关闭快捷键说明后没有回到入口');
  // F1 opens it once, and a repeat press does not stack a second dialog.
  await page.keyboard.press('F1');
  await dialog.waitFor();
  await page.keyboard.press('F1');
  await page.waitForTimeout(120);
  check(await page.locator('dialog:modal').count() === 1, '重复按 F1 叠开了第二个对话框');
  // A composing input method must not fire the shortcut.
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'hidden' });
  await page.evaluate(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'F1', isComposing: true })));
  await page.waitForTimeout(120);
  check(await dialog.count() === 0, '输入法组合中的 F1 打开了快捷键说明');
  // An open dialog keeps the keyboard: F1 neither preempts nor closes it.
  await page.getByRole('button', { name: '会话设置', exact: true }).click();
  const settingsDialog = page.locator('dialog[aria-label="会话设置"]');
  await settingsDialog.waitFor();
  await page.keyboard.press('F1');
  await page.waitForTimeout(150);
  check(await dialog.count() === 0, 'F1 抢占了已经打开的对话框');
  check(await settingsDialog.evaluate(el => el.matches(':modal')), 'F1 关闭了已经打开的对话框');
  await page.keyboard.press('Escape');
  await settingsDialog.waitFor({ state: 'hidden' });
  // The original window shortcuts still work.
  await page.keyboard.press('Control+K');
  await page.locator('.session-search').waitFor();
  await page.keyboard.press('Escape');
  await page.locator('.session-search').waitFor({ state: 'hidden' });
  // Narrow windows keep the panel inside the viewport and closable by keyboard.
  await page.setViewportSize({ width: 380, height: 760 });
  await page.keyboard.press('F1');
  await dialog.waitFor();
  const narrow = await page.evaluate(() => {
    const panel = document.querySelector('.shortcuts-panel');
    const rect = panel.getBoundingClientRect();
    return {
      page: document.documentElement.scrollWidth <= window.innerWidth,
      inside: rect.x >= 0 && rect.right <= window.innerWidth + 1 && rect.top >= 0,
    };
  });
  check(narrow.page && narrow.inside, '窄窗下快捷键说明溢出了窗口');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'hidden' });
  await page.setViewportSize({ width: 1280, height: 850 });
  passed.push('快捷键说明可从侧栏和 F1 打开，键盘关闭，且不抢占其它对话框');

  // ---------- 2. code fence language tag ----------
  const codeBlocks = page.locator('.transcript-inner .code-block');
  await codeBlocks.first().waitFor();
  const labels = await page.$$eval('.transcript-inner .code-lang', nodes => nodes.map(node => ({
    text: node.textContent, title: node.title, truncated: node.scrollWidth > node.clientWidth,
  })));
  check(labels.length === 3, '未声明语言的代码块也被加了语言标签');
  check(same(labels.map(label => label.text), ['ts', LONG_LANGUAGE, XSS_LANGUAGE]),
    '语言标签不是代码块自己声明的原名');
  check(labels.every(label => label.title === label.text), '语言标签缺少完整原文的 title');
  check(labels[0].truncated === false && labels[1].truncated, '过长的语言标签没有截断');
  const inline = await page.$$eval('.transcript-inner .markdown :not(pre) > code', nodes => nodes.length);
  check(inline === 1, '行内代码数量变了');
  check(await page.locator('.transcript-inner .code-lang img, .transcript-inner .code-lang script').count() === 0,
    '语言标签把 HTML 当成了内容');
  check(await page.evaluate(() => !window.__LANG_XSS__), '语言标签执行了脚本');
  const bars = await page.$$eval('.transcript-inner .code-block-bar', nodes => nodes.map(node => {
    const bar = node.getBoundingClientRect();
    const tag = node.querySelector('.code-lang');
    const copy = node.querySelector('.code-copy').getBoundingClientRect();
    return {
      tagShare: tag ? (tag.getBoundingClientRect().width / bar.width) : 0,
      copyInside: copy.right <= bar.right + 1 && copy.left >= bar.left,
    };
  }));
  check(bars.length === 4 && bars.every(bar => bar.copyInside), '语言标签挤走了复制按钮');
  check(bars.filter(bar => bar.tagShare).every(bar => bar.tagShare <= 0.63), '语言标签超过了工具栏的一半');
  // Copy stays byte-exact: no language, no button text.
  await page.evaluate(() => {
    window.__LANG_CLIPBOARD__ = [];
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
      writeText: async value => { window.__LANG_CLIPBOARD__.push(value); },
    } });
  });
  await page.locator('.transcript-inner .code-copy').nth(0).click();
  await page.waitForFunction(() => window.__LANG_CLIPBOARD__.length === 1);
  check(await page.evaluate(() => window.__LANG_CLIPBOARD__[0]) === `${TS_BLOCK}\n`,
    '复制内容包含了语言标签或丢失了原文');
  passed.push('代码块语言标签来自 language-* 原文，过长截断，复制内容不变');

  // ---------- 3. pinned conversations ----------
  const group = index => page.locator('.session-folder').nth(index);
  const titles = async index => group(index).locator('.session-main > span').allTextContents();
  const pinnedKeyCount = () => page.evaluate(() =>
    Object.keys(localStorage).filter(key => key.startsWith('kroma.desktop.pinned.v1:')).length);
  const switchCalls = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'switch_session').length);
  check(same(await titles(0), SESSIONS.slice(0, 7).map(session => session.name)), '默认折叠不是最近的 7 条');
  check(same(await titles(1), ['个人同 id', '随手记下']), '第二个工作区的初始顺序不对');
  check(await page.locator('.session-row .session-pin').count() === 9, '不是每个会话行都有置顶按钮');
  check(await page.locator('.session-pin.on').count() === 0, '未置顶时也显示了置顶标记');
  // Folded-away conversations are reached through the existing "show more".
  await group(0).getByRole('button', { name: '查看更多…', exact: true }).click();
  check((await titles(0)).length === 9, '查看更多没有展开全部会话');
  const switchesBefore = await switchCalls();
  const sessionBefore = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id);
  await page.getByRole('button', { name: '置顶 会话 8', exact: true }).click();
  check(same((await titles(0))[0], '会话 8'), '置顶会话没有移到分组顶部');
  check(await group(0).locator('.session-row.pinned').count() === 1, '置顶会话没有可辨认标记');
  check(await group(0).getByRole('button', { name: '取消置顶 会话 8', exact: true }).count() === 1,
    '置顶会话没有取消入口');
  check(await group(0).getByRole('button', { name: '置顶 会话 8', exact: true }).count() === 0,
    '置顶后仍显示置顶按钮');
  check(await switchCalls() === switchesBefore, '点击置顶切换了会话');
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id) === sessionBefore,
    '点击置顶改变了当前会话');
  check(await page.locator('.session-select.on').count() === 0, '点击置顶触发了多选');
  check((await page.locator('.notice-banner').innerText()).includes('已置顶会话'), '置顶没有给出提示');
  check(await pinnedKeyCount() === 1, '置顶没有写入本机存储');
  passed.push('点击置顶不导航、不多选，并写到本机存储');

  // A reload is the same as reopening Desktop: pins survive and lead the group
  // even though the rest of it is folded back to the default 7.
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  check(same((await titles(0))[0], '会话 8'), '重启后置顶顺序丢失');
  check((await titles(0)).length === 8, '重启后置顶会话被折叠隐藏，或挤占了最近 7 条');
  check(same((await titles(0)).slice(1), SESSIONS.slice(0, 7).map(session => session.name)),
    '置顶之外的会话没有保持原来的最近顺序');
  check(await group(0).locator('.session-row.pinned').count() === 1, '重启后置顶标记丢失');
  passed.push('重启后保留置顶，且置顶项不被默认折叠隐藏');

  // Pinning is scoped to one workspace: the same id in another project is untouched.
  await page.getByRole('button', { name: '置顶 个人同 id', exact: true }).click();
  check(same((await titles(0))[0], '会话 8') && await group(0).locator('.session-row.pinned').count() === 1,
    '在另一个工作区置顶影响了第一个工作区');
  check(await group(1).locator('.session-row.pinned').count() === 1, '第二个工作区没有置顶标记');
  check(await pinnedKeyCount() === 2, '两个工作区共用了同一个置顶存储键');
  await page.getByRole('button', { name: '取消置顶 个人同 id', exact: true }).click();
  check(await group(1).locator('.session-row.pinned').count() === 0, '取消置顶没有移除标记');
  check(await group(1).getByRole('button', { name: '置顶 个人同 id', exact: true }).count() === 1,
    '取消置顶后没有恢复置顶入口');
  passed.push('每个工作区独立置顶，同一个 id 互不影响');

  // Search only reorders the matches it has: a pinned non-match stays hidden.
  await page.keyboard.press('Control+K');
  const search = page.locator('.session-search');
  await search.waitFor();
  await search.fill('会话 3');
  await page.waitForTimeout(500);
  check(same(await page.locator('.session-row .session-main > span').allTextContents(), ['会话 3']),
    '搜索泄露了不匹配的置顶会话');
  await search.fill('');
  await page.keyboard.press('Escape');
  await search.waitFor({ state: 'hidden' });
  check(same((await titles(0))[0], '会话 8'), '退出搜索后置顶顺序丢失');
  passed.push('搜索只排序匹配项，不强行展示不匹配的置顶会话');

  // Unpinning returns the conversation to the ordinary recent order.
  await page.getByRole('button', { name: '取消置顶 会话 8', exact: true }).click();
  check(same(await titles(0), SESSIONS.slice(0, 7).map(session => session.name)), '取消置顶没有回到最近的顺序');
  check(await group(0).locator('.session-row.pinned').count() === 0, '取消置顶后仍留有置顶标记');
  check(await pinnedKeyCount() === 0, '取消置顶没有清理本机存储');
  passed.push('取消置顶回到正常最近排序');

  // Corrupt storage is a missing preference, not a broken sidebar.
  await page.evaluate(project => {
    const normalized = project.replace(/\\/g, '/');
    const key = 'kroma.desktop.pinned.v1:' + encodeURIComponent(
      /^[a-z]:\//i.test(normalized) || normalized.startsWith('//') ? normalized.toLowerCase() : normalized);
    localStorage.setItem(key, '{not json');
  }, await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.session_groups[0].project));
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  check(await page.locator('.session-row.pinned').count() === 0, '损坏的置顶存储被当成了置顶');
  check(same(await titles(0), SESSIONS.slice(0, 7).map(session => session.name)), '损坏的置顶存储打乱了会话');
  passed.push('损坏的本机置顶数据不崩溃也不误解');

  // An unwritable store keeps the pin for this window and says so.
  await page.evaluate(() => {
    window.__REAL_SET_ITEM__ = Storage.prototype.setItem;
    Storage.prototype.setItem = () => { throw new Error('合成存储不可用'); };
  });
  await page.getByRole('button', { name: '置顶 会话 2', exact: true }).click();
  check(same((await titles(0))[0], '会话 2'), '存储失败时本次置顶没有生效');
  const notice = await page.locator('.notice-banner').innerText();
  check(notice.includes('仅本次有效'), '存储失败时谎称已持久化');
  await page.evaluate(() => { Storage.prototype.setItem = window.__REAL_SET_ITEM__; });
  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));
  check(await page.locator('.session-row.pinned').count() === 0, '存储失败后却声称置顶已保存');
  passed.push('本机存储不可用时置顶仅本次有效，并明确提示');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

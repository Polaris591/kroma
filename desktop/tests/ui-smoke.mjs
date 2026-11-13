import { installDesktopFixture } from './fixtures/desktop.mjs';

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyDesktopUi(page, baseURL = 'http://127.0.0.1:1420/') {
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
  const defaultBackground = await page.evaluate(() => getComputedStyle(document.querySelector('.main-area')).backgroundColor);
  check(await page.locator('.conversation-turn').count() === 2, 'Initial transcript lost turns');
  check(await page.locator('.session-select').count() === 0, 'Session rows reserve space for a checkbox');
  await page.getByRole('button', { name: '选择会话', exact: true }).click();
  check(await page.locator('.session-select').count() > 0, 'Selection mode did not reveal checkboxes');
  await page.getByRole('button', { name: '选择会话', exact: true }).click();
  check(await page.locator('.session-select').count() === 0, 'Leaving selection mode kept checkboxes');
  const actionsFit = await page.evaluate(() => {
    const row = document.querySelector('.sidebar-actions');
    const button = row.querySelector('.new-chat');
    return {
      overflows: row.scrollWidth > row.clientWidth + 1,
      whiteSpace: getComputedStyle(button).whiteSpace,
      height: Math.round(button.getBoundingClientRect().height),
    };
  });
  check(!actionsFit.overflows && actionsFit.whiteSpace === 'nowrap' && actionsFit.height <= 40,
    `New-session button is squeezed: ${JSON.stringify(actionsFit)}`);

  await page.getByRole('button', { name: '选择会话', exact: true }).click();
  await page.locator('.session-select').first().click();
  await page.locator('.bulk-delete').click();
  const bulkDialog = page.getByRole('dialog', { name: '删除 1 个会话？', exact: true });
  await bulkDialog.waitFor();
  await bulkDialog.getByRole('button', { name: '取消', exact: true }).click();
  await bulkDialog.waitFor({ state: 'hidden' });
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.every(call => call.command !== 'delete_session')), 'Cancelling a bulk delete removed a session');
  await page.getByRole('button', { name: '选择会话', exact: true }).click();

  const geometry = await page.evaluate(() => {
    const rect = selector => document.querySelector(selector).getBoundingClientRect().toJSON();
    return { reading: rect('.transcript-inner'), prompt: rect('.prompt'), notes: rect('.turn-notes') };
  });
  check(geometry.prompt.x > geometry.reading.x, 'User prompt must sit on the right');
  check(geometry.notes.right <= geometry.reading.right + 1, 'Process notes escaped the reading column');
  for (const width of [880, 780, 1280]) {
    await page.setViewportSize({ width, height: width === 780 ? 560 : 850 });
    await page.waitForFunction(() => {
      const reading = document.querySelector('.transcript-inner').getBoundingClientRect();
      const composer = document.querySelector('.composer-outer').getBoundingClientRect();
      return Math.abs(reading.x - composer.x) < 1 && Math.abs(reading.width - composer.width) < 1;
    });
  }
  passed.push('message hierarchy, aligned reading/composer columns and inline process notes');

  await page.getByRole('button', { name: '收起侧边栏', exact: true }).click();
  await page.locator('.sidebar').waitFor({ state: 'hidden' });
  await page.setViewportSize({ width: 780, height: 560 });
  await page.setViewportSize({ width: 1280, height: 850 });
  check(!await page.locator('.sidebar').isVisible(), 'Resize overwrote sidebar choice');
  await page.getByRole('button', { name: '展开侧边栏', exact: true }).click();
  await page.locator('.sidebar').waitFor();
  passed.push('wide sidebar collapses and survives resize');

  const model = page.getByRole('button', { name: '选择模型与思考强度', exact: true });
  await model.click();
  const menu = page.getByRole('dialog', { name: '模型与思考设置', exact: true });
  await menu.waitFor();
  check(await page.getByRole('textbox', { name: '搜索模型', exact: true }).evaluate(el => el === document.activeElement), 'Model search did not receive focus');
  const effortTops = await page.locator('.thinking-menu-row').evaluateAll(rows => rows.map(row => Math.round(row.getBoundingClientRect().top)));
  check(new Set(effortTops).size === 1, `Effort levels wrapped onto ${new Set(effortTops).size} rows`);
  check(await page.locator('.thinking-menu-row svg').count() === 0, 'Selected effort still shifts its label with an icon');
  await page.getByRole('group', { name: '当前临时思考强度' }).getByRole('button', { name: '高', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.reasoning_level === 'high');
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.profiles.profiles[0].reasoning === 'auto'), 'Temporary effort overwrote saved profile');
  if (!await menu.isVisible()) await model.click();
  await page.keyboard.press('Escape');
  await menu.waitFor({ state: 'hidden' });
  check(await model.evaluate(el => el === document.activeElement), 'Escape did not return focus to model trigger');
  passed.push('one model/effort panel, temporary effort and Escape focus');

  await page.locator('.note-toggle').last().click();
  await page.locator('.process-panel').waitFor();
  check((await page.locator('.process-panel').innerText()).includes('read_file'), 'Tool details disappeared');
  await page.locator('.note-toggle').last().click();
  await editor.fill('这段草稿应该留在这里');
  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: '返回对话', exact: true }).click();
  check(await editor.inputValue() === '这段草稿应该留在这里', 'Tab switch lost draft');
  passed.push('process details and draft survive view changes');

  await editor.fill('这是一段多行草稿。\n'.repeat(18));
  await page.locator('.transcript').evaluate(el => { el.scrollTop = 0; el.dispatchEvent(new Event('scroll')); });
  await page.getByRole('button', { name: '回到最新消息', exact: true }).waitFor();
  const latestBounds = await page.getByRole('button', { name: '回到最新消息', exact: true }).boundingBox();
  const regionBounds = await page.locator('.transcript-region').boundingBox();
  check(latestBounds.y + latestBounds.height <= regionBounds.y + regionBounds.height, 'Latest-message button overlaps the expanded composer');
  await page.getByRole('button', { name: '回到最新消息', exact: true }).click();
  await editor.fill('这段草稿应该留在这里');
  passed.push('latest-message control follows transcript, not input height');

  await page.evaluate(() => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    fixture.state.profiles.profiles = fixture.state.profiles.profiles.slice(0, 2);
    fixture.state.snapshot.messages.at(-1).content += '\n\n```ts\nconst longPath = "' + 'very-long-path/'.repeat(80) + '";\n```\n\n<script>window.__UI_TEST_ATTACKED__ = true</script>\n\n[unsafe](javascript:window.__UI_TEST_ATTACKED__=true)';
  });
  await model.click();
  await page.locator('.model-menu-row').nth(1).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.connected_profile === 'thinking');
  await model.click();
  check(await menu.getByRole('textbox', { name: '搜索模型', exact: true }).count() === 0, 'Small model list should not need search');
  check(await menu.locator('.model-menu-row[aria-pressed="true"]').evaluate(el => el === document.activeElement), 'Small model list did not focus the selected model');
  await page.keyboard.press('Escape');
  check(await page.evaluate(() => !window.__UI_TEST_ATTACKED__ && !document.querySelector('.markdown a[href^="javascript:"]')), 'Unsafe Markdown survived sanitizing');
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Long code widened the app');
  passed.push('model focus without search, safe Markdown and locally scrolling code');

  check(await page.locator('.mcp-notice').count() === 0, 'MCP approval notice is still present');
  check(await page.getByRole('button', { name: '批准并重连', exact: true }).count() === 0, 'MCP still requires approval');
  passed.push('MCP approval UI removed');

  await editor.fill('检查界面状态');
  await editor.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true });
  check(await page.evaluate(() => !window.__KROMA_UI_FIXTURE__.calls.some(call => call.command === 'send_message')), 'IME confirmation sent a task');
  await editor.press('Enter');
  await page.getByRole('button', { name: '停止', exact: true }).waitFor();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.stream({ kind: 'batch', events: [{ kind: 'text', text: '正在检查界面布局。' }] }));
  await editor.fill('再检查一下窄窗口');
  await editor.press('Control+Enter');
  await page.locator('.followup-heading').waitFor();
  check(await editor.inputValue() === '', 'Queue action did not clear submitted text');
  await editor.fill('提问结束后保留这段输入');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.stream({ kind: 'ask', ask: { id: 41, question: '优先检查哪一部分？', options: ['主界面', '插件区'], multiple: false } }));
  await page.getByRole('group', { name: '插件提问', exact: true }).waitFor();
  await page.getByRole('option', { name: '主界面', exact: true }).click();
  await page.getByRole('button', { name: '回答', exact: true }).click();
  await editor.waitFor();
  check(await editor.inputValue() === '提问结束后保留这段输入', 'Plugin question lost input');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.stream({ kind: 'ask', ask: { id: 42, question: '可以继续吗？', options: [], multiple: false } }));
  await page.getByRole('button', { name: '关闭问题', exact: true }).click();
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.some(call => call.command === 'answer_question' && call.args.id === 42 && call.args.dismissed)), 'Closing a question was not dismissed');
  await page.getByRole('button', { name: '停止', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.followup-heading')?.textContent.includes('已暂停'));
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.filter(call => call.command === 'send_message').length === 1), 'Cancellation ran a queued successor');
  passed.push('IME, running queue, plugin answer/dismiss, cancellation pauses queue');

  await page.reload();
  await editor.waitFor();
  await page.waitForFunction(() => document.querySelector('.followup-heading')?.textContent.includes('已暂停'));
  check(await page.evaluate(() => !window.__KROMA_UI_FIXTURE__.calls.some(call => call.command === 'send_message')), 'Restart automatically ran the queue');
  passed.push('restored queue never auto-runs');

  await page.evaluate(async () => {
    await window.__KROMA_UI_FIXTURE__.present({
      ...window.__KROMA_UI_FIXTURE__.presentation,
      colors: { background: '#faf9f6', text: '#1d2924', muted: '#50635a', accent: '#3c765c', border: '#c4cbc6' },
      layout: { content_width: 360, density: 'compact', sidebar: 'hidden' }, composer_max_rows: 1,
      widgets: ['header', 'transcript', 'above_composer', 'below_composer', 'footer', 'sidebar'].map(placement => ({
        plugin_id: 'ui-test', id: placement, placement, title: `测试位置 ${placement}`, text: '插件内容仍然可见。', blocks: [], actions: [],
      })),
      desktop_surfaces: [
        { plugin_id: 'frame', title: '自定义输入区', slot: 'above_composer', height: 96 },
        { plugin_id: 'sidebar-frame', title: '独立插件面板', slot: 'sidebar', height: 160 },
      ],
    });
  });
  await page.locator('.composer-extensions .plugin-desktop-iframe').waitFor();
  check(await page.evaluate(() => ['header', 'transcript', 'above_composer', 'below_composer', 'footer', 'sidebar']
    .every(placement => document.querySelectorAll(`.slot-${placement}`).length === 1)), 'A declared widget position disappeared');
  check(await page.locator('.transcript .slot-transcript').count() === 1, 'Transcript plugin is not inside the scroll area');
  check(await page.locator('.composer-extensions .slot-above_composer').count() === 1, 'Composer plugin escaped its extension region');
  const theme = await page.evaluate(() => ({
    text: getComputedStyle(document.querySelector('.markdown')).color,
    background: getComputedStyle(document.querySelector('.main-area')).backgroundColor,
    inputHeight: document.querySelector('.composer textarea').getBoundingClientRect().height,
  }));
  check(theme.text === 'rgb(29, 41, 36)', 'Markdown ignored plugin text colour');
  check(theme.background === 'rgb(250, 249, 246)', 'Main area ignored plugin background');
  check(theme.inputHeight <= 26.5, 'Composer ignored plugin row cap');
  passed.push('all widget positions, custom frame, light theme, density and row cap');

  await page.setViewportSize({ width: 780, height: 560 });
  await page.getByRole('button', { name: '插件区', exact: true }).waitFor();
  check(!await page.locator('.plugin-right-dock').isVisible(), 'Narrow plugin dock should start closed');
  await page.getByRole('button', { name: '插件区', exact: true }).click();
  await page.locator('.plugin-right-dock').waitFor();
  const dock = await page.locator('.plugin-right-dock').boundingBox();
  const dockFrame = await page.locator('.plugin-right-dock .plugin-desktop-iframe').elementHandle();
  check(dockFrame, 'Sidebar frame did not mount');
  check(dock.x >= 0 && dock.x + dock.width <= 780, 'Plugin drawer escaped the viewport');
  await page.getByRole('button', { name: '收起插件区', exact: true }).click();
  check(await dockFrame.evaluate(frame => frame.isConnected), 'Closing the drawer destroyed its frame');
  await dockFrame.dispose();
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Narrow window scrolls horizontally');
  passed.push('780×560 plugin drawer keeps frames mounted without page-wide overflow');

  await page.evaluate(async () => {
    await window.__KROMA_UI_FIXTURE__.present({ ...window.__KROMA_UI_FIXTURE__.presentation,
      desktop_surfaces: [{ plugin_id: 'workspace', title: '自由工作区', slot: 'workspace' }] });
  });
  await page.getByRole('button', { name: '回到对话', exact: true }).waitFor();
  await page.getByRole('button', { name: '回到对话', exact: true }).click();
  await editor.waitFor();
  check(await page.evaluate(background => getComputedStyle(document.querySelector('.main-area')).backgroundColor === background, defaultBackground), 'Removing theme did not restore default colours');
  passed.push('workspace takeover remains escapable and theme restores');
  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

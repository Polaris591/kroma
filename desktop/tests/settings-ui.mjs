import { installDesktopFixture } from './fixtures/desktop.mjs';

export async function verifyDesktopSettings(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page, {
    existingKey: true, changes: true,
    plugins: [{ id: 'ui-demo', name: '合成测试插件', version: '1.0', description: '无真实操作', enabled: true,
      project: true, settings: { '备注': '旧值' }, has_model: false, has_context: false, has_ui: true, has_desktop_ui: true }],
    presentation: { panels: [{ id: 'ui-demo', title: '查看合成面板', dynamic: false }],
      desktop_surfaces: [{ plugin_id: 'ui-demo', title: '自由插件界面', slot: 'panel' }] },
  });
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await editor.fill('保留这段对话草稿');
  await page.getByRole('button', { name: '设置', exact: true }).click();
  const form = page.locator('.settings-form');
  const model = form.getByLabel('模型 ID', { exact: true });
  const key = form.getByLabel('API Key', { exact: true });
  const profiles = page.getByRole('group', { name: '选择模型配置', exact: true });
  const onlySave = page.getByRole('button', { name: '仅保存', exact: true });
  const localError = form.locator('.settings-error');
  const add = page.getByRole('button', { name: '添加模型', exact: true });

  async function closeWithEscape(dialog, opener) {
    await dialog.waitFor();
    check(await dialog.evaluate(el => el.matches(':modal')), 'Dialog is not genuinely modal');
    for (let i = 0; i < 8; i++) {
      await page.keyboard.press('Tab');
      check(await dialog.evaluate(el => document.activeElement === document.body || el.contains(document.activeElement)), 'Tab focused a background control');
    }
    const count = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.filter(c => c.command === 'new_session').length);
    await page.keyboard.press('Control+N');
    check(await page.evaluate(n => window.__KROMA_UI_FIXTURE__.calls.filter(c => c.command === 'new_session').length === n, count), 'Modal shortcut created a background session');
    const bounds = await dialog.locator('.dialog-panel, .review-panel, .panel-card').boundingBox();
    check(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= (await page.viewportSize()).width + 1
      && bounds.y + bounds.height <= (await page.viewportSize()).height + 1, 'Dialog escaped the viewport');
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'hidden' });
    if (opener) check(await opener.evaluate(el => el === document.activeElement), 'Closing did not restore the opener');
  }

  await model.fill('draft-model');
  const pattern = await page.evaluate(() => {
    const input = document.querySelector('.settings-form input[pattern]');
    if (!input) return 'missing';
    try { new RegExp('^(?:' + input.pattern + ')$', 'v'); return 'ok'; } catch { return input.pattern; }
  });
  check(pattern === 'ok', `Config name pattern is invalid in this browser: ${pattern}`);
  await key.fill('synthetic-new-value');
  await profiles.getByRole('button', { name: /thinking/ }).click();
  await profiles.getByRole('button', { name: /^everyday/ }).click();
  check(await model.inputValue() === 'draft-model' && await key.inputValue() === 'synthetic-new-value', 'Switching profiles lost input');
  await page.getByRole('button', { name: '返回对话', exact: true }).click();
  check(await editor.inputValue() === '保留这段对话草稿', 'Settings lost the chat draft');
  await page.getByRole('button', { name: '设置', exact: true }).click();
  check(await model.inputValue() === 'draft-model' && await key.inputValue() === 'synthetic-new-value', 'Leaving settings lost its draft');
  check(await page.evaluate(() => !window.__KROMA_UI_FIXTURE__.calls.some(c => c.command === 'save_profile')), 'Drafts were saved without asking');
  passed.push('settings drafts survive profile/page switches without autosave');

  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.save_profile = '合成保存失败'; });
  await onlySave.click();
  await localError.waitFor();
  check(await model.inputValue() === 'draft-model' && await key.inputValue() === 'synthetic-new-value', 'Save failure lost input');
  await key.fill('');
  const sideEffects = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(c => c.command === 'connect' || c.command === 'set_default_profile').length);
  const beforeSave = await sideEffects();
  await onlySave.click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.profiles.profiles[0].model === 'draft-model');
  check(await sideEffects() === beforeSave, 'Only-save changed the connection or the default model');
  check(await page.evaluate(() => {
    const f = window.__KROMA_UI_FIXTURE__;
    const p = f.calls.filter(c => c.command === 'save_profile').at(-1).args.profile;
    return p.api_key == null && !p.clear_key && f.state.profiles.profiles[0].has_key;
  }), 'Only-save cleared an omitted key');
  await form.getByRole('checkbox', { name: /清除已保存的 API Key/ }).check();
  await onlySave.click();
  await page.waitForFunction(() => !window.__KROMA_UI_FIXTURE__.state.profiles.profiles[0].has_key);
  check(await key.inputValue() === '', 'Saving retained a submitted key in the editor');
  passed.push('save failure preserves input; omitted and explicit-clear keys stay distinct');

  await profiles.getByRole('button', { name: /thinking/ }).click();
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.set_default_profile = '合成默认设置失败'; });
  await page.getByRole('button', { name: '设为默认', exact: true }).click();
  await localError.waitFor();
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.profiles.active === 'everyday'), 'Failed default change was reported as saved');
  await page.getByRole('button', { name: '设为默认', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.profiles.active === 'thinking');
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.connect = '合成连接失败'; });
  await page.getByRole('button', { name: '连接此模型', exact: true }).click();
  await localError.waitFor();
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.connected_profile === 'everyday'), 'Failed connection changed the connected profile');
  passed.push('default and connection failures remain failures, with an editable settings page');

  await add.click();
  let dialog = page.getByRole('dialog', { name: '添加模型', exact: true });
  check(await dialog.getByLabel('模型 ID', { exact: true }).evaluate(el => el === document.activeElement), 'Add model did not focus model ID');
  await closeWithEscape(dialog, add);
  await add.click();
  await page.evaluate(async () => {
    await window.__KROMA_UI_FIXTURE__.present({ ...window.__KROMA_UI_FIXTURE__.presentation,
      live_panel: { plugin_id: 'ui-demo', title: '叠加插件面板', text: '不会关闭下面的编辑框。', blocks: [], actions: [] } });
  });
  await page.getByRole('dialog', { name: '叠加插件面板', exact: true }).waitFor();
  await page.keyboard.press('Escape');
  await page.getByRole('dialog', { name: '叠加插件面板', exact: true }).waitFor({ state: 'hidden' });
  check(await dialog.isVisible(), 'Escape closed the underlying model dialog');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'hidden' });
  passed.push('native modal keyboard range, opener focus and nested Escape ownership');

  await add.click();
  await dialog.getByLabel('模型 ID', { exact: true }).fill('new-ui-model');
  await dialog.getByLabel('接口地址', { exact: true }).fill('https://example.invalid/v1');
  await dialog.getByLabel('API Key', { exact: true }).fill('synthetic-new-value');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('connect'));
  await dialog.getByRole('button', { name: '保存并连接', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.profiles.profiles.some(p => p.name === 'new-ui-model'));
  await page.keyboard.press('Escape');
  check(await dialog.isVisible(), 'Escape hid an in-progress connection');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('connect', '合成连接失败'));
  await dialog.getByRole('alert').waitFor();
  check(await dialog.getByLabel('API Key', { exact: true }).inputValue() === 'synthetic-new-value', 'Connection failure lost form input');
  await dialog.getByRole('button', { name: '保存并连接', exact: true }).click();
  await dialog.waitFor({ state: 'hidden' });
  await editor.waitFor();
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.profiles.profiles.filter(p => p.name === 'new-ui-model').length === 1), 'Retry created duplicate profiles');
  check(await editor.evaluate(el => el === document.activeElement), 'Successful connection did not focus the chat input');
  check(await editor.inputValue() === '保留这段对话草稿', 'Successful connection lost the chat draft');
  passed.push('busy model dialog stays visible, saved-but-failed creation retries without duplication');

  const context = page.getByRole('button', { name: /上下文占用估算/ });
  await context.click();
  const abilities = page.getByRole('button', { name: /已加载 3 个工具/ });
  await abilities.click();
  await closeWithEscape(page.getByRole('dialog', { name: '本会话已加载的能力', exact: true }), abilities);
  await page.keyboard.press('Escape');
  const remove = page.getByRole('button', { name: '删除 整理项目的下一步', exact: true });
  await remove.focus();
  await remove.press('Enter');
  dialog = page.getByRole('dialog', { name: '删除此会话？', exact: true });
  check(await dialog.getByRole('button', { name: '取消', exact: true }).evaluate(el => el === document.activeElement), 'Delete focused the destructive choice');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('delete_session'));
  await dialog.getByRole('button', { name: '删除会话', exact: true }).click();
  await page.keyboard.press('Escape');
  check(await dialog.isVisible(), 'Escape hid an in-progress delete');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('delete_session', '合成删除失败'));
  await dialog.getByRole('alert').waitFor();
  await closeWithEscape(dialog, remove);
  passed.push('abilities return focus; delete starts safely and exposes failures inside the modal');

  const rewind = page.getByRole('button', { name: '回退', exact: true }).last();
  await rewind.click();
  dialog = page.getByRole('dialog', { name: '回到这一轮之前', exact: true });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('rewind_turn'));
  await dialog.getByRole('button', { name: '只回退对话', exact: true }).click();
  await page.keyboard.press('Escape');
  check(await dialog.isVisible(), 'Rewind confirmation vanished while applying');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('rewind_turn', '合成回退失败'));
  await dialog.getByRole('alert').waitFor();
  await dialog.getByRole('button', { name: '只回退对话', exact: true }).click();
  await dialog.waitFor({ state: 'hidden' });
  const turnChanges = page.getByRole('button', { name: /本轮改动 1 个/ });
  const workspaceChanges = page.getByRole('button', { name: /工作区变更 1/ });
  check(await turnChanges.isVisible() && await workspaceChanges.isVisible(), 'Turn changes hid the independent workspace view');
  await turnChanges.click();
  await closeWithEscape(page.getByRole('dialog', { name: /本轮改动 · 第/ }), turnChanges);
  await workspaceChanges.click();
  await closeWithEscape(page.getByRole('dialog', { name: '查看工作区变更', exact: true }), workspaceChanges);
  passed.push('rewind remains visible through failure/retry; both diff views keep working');

  await page.getByRole('button', { name: '设置', exact: true }).click();
  const storage = page.getByRole('button', { name: '本机数据与清理', exact: true });
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('storage_report'));
  await storage.click();
  dialog = page.getByRole('dialog', { name: '本机数据', exact: true });
  await closeWithEscape(dialog, storage);
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('storage_report'));
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.storage_report = '合成统计失败'; });
  await storage.click();
  await dialog.getByRole('alert').waitFor();
  await closeWithEscape(dialog, storage);
  await storage.click();
  await dialog.getByRole('button', { name: '清理全部空会话', exact: true }).waitFor();
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.hold('delete_session'));
  await dialog.getByRole('button', { name: '清理全部空会话', exact: true }).click();
  await dialog.getByRole('status').waitFor();
  await page.keyboard.press('Escape');
  check(await dialog.isVisible(), 'Escape hid an in-progress cleanup');
  await page.evaluate(() => window.__KROMA_UI_FIXTURE__.release('delete_session', '合成清理失败'));
  await dialog.getByRole('alert').waitFor();
  check((await dialog.getByRole('alert').innerText()).includes('1 个空会话未能清理'), 'Partial cleanup falsely reported complete success');
  await dialog.getByRole('button', { name: '清理全部空会话', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.storage[0].empty === 0);
  await closeWithEscape(dialog, storage);
  passed.push('storage reads can close; cleanup stays visible and reports partial failures');

  await page.getByRole('button', { name: '插件', exact: true }).click();
  await page.getByRole('button', { name: /合成测试插件/ }).click();
  const note = page.getByLabel('备注', { exact: true });
  await note.fill('新值');
  await page.getByRole('heading', { name: '合成测试插件', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.plugins[0].settings['备注'] === '新值');
  check(await page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.some(call => call.command === 'plugin_action' && call.args.action === 'set' && call.args.key === '备注')), 'Plugin text setting only saved on an explicit click');
  const panel = page.getByRole('button', { name: '查看合成面板', exact: true });
  await panel.click();
  await closeWithEscape(page.getByRole('dialog', { name: '合成插件面板', exact: true }), panel);
  const custom = page.getByRole('button', { name: '打开桌面自定义界面', exact: true });
  await custom.click();
  dialog = page.getByRole('dialog', { name: '自由插件界面', exact: true });
  await dialog.frameLocator('iframe').getByRole('button', { name: '插件按钮', exact: true }).click();
  check(await dialog.isVisible(), 'Typing inside plugin content closed the panel');
  await dialog.locator('.panel-head button').focus();
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'hidden' });
  check(await custom.evaluate(el => el === document.activeElement), 'Closing the custom frame did not return focus');
  passed.push('plugin management and custom iframe modals preserve keyboard ownership');

  await page.getByRole('navigation', { name: '设置分类', exact: true }).getByRole('button', { name: '模型', exact: true }).click();
  await page.setViewportSize({ width: 780, height: 560 });
  await add.click();
  await closeWithEscape(page.getByRole('dialog', { name: '添加模型', exact: true }), add);
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Narrow settings overflowed horizontally');
  passed.push('780×560 settings and modal bounds');
  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

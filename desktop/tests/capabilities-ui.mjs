import { installDesktopFixture } from './fixtures/desktop.mjs';

export async function verifyDesktopCapabilities(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  const duplicate = String.raw`duplicate plugin id 'sample': \\?\C:\Users\Example\.kroma\plugins\sample`;
  const mcpWarning = 'MCP inventory: 合成服务清单无法读取';
  const skillWarning = 'Skill inventory: 合成技能清单无法读取';
  await installDesktopFixture(page, {
    capabilityWarnings: {plugins: [duplicate, duplicate], mcp: [mcpWarning], skills: [skillWarning]},
    plugins: [
      {id: 'sample', name: '插件甲', description: '默认开启', enabled: true, settings: {}},
      {id: 'other', name: '插件乙', description: '默认关闭', enabled: false, settings: {}},
    ],
    presentation: {desktop_surfaces: [
      {plugin_id: 'sample', title: '插件甲页面', slot: 'tab'},
      {plugin_id: 'other', title: '插件乙页面', slot: 'tab'},
    ]},
  });
  await page.setViewportSize({width: 1280, height: 850});
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', {name: '输入任务', exact: true});
  await editor.waitFor();
  await editor.fill('切换能力时保留草稿');
  check(await page.locator('.mcp-notice').count() === 0, 'MCP approval notice remains');
  check(await page.locator('.sidebar-utility-nav').getByRole('button', {name: '设置', exact: true}).count() === 1
    && await page.locator('.sidebar-utility-nav').getByRole('button', {name: '插件', exact: true}).count() === 0,
    'Settings still has multiple management entry points');
  await page.getByRole('button', {name: '设置', exact: true}).click();
  const navigation = page.getByRole('navigation', {name: '设置分类', exact: true});
  for (const name of ['模型', '工具', 'MCP', 'Skills', '插件']) {
    check(await navigation.getByRole('button', {name, exact: true}).count() === 1, `Missing category ${name}`);
  }
  const group = (name, root = page) => root.getByRole('region', {name, exact: true});
  async function idle() {
    await page.waitForFunction(() => document.querySelector('.capability-settings')?.getAttribute('aria-busy') === 'false');
  }
  async function toggle(control, enabled) {
    await control.waitFor();
    if (await control.isChecked() !== enabled) await control.click();
    await idle();
    check(await control.isChecked() === enabled, `Checkbox did not become ${enabled}`);
  }
  async function select(control, value) {
    await control.waitFor();
    await control.selectOption(value);
    await idle();
    check(await control.inputValue() === value, `Session item did not become ${value}`);
  }
  const lastChange = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.filter(call => call.command === 'set_capability_items').at(-1)?.args);
  const changeCount = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls.filter(call => call.command === 'set_capability_items').length);

  await navigation.getByRole('button', {name: '工具', exact: true}).click();
  const read = group('工具').getByRole('checkbox', {name: 'read_file', exact: true});
  const shell = group('工具').getByRole('checkbox', {name: 'shell', exact: true});
  await read.waitFor();
  check(await group('工具').getByRole('checkbox').count() === 3, 'Tools are still controlled by a single switch');
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.set_capability_items = '合成设置失败'; });
  await read.click();
  await page.getByRole('alert').filter({hasText: '合成设置失败'}).waitFor();
  check(await read.isChecked(), 'Failed save displayed a false disabled state');
  await toggle(read, false);
  check(await shell.isChecked(), 'Disabling read_file disabled shell');
  check(JSON.stringify((await lastChange()).items) === '{"read_file":false}', 'A single change rewrote unrelated tools');
  check(await read.isVisible(), 'Disabled tool disappeared from settings');
  passed.push('each tool has its own switch; failure and retry stay truthful');

  await toggle(shell, false);
  const search = page.getByRole('searchbox', {name: '搜索能力', exact: true});
  await search.fill('read_file');
  const beforeFiltered = await changeCount();
  await page.getByRole('button', {name: '工具开启匹配项', exact: true}).click();
  await idle();
  check(await changeCount() === beforeFiltered + 1, 'Filtered bulk action performed multiple saves');
  check(JSON.stringify((await lastChange()).items) === '{"read_file":true}', 'Filtered bulk action changed hidden items');
  await search.fill('');
  check(!await shell.isChecked(), 'Filtered action enabled a non-matching tool');
  const beforeBulk = await changeCount();
  await page.getByRole('button', {name: '工具全部关闭', exact: true}).click();
  await idle();
  check(await changeCount() === beforeBulk + 1 && Object.keys((await lastChange()).items).length === 3, 'Bulk action is not one grouped change');
  check(await group('工具').getByRole('checkbox').evaluateAll(nodes => nodes.every(node => !node.checked)), 'Bulk-off left a tool enabled');
  await toggle(read, true);
  check(!await shell.isChecked() && !await group('工具').getByRole('checkbox', {name: 'write_file', exact: true}).isChecked(), 'Reopening one item after bulk-off reopened its siblings');
  await page.getByRole('button', {name: '工具全部开启', exact: true}).click();
  await idle();
  await toggle(read, false);
  passed.push('all-on/off are optional shortcuts; search only changes matching items');

  for (const [category, first, second] of [['MCP', 'sample-server', 'other-server'], ['Skills', 'sample-skill', 'other-skill']]) {
    await navigation.getByRole('button', {name: category, exact: true}).click();
    const region = group(category);
    await toggle(region.getByRole('checkbox', {name: first, exact: true}), false);
    check(await region.getByRole('checkbox', {name: second, exact: true}).isChecked(), `${category}: one item disabled its sibling`);
    check(await region.getByRole('checkbox').count() === 2, `${category}: disabled item disappeared`);
  }
  await navigation.getByRole('button', {name: '插件', exact: true}).click();
  const pluginRow = name => page.locator('.plugin-row').filter({has: page.getByText(name, {exact: true})});
  const pluginA = pluginRow('插件甲');
  const pluginB = pluginRow('插件乙');
  const inspectorSwitch = page.getByRole('button', {name: '切换插件', exact: true});
  async function pluginDefault(row, enabled) {
    await row.click();
    if (await inspectorSwitch.getAttribute('aria-pressed') !== String(enabled)) await inspectorSwitch.click();
    await page.waitForFunction(value => {
      const control = document.querySelector('.inspector-toggle button');
      return control?.getAttribute('aria-pressed') === value && !control.disabled;
    }, String(enabled));
  }
  check(await page.getByRole('heading', {name: '插件', exact: true}).count() === 1, 'Plugin management is duplicated');
  check(await page.locator('.capability-settings').count() === 0, 'Duplicate plugin activation list remains');
  check(await page.locator('.plugin-row').count() === 2, 'Plugin inventory lost an item');
  await pluginDefault(pluginA, false);
  check((await pluginB.innerText()).includes('未启用'), 'Disabling one plugin enabled another');
  await pluginDefault(pluginB, true);
  check((await pluginA.innerText()).includes('未启用'), 'Enabling one plugin re-enabled another');
  check(await page.evaluate(() => {
    const change = window.__KROMA_UI_FIXTURE__.calls.filter(call => call.command === 'plugin_action').at(-1).args;
    return change.action === 'enable' && change.id === 'other';
  }), 'A plugin change did not target the selected plugin');
  await pluginDefault(pluginA, true);
  await pluginDefault(pluginB, false);
  passed.push('MCP services, skills and plugins are independent; one plugin management section and one activation control');

  await page.getByRole('button', {name: '返回对话', exact: true}).click();
  check(await editor.inputValue() === '切换能力时保留草稿', 'Settings lost draft');
  const opener = page.getByRole('button', {name: '会话设置', exact: true});
  await opener.click();
  const dialog = page.getByRole('dialog', {name: '会话设置', exact: true});
  const sessionRead = group('工具', dialog).getByRole('combobox', {name: 'read_file', exact: true});
  const sessionShell = group('工具', dialog).getByRole('combobox', {name: 'shell', exact: true});
  await sessionRead.waitFor();
  check(await sessionRead.inputValue() === 'default' && (await sessionRead.locator('option:checked').innerText()).includes('关闭'), 'Item inheritance ignored its default');
  await select(sessionRead, 'on');
  await select(sessionShell, 'off');
  check(await group('工具', dialog).getByRole('combobox', {name: 'write_file', exact: true}).inputValue() === 'default', 'One override changed another tool');
  await select(group('MCP', dialog).getByRole('combobox', {name: 'sample-server', exact: true}), 'on');
  await select(group('Skills', dialog).getByRole('combobox', {name: 'sample-skill', exact: true}), 'on');
  check(await group('MCP', dialog).getByRole('combobox', {name: 'other-server', exact: true}).inputValue() === 'default', 'MCP override affected another service');
  check(await group('Skills', dialog).getByRole('combobox', {name: 'other-skill', exact: true}).inputValue() === 'default', 'Skill override affected another skill');
  passed.push('each session item independently overrides or inherits its own default');

  await select(group('插件', dialog).getByRole('combobox', {name: '插件甲', exact: true}), 'off');
  await select(group('插件', dialog).getByRole('combobox', {name: '插件乙', exact: true}), 'on');
  const defaultsUnchanged = await page.evaluate(async () => {
    const data = await window.__TAURI_INTERNALS__.invoke('capability_settings');
    return data.items.plugins.find(item => item.id === 'sample').default_enabled
      && !data.items.plugins.find(item => item.id === 'other').default_enabled;
  });
  check(defaultsUnchanged, 'Session selection changed global plugin defaults');
  await page.keyboard.press('Escape');
  await dialog.waitFor({state: 'hidden'});
  await page.getByRole('button', {name: '插件甲页面', exact: true}).waitFor({state: 'hidden'});
  await page.getByRole('button', {name: '插件乙页面', exact: true}).waitFor({state: 'visible'});
  check(await opener.evaluate(el => el === document.activeElement), 'Dialog did not restore focus');
  check(await editor.inputValue() === '切换能力时保留草稿', 'Saving session settings lost draft');
  passed.push('one plugin can be off while another default-disabled plugin is on, without changing defaults');

  await page.reload();
  await editor.waitFor();
  await opener.click();
  await sessionRead.waitFor();
  check(await sessionRead.inputValue() === 'on' && await sessionShell.inputValue() === 'off', 'Tool overrides did not survive reload');
  check(await group('插件', dialog).getByRole('combobox', {name: '插件乙', exact: true}).inputValue() === 'on', 'Plugin override did not survive reload');
  const skill = group('Skills', dialog).getByRole('combobox', {name: 'sample-skill', exact: true});
  await page.evaluate(() => { window.__KROMA_UI_FIXTURE__.failures.set_capability_items = '合成会话设置失败'; });
  await skill.selectOption('off');
  await dialog.getByRole('alert').filter({hasText: '合成会话设置失败'}).waitFor();
  check(await skill.inputValue() === 'on', 'Failed session save displayed a false override');
  await select(skill, 'default');
  check((await lastChange()).items['sample-skill'] === null, 'Reset did not remove only the selected override');
  check(await group('MCP', dialog).getByRole('combobox', {name: 'sample-server', exact: true}).inputValue() === 'on', 'Resetting a skill changed MCP');
  await page.keyboard.press('Escape');
  passed.push('per-item persistence and resetting one override do not rewrite another group');

  await page.getByRole('button', {name: '新建会话', exact: true}).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id.startsWith('new-'));
  await page.getByRole('button', {name: '插件甲页面', exact: true}).waitFor({state: 'visible'});
  await page.getByRole('button', {name: '插件乙页面', exact: true}).waitFor({state: 'hidden'});
  await opener.click();
  await sessionRead.waitFor();
  check(await sessionRead.inputValue() === 'default', 'New conversation inherited another conversation override');
  await page.keyboard.press('Escape');
  await page.getByRole('button', {name: /^让想法自然发生/}).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id === 'design');
  await page.getByRole('button', {name: '插件甲页面', exact: true}).waitFor({state: 'hidden'});
  await page.getByRole('button', {name: '插件乙页面', exact: true}).waitFor({state: 'visible'});
  await opener.click();
  await sessionRead.waitFor();
  check(await sessionRead.inputValue() === 'on', 'Switching conversations lost an override');
  await page.setViewportSize({width: 780, height: 560});
  const bounds = await dialog.locator('.dialog-panel').boundingBox();
  check(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= 781 && bounds.y + bounds.height <= 561, 'Item list made the dialog overflow');
  const beforeReset = await changeCount();
  await dialog.getByRole('button', {name: '工具全部跟随默认', exact: true}).click();
  await idle();
  check(await changeCount() === beforeReset + 1 && await sessionRead.inputValue() === 'default' && await sessionShell.inputValue() === 'default', 'Bulk inherit did not reset the tool group once');
  check(await group('插件', dialog).getByRole('combobox', {name: '插件乙', exact: true}).inputValue() === 'on', 'Tool reset affected plugins');
  await page.keyboard.press('Escape');
  passed.push('conversation isolation, live plugin UI restoration, narrow list bounds and scoped reset');

  await page.setViewportSize({width: 1280, height: 850});
  await page.getByRole('button', {name: '设置', exact: true}).click();
  await navigation.getByRole('button', {name: '插件', exact: true}).click();
  await page.getByRole('button', {name: '返回对话', exact: true}).click();
  await editor.fill('运行期间不切换能力');
  await editor.press('Enter');
  await page.getByRole('button', {name: '停止', exact: true}).waitFor();
  check(await opener.isDisabled(), 'Running conversation allows changes');
  await page.getByRole('button', {name: '设置', exact: true}).click();
  check(await navigation.getByRole('button', {name: '工具', exact: true}).isDisabled(), 'Running task allows defaults to change');
  await pluginA.click();
  check(await inspectorSwitch.isDisabled(), 'Running task allows plugin activation to change');
  await page.getByRole('button', {name: '返回对话', exact: true}).click();
  await page.getByRole('button', {name: '停止', exact: true}).click();
  passed.push('item changes never interrupt a running task');

  await page.getByRole('button', {name: '设置', exact: true}).click();
  await navigation.getByRole('button', {name: '工具', exact: true}).click();
  await read.waitFor();
  check(await page.locator('.settings-diagnostics').count() === 0, 'Plugin diagnostics leaked into tool settings');
  for (const [category, item, warning] of [['MCP', 'sample-server', mcpWarning], ['Skills', 'sample-skill', skillWarning]]) {
    await navigation.getByRole('button', {name: category, exact: true}).click();
    await page.getByRole('checkbox', {name: item, exact: true}).waitFor();
    const diagnostics = page.locator('.settings-diagnostics');
    check(await diagnostics.count() === 1 && await diagnostics.getAttribute('open') === null, `${category}: diagnostics are not collapsed`);
    check(await page.getByRole('alert').count() === 0, `${category}: inventory warnings are presented as failures`);
    await diagnostics.locator('summary').focus();
    await page.keyboard.press('Enter');
    check(await diagnostics.locator('code').innerText() === warning, `${category}: another inventory's warnings leaked in`);
  }
  await navigation.getByRole('button', {name: '插件', exact: true}).click();
  await page.locator('.plugin-list').waitFor();
  const diagnostics = page.locator('.settings-diagnostics');
  check((await diagnostics.locator('summary').innerText()).includes('（1）'), 'Repeated warnings were not deduplicated');
  await diagnostics.locator('summary').click();
  check(await diagnostics.locator('strong').innerText() === '插件 sample 存在同名来源', 'Duplicate diagnostic has no readable explanation');
  check(await diagnostics.locator('code').innerText() === duplicate, 'Original diagnostic details were lost');
  await page.setViewportSize({width: 380, height: 700});
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Long diagnostic path overflows a narrow window');
  await page.setViewportSize({width: 1280, height: 850});
  await page.evaluate(() => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    window.__TAURI_INTERNALS__.invoke = async (command, args) => {
      const result = await invoke(command, args);
      if (command === 'capability_settings') {
        delete result.warning_groups;
        result.warnings = ['旧版宿主能力检查提示'];
      }
      return result;
    };
  });
  await navigation.getByRole('button', {name: '工具', exact: true}).click();
  await read.waitFor();
  await page.locator('.settings-diagnostics summary').click();
  check(await page.locator('.settings-diagnostics code').innerText() === '旧版宿主能力检查提示', 'Legacy host diagnostics were silently discarded');
  passed.push('inventory diagnostics are scoped, collapsed, deduplicated, keyboard-accessible and compatible with older hosts');
  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return {passed, runtimeErrors: errors};
}

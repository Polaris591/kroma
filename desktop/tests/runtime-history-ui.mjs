import { installDesktopFixture } from './fixtures/desktop.mjs';

// Frozen pre-task native snapshots are deliberately stricter than the ordinary
// fixture. Streamed text stays separate from the durable message list.
export async function verifyRuntimeHistory(page, baseURL = 'http://127.0.0.1:1420/') {
  const passed = [], errors = [];
  const check = (condition, message) => { if (!condition) throw new Error(message); passed.push(message); };
  page.on('pageerror', error => errors.push(error.message));
  const project = 'D:/Kroma/Design-Sandbox';
  const full = Array.from({length: 90}, (_, index) => [
    {role: 'user', content: `A 历史提问 ${index}`},
    {role: 'assistant', content: `A 历史回答 ${index}`},
  ]).flat();
  const b = [{role: 'user', content: 'B 的独立历史'}, {role: 'assistant', content: 'B 的独立回答'}];
  await installDesktopFixture(page, {nativeRuntimeSnapshots: true,
    histories: {design: {full, windowSize: 120}, review: {full: b, windowSize: 120}}});
  await page.setViewportSize({width: 1280, height: 850});
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', {name: '输入任务', exact: true});
  await editor.waitFor();
  const open = async (target, name, id) => {
    await target.locator('.session-row').filter({hasText: name}).locator('.session-main').click();
    await target.waitForFunction(id => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id === id, id);
    await target.waitForFunction(() => document.querySelector('.transcript')?.getAttribute('aria-busy') === 'false');
  };
  const send = async (target, text) => {
    await target.getByRole('textbox', {name: '输入任务', exact: true}).fill(text);
    await target.getByRole('button', {name: '发送任务', exact: true}).click();
    await target.getByRole('button', {name: '停止', exact: true}).waitFor();
    await target.waitForFunction(text => window.__KROMA_UI_FIXTURE__.calls.some(call => call.command === 'send_message' && call.args.text === text), text);
  };
  await page.getByRole('button', {name: '加载更早的完整历史', exact: true}).click();
  await page.getByRole('button', {name: /^显示更早的/}).click();
  const earliest = page.locator('.transcript').getByText('A 历史提问 0', {exact: true});
  await earliest.waitFor();
  await send(page, 'A 的长任务');
  const events = [{kind: 'context', estimated: 20000, available: 183900}];
  for (let index = 0; index < 25; index++) {
    events.push({kind: 'tool_started', id: `call-${index}`, name: 'read_file', arguments: '{}'},
      {kind: 'tool_finished', id: `call-${index}`, name: 'read_file', result: 'durable tool result'});
  }
  events.push({kind: 'model_finished', request: 1, input: 19700, output: 800, cached: 10000},
    {kind: 'context', estimated: 70000, available: 183900});
  await page.evaluate(({project, events}) => window.__KROMA_UI_FIXTURE__.streamTo(project, 'design', {kind: 'batch', events}), {project, events});
  await page.getByRole('button', {name: '上下文占用估算 35%，点击查看详情', exact: true}).waitFor();
  await page.locator('.context-trigger').click();
  const details = page.getByRole('dialog', {name: '上下文使用详情', exact: true});
  check((await details.getByText('已占用合计', {exact: true}).locator('..').innerText()).includes('70.0k'), 'context updates every admitted request while the task is running');
  check((await details.getByText('输入', {exact: true}).locator('..').innerText()).includes('19.7k'), 'reported input usage updates before the task finishes');
  check((await details.getByText('输出', {exact: true}).locator('..').innerText()).includes('800'), 'reported output is not left at zero during tool loops');
  check(await details.locator('.context-legend').count() === 0 && (await details.innerText()).includes('最近请求估算'), 'live request totals do not retain stale or fabricated composition slices');
  await page.keyboard.press('Escape');

  await open(page, '整理项目的下一步', 'review');
  const bHistory = page.locator('.transcript').getByText('B 的独立历史', {exact: true});
  await bHistory.waitFor();
  await page.evaluate(project => window.__KROMA_UI_FIXTURE__.streamTo(project, 'design', {kind: 'batch', events: [
    {kind: 'context', estimated: 90000, available: 183900}, {kind: 'text_delta', text: 'A 继续执行，历史不能消失'},
  ]}), project);
  check((await page.locator('.context-trigger').getAttribute('aria-label')).includes('4%'), 'A background context update never changes B metrics');
  await open(page, '让想法自然发生', 'design');
  await page.getByRole('button', {name: '停止', exact: true}).waitFor();
  check(await earliest.isVisible(), 'returning to a running task retains already-opened older history');
  await page.getByRole('button', {name: '上下文占用估算 45%，点击查看详情', exact: true}).waitFor();
  check(await page.locator('.transcript').getByText('B 的独立历史', {exact: true}).count() === 0, 'returning to A never installs B history');
  check(await page.locator('.transcript').getByText('A 继续执行，历史不能消失', {exact: true}).isVisible(), 'returning to A keeps its own live output');
  check(await page.locator('.prompt.draft').filter({hasText: 'A 的长任务'}).count() === 1, 'the in-flight user prompt is not lost or duplicated');
  check(await page.getByRole('button', {name: /^显示更早的/}).count() === 0, 'navigation does not collapse the reading window back to 72 messages');
  await page.evaluate(project => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    fixture.state.snapshot.usage.coding = {calls: 1, input: 19700, output: 800, cached: 10000, reasoning: 0};
    fixture.finishSession(project, 'design');
  }, project);
  await page.getByRole('button', {name: '发送任务', exact: true}).waitFor();
  check(await earliest.isVisible(), 'task completion preserves the previously loaded historical prefix');
  await open(page, '整理项目的下一步', 'review');
  await open(page, '让想法自然发生', 'design');
  check(await earliest.isVisible(), 'a completed background snapshot does not shrink the restored reading window');

  // A first-turn ID can arrive after navigation to B. It must update A's own
  // task/queue/sidebar, not rename B or borrow B's messages.
  const migrating = await page.context().newPage();
  migrating.on('pageerror', error => errors.push(error.message));
  try {
    await installDesktopFixture(migrating, {empty: true, initialSession: 'unknown', nativeRuntimeSnapshots: true,
      histories: {review: {full: b, windowSize: 120}}});
    await migrating.goto(baseURL);
    await migrating.getByRole('textbox', {name: '输入任务', exact: true}).waitFor();
    await send(migrating, '尚未分配 ID 的第一项');
    await open(migrating, '整理项目的下一步', 'review');
    await migrating.evaluate(project => window.__KROMA_UI_FIXTURE__.migrateSession(project, 'unknown', 'allocated-a'), project);
    await migrating.waitForTimeout(50);
    check(await migrating.locator('.transcript').getByText('B 的独立历史', {exact: true}).isVisible()
      && await migrating.getByRole('button', {name: '停止', exact: true}).count() === 0,
      'a delayed real session ID never relabels the visible B conversation');
    await open(migrating, '新对话', 'allocated-a');
    await migrating.getByRole('button', {name: '停止', exact: true}).waitFor();
    check(await migrating.locator('.transcript').getByText('B 的独立历史', {exact: true}).count() === 0,
      'the allocated task reopens with its own transcript, not the previous conversation');
    await migrating.evaluate(project => window.__KROMA_UI_FIXTURE__.finishSession(project, 'allocated-a'), project);
  } finally {await migrating.close();}
  check(errors.length === 0, `no uncaught runtime/history errors (${errors.join('; ')})`);
  return {passed, runtimeErrors: errors};
}

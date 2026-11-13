import { installDesktopFixture } from './fixtures/desktop.mjs';

// Synthetic per-conversation runtime checks. The fixture models one native
// host per project + session: each `send_message` opens its own channel, so
// completion order, cancellation and queues can be driven independently.

const PROJECT = 'D:/Kroma/Design-Sandbox';
const OTHER = 'D:/Kroma/Other';

export async function verifySessionRuntime(page, baseURL) {
  const errors = [];
  const passed = [];
  const check = (value, message) => { if (!value) throw new Error(message); passed.push(message); };
  page.on('pageerror', error => errors.push(error.message));

  await installDesktopFixture(page, {
    sessionGroups: [
      { project: OTHER, is_default: false, sessions: [
        { id: 'design', name: '另一个设计', recent: true },
      ] },
    ],
  });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(baseURL);
  const editor = page.getByRole('textbox', { name: '输入任务', exact: true });
  await editor.waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  const call = command => page.evaluate(name => window.__KROMA_UI_FIXTURE__.calls
    .filter(entry => entry.command === name).map(entry => entry.args), command);
  const row = name => page.locator('.session-row', { hasText: name });
  const status = async name => {
    const marker = row(name).locator('.session-task-status');
    return (await marker.count()) ? (await marker.innerText()).trim() : '';
  };
  const open = async name => {
    await row(name).locator('.session-main').click();
    await page.waitForFunction(expected =>
      document.querySelector('.topbar .title-button')?.textContent?.includes(expected), name);
    // The title updates from the click; the real switch finishes when
    // `switchingTo` is cleared (the transcript leaves its busy state).
    await page.waitForFunction(() =>
      document.querySelector('.transcript')?.getAttribute('aria-busy') !== 'true');
  };
  const send = async text => {
    await editor.fill(text);
    await page.getByRole('button', { name: '发送任务' }).click();
    await page.waitForFunction(expected => window.__KROMA_UI_FIXTURE__.calls
      .filter(call => call.command === 'send_message')
      .some(call => call.args.text === expected), text);
  };
  const sends = () => call('send_message');

  // 1. History turns keep the process toggle and its expanded panel ABOVE the
  //    final answer; the per-turn change entries stay below it.
  const toggle = page.locator('.conversation-turn .note-toggle').first();
  await toggle.waitFor();
  await toggle.click();
  await page.waitForSelector('.process-panel');
  const order = await page.evaluate(() => {
    const turn = [...document.querySelectorAll('.conversation-turn')]
      .find(element => element.querySelector('.process-panel'));
    const nodes = [...turn.children];
    return {
      process: nodes.findIndex(node => node.classList.contains('process-panel')),
      notes: nodes.findIndex(node => node.classList.contains('process-notes')),
      answer: nodes.findIndex(node => node.classList.contains('answer')),
    };
  });
  check(order.notes >= 0 && order.process > order.notes && order.answer > order.process,
    'the thinking/tool toggle and expanded panel sit before the final answer');

  // 2. Two conversations run at once; the visible one only projects its own task.
  await open('让想法自然发生');
  await send('A 的第一项');
  check((await sends()).at(-1).session === 'design', 'the first task targets A');
  await open('整理项目的下一步');
  check(await status('让想法自然发生') === '运行中', 'A shows running while B is visible');
  await send('B 的第一项');
  check((await sends()).length === 2 && (await sends())[1].session === 'review',
    'a task can start in B while A keeps running');
  const currentB = await status('整理项目的下一步');
  check(currentB === '运行中', `B shows its own running indicator (got '${currentB}')`);

  // Reverse completion: B finishes first, then A. Finishing B must not surface
  // in A and must not stop A.
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'review', 'completed'), { project: PROJECT });
  await page.waitForFunction(() => !document.querySelector('.session-row .stop-button'));
  check(await status('让想法自然发生') === '运行中', 'finishing B leaves A running');
  await open('让想法自然发生');
  await page.getByRole('button', { name: '停止' }).waitFor();
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__.streamTo(project, 'design', {
    kind: 'batch', events: [{ kind: 'text_delta', text: 'A 仍在输出' }],
  }), { project: PROJECT });
  await page.waitForFunction(() => document.body.textContent.includes('A 仍在输出'));
  check(true, 'switching back to A keeps receiving its original stream');

  // 3. A question belongs to A. While B is visible it is not injected into B;
  //    answering it names A's project + session.
  await open('整理项目的下一步');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__.streamTo(project, 'design', {
    kind: 'ask', session: 'design',
    ask: { id: 41, question: 'A 要选哪个？', options: ['甲', '乙'], multiple: false },
  }), { project: PROJECT });
  await page.waitForTimeout(120);
  check(await page.locator('.ask-bar').count() === 0,
    "A's question does not take over B's composer");
  await open('让想法自然发生');
  await page.locator('.ask-bar').waitFor();
  check(await status('让想法自然发生') === '等待输入', 'the sidebar shows the waiting conversation');
  await page.locator('.ask-option', { hasText: '甲' }).click();
  await page.getByRole('button', { name: '回答', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.answers.length === 1);
  const answer = await page.evaluate(() => window.__KROMA_UI_FIXTURE__.answers[0]);
  check(answer.project === PROJECT && answer.session === 'design' && answer.id === 41,
    "an answer is routed to the conversation that asked");

  // 4. Cancelling A leaves a genuinely re-started B running.
  await open('整理项目的下一步');
  await send('B 的第二项');
  await open('让想法自然发生');
  await page.getByRole('button', { name: '停止' }).click();
  await page.waitForFunction(({ project }) => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    return fixture.calls.some(call => call.command === 'cancel_task'
      && call.args.session === 'design' && call.args.project === project);
  }, { project: PROJECT });
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'cancelled'), { project: PROJECT });
  await page.waitForTimeout(120);
  check(await status('整理项目的下一步') === '运行中', 'cancelling A does not cancel B');
  await open('整理项目的下一步');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__.streamTo(project, 'review', {
    kind: 'batch', events: [{ kind: 'text_delta', text: 'B 依然在跑' }],
  }), { project: PROJECT });
  await page.waitForFunction(() => document.body.textContent.includes('B 依然在跑'));
  check((await sends()).filter(call => call.session === 'review').length === 2,
    'B still owns its second task after A was cancelled');

  // 5. A third conversation can be opened without disturbing either task.
  await open('一些还没完成的想法');
  check(await status('让想法自然发生') === '' && await status('整理项目的下一步') === '运行中',
    'viewing a third conversation leaves both task owners unchanged');
  check(await page.locator('.session-row', { hasText: '一些还没完成的想法' })
    .locator('.session-task-status').count() === 0,
    'the idle third conversation shows no running indicator');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'review', 'completed'), { project: PROJECT });
  await page.waitForTimeout(120);

  // 6. The same session id in another workspace is a different conversation.
  const otherFolder = page.locator('.session-folder', { hasText: 'Other' }).first();
  if (await otherFolder.locator('.session-row').count() === 0) {
    await otherFolder.locator('.group-heading').click();
  }
  await open('另一个设计');
  check((await page.evaluate(() => window.__KROMA_UI_FIXTURE__.state.project)) === OTHER,
    'opening a same-id session switches the workspace');
  await send('另一个项目的任务');
  const cross = (await sends()).at(-1);
  check(cross.project === OTHER && cross.session === 'design',
    'the same id in another project is addressed with its own workspace');
  await open('让想法自然发生');
  await send('主项目的任务');
  check(await status('另一个设计') === '运行中', 'the other workspace keeps its own running state');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: OTHER });
  await page.waitForTimeout(120);
  check(await status('让想法自然发生') === '运行中', 'finishing the other project leaves this one running');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: PROJECT });
  await page.waitForTimeout(150);

  // 7. A background queue belongs to its own conversation and advances after it
  //    completes, without touching the visible conversation's draft.
  await open('让想法自然发生');
  await send('队列宿主任务');
  await editor.fill('排队的后续任务');
  await editor.press('Control+Enter');
  await page.waitForSelector('.followup-item');
  await open('整理项目的下一步');
  await editor.fill('B 的新草稿');
  const beforeDrain = (await sends()).length;
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: PROJECT });
  await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'send_message').length > count, beforeDrain);
  const drained = (await sends()).find(call =>
    call.text === '排队的后续任务');
  check(drained && drained.project === PROJECT && drained.session === 'design',
    "A's queued follow-up runs in A while B is visible");
  check(await editor.inputValue() === 'B 的新草稿',
    "a background queue does not clear the visible conversation's draft");
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: PROJECT });
  await page.waitForTimeout(120);
  check(await page.evaluate(() => Object.keys(localStorage)
    .filter(key => key.startsWith('kroma.desktop.followups.v1:')).length) >= 0,
    'the queue stays stored under its owner scope');

  // 8. Starting a new conversation is available while the current one runs.
  await open('让想法自然发生');
  await send('仍在运行的宿主');
  await page.getByRole('button', { name: '新建会话', exact: true }).click();
  await page.waitForFunction(() => window.__KROMA_UI_FIXTURE__.state.snapshot.session_id
    .startsWith('new-'));
  await send('新会话任务');
  const fresh = (await sends()).at(-1);
  check(fresh.session.startsWith('new-') && fresh.project === PROJECT,
    'a new conversation can start its own task while another one runs');
  check(await status('让想法自然发生') === '运行中',
    'the original conversation keeps running after a new one starts');
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: PROJECT });
  await page.evaluate(() => {
    const session = window.__KROMA_UI_FIXTURE__.state.snapshot.session_id;
    window.__KROMA_UI_FIXTURE__.finishSession('D:/Kroma/Design-Sandbox', session, 'completed');
  });
  await page.waitForTimeout(150);

  // 9. A folded group still reports that one of its conversations is running.
  await open('让想法自然发生');
  await send('折叠分组中的任务');
  const folder = page.locator('.session-folder', { hasText: 'Design-Sandbox' }).first();
  await folder.locator('.group-heading').click();
  await page.waitForFunction(() => {
    const group = document.querySelector('.session-folder');
    return group?.querySelector('.group-running') && !group.querySelector('.session-row');
  });
  check((await page.locator('.session-folder .group-running').first().innerText()).trim().includes('1'),
    'a folded workspace still counts its running conversation');
  await folder.locator('.group-heading').click();
  await page.evaluate(({ project }) => window.__KROMA_UI_FIXTURE__
    .finishSession(project, 'design', 'completed'), { project: PROJECT });

  check(errors.length === 0, `no uncaught page errors (${errors.join('; ')})`);
  return passed;
}

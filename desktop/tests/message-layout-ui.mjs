import { installDesktopFixture } from './fixtures/desktop.mjs';

// A short Chinese question, the width of the code it asks for, and one fenced
// block that must keep its own frame and its exact text.
const SHORT = '给我输出一段代码块';
const LONG = '请把下面这段界面整理逻辑重写成更清楚的样子：先区分提问和回答的语气，再让很长的说明在窄窗口里自然换行，同时保留附件、复制和键盘操作。';
const CODE = 'const pad = (n) =>\n  n < 10 ? "0" + String(n) : String(n);\n\n// 空行与缩进都要保留\nconst longPath = "src/very/long/path/that/keeps/going/for/the/narrow/window.ts";';
const CODE_TEXT = `${CODE}\n`;
const same = (left, right) => JSON.stringify(left) === JSON.stringify(right);

function messages(user, withAttachment = false) {
  return [
    {
      role: 'user', content: user,
      ...(withAttachment ? { provider_items: [{ type: 'kroma_attachment', name: 'layout-notes.md' }] } : {}),
    },
    { role: 'assistant', content: `先看这段实现。\n\n\`\`\`ts\n${CODE}\n\`\`\`` },
  ];
}

// Accepts a Playwright Page; the production app has no test or demo branches.
export async function verifyMessageLayout(page, baseURL = 'http://127.0.0.1:1428/') {
  const passed = [];
  const errors = [];
  const check = (value, message) => { if (!value) throw new Error(message); };
  page.on('pageerror', error => errors.push(error.message));
  await installDesktopFixture(page);
  await page.setViewportSize({ width: 1280, height: 850 });
  await page.goto(baseURL);
  await page.getByRole('textbox', { name: '输入任务', exact: true }).waitFor();
  await page.waitForFunction(() => !document.querySelector('.loading-screen'));

  await page.evaluate(() => {
    const clipboard = { texts: [], writeText(text) { clipboard.texts.push(text); return Promise.resolve(); } };
    window.__KROMA_CLIPBOARD__ = clipboard;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: clipboard });
  });

  const setMessages = value => page.evaluate(next => {
    window.__KROMA_UI_FIXTURE__.state.snapshot.messages = next;
  }, value);
  const connectCalls = () => page.evaluate(() => window.__KROMA_UI_FIXTURE__.calls
    .filter(call => call.command === 'connect').length);
  // Changing the model through the existing control re-reads the snapshot, so
  // new synthetic messages replace the transcript without reloading the window.
  const refresh = async index => {
    const before = await connectCalls();
    await page.getByRole('button', { name: '选择模型与思考强度', exact: true }).click();
    await page.locator('.model-menu-row').nth(index).click();
    await page.waitForFunction(count => window.__KROMA_UI_FIXTURE__.calls
      .filter(call => call.command === 'connect').length > count, before);
  };
  const waitPrompt = text => page.waitForFunction(expected =>
    document.querySelector('.turn-prompt .prompt-text')?.textContent === expected, text);
  // Geometry only: bubbles, action rows and the code frame are compared by
  // where they land, not by how they are built.
  const layout = () => page.evaluate(() => {
    const rect = selector => {
      const node = document.querySelector(selector);
      if (!node) return null;
      const box = node.getBoundingClientRect();
      return { x: box.x, y: box.y, width: box.width, height: box.height, top: box.top, bottom: box.bottom, left: box.left, right: box.right };
    };
    const textWidth = selector => {
      const node = document.querySelector(selector);
      if (!node || !node.firstChild) return 0;
      const range = document.createRange();
      range.selectNodeContents(node);
      return range.getBoundingClientRect().width;
    };
    const style = selector => {
      const node = document.querySelector(selector);
      if (!node) return null;
      const computed = getComputedStyle(node);
      return { opacity: Number(computed.opacity), borderTopWidth: computed.borderTopWidth };
    };
    const pre = document.querySelector('.transcript-inner .answer .code-block pre');
    return {
      turn: rect('.transcript-inner .conversation-turn'),
      prompt: rect('.transcript-inner .turn-prompt .prompt'),
      textWidth: textWidth('.transcript-inner .prompt-text'),
      actions: rect('.transcript-inner .prompt-actions'),
      actionsStyle: style('.transcript-inner .prompt-actions'),
      buttonsInPrompt: document.querySelectorAll('.transcript-inner .prompt button').length,
      attachment: rect('.transcript-inner .prompt .attachment-chip'),
      block: rect('.transcript-inner .answer .code-block'),
      bar: rect('.transcript-inner .answer .code-block-bar'),
      copy: rect('.transcript-inner .answer .code-copy'),
      pre: rect('.transcript-inner .answer .code-block pre'),
      blockStyle: style('.transcript-inner .answer .code-block'),
      preStyle: style('.transcript-inner .answer .code-block pre'),
      pageWidth: document.documentElement.scrollWidth,
      viewport: window.innerWidth,
      preScrolls: !!pre && pre.scrollWidth > pre.clientWidth,
    };
  });

  // 1. A short question keeps the size of its own text, and the actions sit
  //    outside the bubble, under its right edge.
  await setMessages(messages(SHORT));
  await refresh(1);
  await waitPrompt(SHORT);
  await page.mouse.move(0, 0);
  await page.waitForTimeout(250);
  let view = await layout();
  check(view.buttonsInPrompt === 0, 'The prompt bubble still contains its action buttons');
  check(Math.abs(view.prompt.width - (view.textWidth + 32)) <= 2,
    `The short bubble is not its text plus padding (bubble ${view.prompt.width}, text ${view.textWidth})`);
  check(Math.abs(view.prompt.height - 48) <= 3, `The short bubble height follows something other than one text line (${view.prompt.height})`);
  check(view.prompt.width < 182 && view.prompt.height < 80, 'The short bubble still carries the old action-sized box');
  check(view.actions.top >= view.prompt.bottom - 0.5, 'The actions are not below the bubble');
  check(view.actions.right <= view.prompt.right + 1 && view.prompt.right - view.actions.right <= 4,
    'The actions are not right-aligned under the bubble');
  check(view.actionsStyle.opacity === 0, 'The actions are visible before hover or focus');
  await page.locator('.transcript-inner .turn-prompt .prompt').hover();
  await page.waitForFunction(() => Number(getComputedStyle(document.querySelector('.transcript-inner .prompt-actions')).opacity) === 1);
  passed.push('a short prompt is sized by its text, with its actions outside and below');

  // 2. The same actions are reachable from the keyboard without a pointer.
  await page.mouse.move(0, 0);
  await page.waitForFunction(() => Number(getComputedStyle(document.querySelector('.transcript-inner .prompt-actions')).opacity) === 0);
  await page.locator('.transcript-inner .prompt-actions button').first().focus();
  await page.waitForFunction(() => Number(getComputedStyle(document.querySelector('.transcript-inner .prompt-actions')).opacity) === 1);
  passed.push('keyboard focus reveals the action row');

  // 3. The copy control sits in the code block's own frame, above the code, and
  //    copies exactly the fenced text.
  view = await layout();
  const inside = (inner, outer) => inner.left >= outer.left - 1 && inner.right <= outer.right + 1
    && inner.top >= outer.top - 1 && inner.bottom <= outer.bottom + 1;
  check(inside(view.bar, view.block) && inside(view.copy, view.block) && inside(view.pre, view.block),
    'The toolbar or the code escaped the code block frame');
  check(view.copy.bottom <= view.pre.top + 1, 'The copy control covers the code');
  check(view.blockStyle.borderTopWidth === '1px' && view.preStyle.borderTopWidth === '0px',
    'The code frame is drawn twice or not at all');
  check(Math.abs(view.block.width - view.pre.width) <= 2, 'The toolbar and the code do not share one frame width');
  await page.locator('.transcript-inner .answer .code-copy').click();
  await page.waitForFunction(() => document.querySelector('.transcript-inner .answer .code-copy')?.textContent.trim() === '已复制');
  check(same(await page.evaluate(() => [...window.__KROMA_CLIPBOARD__.texts]), [CODE_TEXT]),
    'The framed copy control no longer copies the exact block text');
  passed.push('the toolbar shares the code frame and copies exact text');

  // 4. Long text and an attachment wrap inside a narrow window without pushing
  //    the page, and the actions stay attached to the bubble.
  await setMessages(messages(LONG, true));
  await refresh(2);
  await waitPrompt(LONG);
  await page.setViewportSize({ width: 380, height: 780 });
  await page.waitForTimeout(250);
  const narrow = await layout();
  check(narrow.pageWidth <= narrow.viewport, 'The prompt or its actions widened the narrow window');
  check(narrow.prompt.width <= narrow.turn.width * 0.85 + 1, 'The long prompt ignored its 85% cap');
  check(narrow.prompt.height > 60, 'The long prompt did not wrap');
  check(inside(narrow.attachment, narrow.prompt), 'The attachment left the prompt bubble');
  check(narrow.actions.top >= narrow.prompt.bottom - 0.5, 'The actions left the bubble in a narrow window');
  check(narrow.actions.right <= narrow.viewport && narrow.actions.left >= 0, 'The action row left the narrow window');
  check(narrow.copy.right <= narrow.viewport, 'The code frame overflowed the narrow window');
  check(narrow.preScrolls, 'Long code stopped scrolling inside its own block');
  await page.locator('.transcript-inner .answer .code-block pre').evaluate(node => { node.scrollLeft = 120; });
  const scrolled = await layout();
  check(await page.locator('.transcript-inner .answer .code-block pre').evaluate(node => node.scrollLeft) > 0,
    'The long code did not scroll at all');
  check(Math.abs(scrolled.copy.x - narrow.copy.x) <= 0.5, 'Scrolling the code moved the copy control');
  passed.push('long text with an attachment wraps in a narrow window and keeps its actions');

  await page.setViewportSize({ width: 1280, height: 850 });
  await page.waitForTimeout(200);

  // 5. Compact density keeps the same structure and the attachment.
  await page.evaluate(async () => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    await fixture.present({ ...fixture.presentation, layout: { density: 'compact' } });
  });
  await page.waitForFunction(() => document.querySelector('.app-shell').classList.contains('compact'));
  const compact = await layout();
  check(compact.prompt && compact.actions, 'Compact density lost the prompt or its actions');
  check(compact.prompt.width <= compact.turn.width * 0.85 + 1 && compact.prompt.width >= compact.turn.width * 0.85 - 2,
    'Compact density did not wrap the long prompt at its 85% cap');
  check(compact.actions.top >= compact.prompt.bottom - 0.5, 'Compact density put the actions back inside the bubble');
  check(inside(compact.attachment, compact.prompt), 'Compact density dropped the attachment');
  check(compact.pre.top >= compact.bar.bottom - 1, 'Compact density overlapped the toolbar and the code');
  check(compact.pageWidth <= compact.viewport, 'Compact density overflowed the window');
  await page.evaluate(async () => {
    const fixture = window.__KROMA_UI_FIXTURE__;
    await fixture.present({ ...fixture.presentation, layout: {} });
  });
  passed.push('compact density keeps the bubble, attachment and code frame intact');

  check(errors.length === 0, `Browser runtime errors: ${errors.join('; ')}`);
  return { passed, runtimeErrors: errors };
}

import { describe, expect, it } from 'vitest';
import { latestAnswerLine, taskNotification } from './notify';
import type { Message } from './api';

const messages: Message[] = [
  { role: 'user', content: 'do it' },
  { role: 'assistant', content: 'first line\nsecond line' },
  { role: 'assistant', content: '   ' },
];

describe('latestAnswerLine', () => {
  it('uses the first non-empty line of the newest answer', () => {
    expect(latestAnswerLine(messages)).toBe('first line');
  });

  it('falls back when there is no answer', () => {
    expect(latestAnswerLine([])).toBe('任务已完成');
    expect(latestAnswerLine([{ role: 'user', content: 'hi' }])).toBe('任务已完成');
  });

  it('truncates a long line', () => {
    expect(latestAnswerLine([{ role: 'assistant', content: 'x'.repeat(120) }])).toHaveLength(60);
  });
});

describe('taskNotification', () => {
  it('stays silent for a cancelled task', () => {
    expect(taskNotification('cancelled', 'Chat', messages)).toBeNull();
  });

  it('reports a failure without leaking transcript text', () => {
    expect(taskNotification('failed', 'Chat', messages)).toEqual({
      title: 'Chat · 执行失败', body: '本轮执行失败，打开窗口查看原因',
    });
  });

  it('announces the answer line on success and falls back to the app name', () => {
    expect(taskNotification('completed', '', messages)).toEqual({
      title: 'Kroma', body: 'first line',
    });
  });
});

it('announces a paused goal instead of its rejected completion text', () => {
  const paused = {owner:'goal', id:'g', revision:1, status:'paused' as const, reason:'BLOCKED', summary:'等待凭据'};
  expect(taskNotification('completed', 'Chat', messages, paused)).toEqual({
    title:'Chat · 目标已暂停', body:'进度已保留，打开窗口查看原因并继续',
  });
});

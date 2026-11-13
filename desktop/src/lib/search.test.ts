import { describe, expect, it } from 'vitest';
import type { Message } from './api';
import {
  findCountLabel, findInMessages, findScopeNote, findableText, stepHit,
} from './search';

const messages: Message[] = [
  { role: 'system', content: 'Sensitive system prompt Search' },
  { role: 'user', content: '请检查 Kroma 的 Search 功能' },
  { role: 'tool', content: 'Search tool output should never match' },
  { role: 'assistant', reasoning_content: 'Search reasoning', content: '已经完成搜索。' },
  { role: 'assistant', content: 'Search is done and 可以复制代码' },
  { role: 'user', content: '继续', provider_items: [{ type: 'kroma_context_reference' }] },
  { role: 'assistant', content: 'hidden', provider_items: [{ type: 'kroma_continuation' }] },
];

describe('in-conversation find', () => {
  it('searches user prompts and assistant answers only', () => {
    const hits = findInMessages(messages, 'search');
    expect(hits.map(hit => hit.messageIndex)).toEqual([1, 4]);
    expect(hits.map(hit => hit.role)).toEqual(['user', 'assistant']);
  });

  it('keeps absolute indexes when only a recent window is loaded', () => {
    const window = messages.slice(3);
    expect(findInMessages(window, 'search', 3).map(hit => hit.messageIndex)).toEqual([4]);
  });

  it('matches mixed Chinese/English text without treating it as a pattern', () => {
    expect(findInMessages(messages, '可以复制').map(hit => hit.messageIndex)).toEqual([4]);
    // A plain substring, not a regular expression or a word-prefix rule.
    expect(findInMessages(messages, 'Kroma 的 search').map(hit => hit.messageIndex)).toEqual([1]);
    expect(findInMessages(messages, 'Kroma.*search')).toEqual([]);
    expect(findInMessages(messages, '.')).toEqual([]);
    expect(findInMessages(messages, ' ')).toEqual([]);
  });

  it('never searches system, tool, reasoning or host references', () => {
    expect(findInMessages(messages, 'sensitive')).toEqual([]);
    expect(findInMessages(messages, 'tool output')).toEqual([]);
    expect(findInMessages(messages, 'reasoning')).toEqual([]);
    expect(findInMessages(messages, 'hidden')).toEqual([]);
    expect(findableText(messages[0])).toBeNull();
    expect(findableText(messages[6])).toBeNull();
  });

  it('wraps navigation in both directions and labels the count', () => {
    const hits = findInMessages(messages, 'search');
    expect(stepHit(hits, 1, 1)).toBe(0);
    expect(stepHit(hits, 0, -1)).toBe(1);
    expect(stepHit([], 0, 1)).toBe(-1);
    expect(findCountLabel(hits, 0)).toBe('1/2');
    expect(findCountLabel([], -1)).toBe('0');
  });

  it('states the searched range instead of claiming the whole history', () => {
    expect(findScopeNote(120, true, false)).toContain('更早历史尚未加载');
    expect(findScopeNote(40, false, false)).toContain('40 条');
    expect(findScopeNote(40, false, true)).toContain('不会触发新的模型或工具调用');
  });
});

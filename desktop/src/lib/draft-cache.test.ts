import { describe, expect, it } from 'vitest';
import { DraftCache, memoryDraftKey } from './draft-cache';

describe('memoryDraftKey', () => {
  it('distinguishes an unsent conversation from a persisted one', () => {
    expect(memoryDraftKey('D:/app', null)).toBe('D:/app::new');
    expect(memoryDraftKey('', undefined)).toBe('::new');
    expect(memoryDraftKey('D:/app', 's1')).toBe('D:/app::s1');
  });
});

describe('DraftCache', () => {
  it('remembers text and paths per conversation', () => {
    const cache = new DraftCache();
    cache.remember('p', 's1', { text: 'hello', paths: ['a.png'] });
    expect(cache.recall('p', 's1')).toEqual({ text: 'hello', paths: ['a.png'] });
    expect(cache.recall('p', 's2')).toBeUndefined();
  });

  it('drops an empty draft instead of storing it', () => {
    const cache = new DraftCache();
    cache.remember('p', 's1', { text: '  ', paths: [] });
    expect(cache.size).toBe(0);
    expect(cache.recall('p', 's1')).toBeUndefined();
  });

  it('keeps attachments without text', () => {
    const cache = new DraftCache();
    cache.remember('p', 's1', { text: '', paths: ['a.png'] });
    expect(cache.size).toBe(1);
  });

  it('forgets exactly one conversation', () => {
    const cache = new DraftCache();
    cache.remember('p', 's1', { text: 'a', paths: [] });
    cache.remember('p', 's2', { text: 'b', paths: [] });
    cache.forget('p', 's1');
    expect(cache.recall('p', 's1')).toBeUndefined();
    expect(cache.recall('p', 's2')).toEqual({ text: 'b', paths: [] });
  });
});

import { describe, expect, it } from 'vitest';
import { extendSelection, parseSelectionKey, sessionSelectionKey, toggleSelection } from './selection';

describe('selection keys', () => {
  it('round-trips a project and id containing no separator', () => {
    const key = sessionSelectionKey('D:/work/app', 'default');
    expect(key).toBe('D:/work/app\u0000default');
    expect(parseSelectionKey(key)).toEqual({ project: 'D:/work/app', id: 'default' });
  });

  it('rejects a key without a separator', () => {
    expect(parseSelectionKey('plain')).toBeNull();
  });

  it('keeps the first separator so ids may contain one', () => {
    expect(parseSelectionKey('proj\u0000a\u0000b')).toEqual({ project: 'proj', id: 'a\u0000b' });
  });
});

describe('toggleSelection', () => {
  it('adds then removes without mutating the input', () => {
    const start = new Set<string>();
    const added = toggleSelection(start, 'p', '1');
    expect(start.size).toBe(0);
    expect(added.has(sessionSelectionKey('p', '1'))).toBe(true);
    expect(toggleSelection(added, 'p', '1').size).toBe(0);
  });
});

describe('extendSelection', () => {
  it('selects the inclusive run in either direction', () => {
    expect([...extendSelection(new Set(), ['a', 'b', 'c', 'd'], 'p', 'c', 'a')]).toEqual([
      sessionSelectionKey('p', 'a'), sessionSelectionKey('p', 'b'), sessionSelectionKey('p', 'c'),
    ]);
  });

  it('returns the input when an id is missing', () => {
    const selected = new Set<string>();
    expect(extendSelection(selected, ['a'], 'p', 'a', 'zz')).toBe(selected);
  });
});

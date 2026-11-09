import { describe, expect, it } from 'vitest';
import { canActOnTurn, turnDiffFor, turnNumberByUserIndex } from './checkpoints';
import type { Message } from './api';

const messages: Message[] = [
  { role: 'system', content: 'rules' },
  { role: 'user', content: 'first' },
  { role: 'assistant', content: 'a' },
  { role: 'user', content: 'second' },
  { role: 'assistant', content: 'b' },
  { role: 'user', content: 'third' },
];

describe('turnNumberByUserIndex', () => {
  it('maps each anchor to the newest user message at or before it', () => {
    const map = turnNumberByUserIndex(
      [{ turn: 1, anchor: 1 }, { turn: 2, anchor: 4 }, { turn: 3, anchor: 99 }], messages,
    );
    expect([...map.entries()]).toEqual([[1, 1], [3, 2], [5, 3]]);
  });

  it('skips turns without an anchor and empty input', () => {
    expect(turnNumberByUserIndex([{ turn: 1, anchor: null }], messages).size).toBe(0);
    expect(turnNumberByUserIndex([], messages).size).toBe(0);
  });

  it('skips an anchor that precedes every user message', () => {
    expect(turnNumberByUserIndex([{ turn: 7, anchor: 0 }], messages).size).toBe(0);
  });
});

describe('canActOnTurn', () => {
  it('accepts only indices pointing at a user message', () => {
    expect(canActOnTurn(messages, 1)).toBe(true);
    expect(canActOnTurn(messages, 2)).toBe(false);
    expect(canActOnTurn(messages, -1)).toBe(false);
    expect(canActOnTurn(messages, 6)).toBe(false);
  });
});

describe('turnDiffFor', () => {
  it('looks up a diff only for a known turn', () => {
    const diffs = new Map([[3, { turn: 3, added: 4, removed: 2, files: 1, truncated: false }]]);
    expect(turnDiffFor(diffs, 3)?.added).toBe(4);
    expect(turnDiffFor(diffs, 4)).toBeNull();
    expect(turnDiffFor(diffs, null)).toBeNull();
  });
});

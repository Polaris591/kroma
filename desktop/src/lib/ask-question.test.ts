import {describe, expect, it} from 'vitest';
import {
  buildAnswer, canAnswer, dismissAnswer, moveCursor, nextAsk, readAsk, togglePick,
  type AskQuestion,
} from './ask-question';

const ask = (): AskQuestion => ({
  id: 7, question: 'Which database?', options: ['Postgres', 'SQLite'], multiple: false,
});

describe('plugin question stream message', () => {
  it('reads the ask payload and answers the superseded question first', () => {
    const incoming = readAsk({id: 7, question: 'Which database?', options: ['Postgres', 'SQLite'], multiple: false});
    expect(incoming).toEqual(ask());
    const first = nextAsk(null, incoming!);
    expect(first.pending).toEqual(ask());
    expect(first.dismiss).toBeNull();
    const newer = readAsk({id: 8, question: 'Which cache?', options: [], multiple: true})!;
    const replaced = nextAsk(first.pending, newer);
    expect(replaced.pending.id).toBe(8);
    expect(dismissAnswer(replaced.dismiss!)).toEqual({id: 7, picked: [], typed: null, dismissed: true});
  });

  it('ignores a malformed payload instead of opening an unanswerable bar', () => {
    expect(readAsk(null)).toBeNull();
    expect(readAsk('ask')).toBeNull();
    expect(readAsk({id: 'x', question: 'Why?'})).toBeNull();
    expect(readAsk({id: 1, question: '   '})).toBeNull();
    expect(readAsk({id: 2, question: 'Any files?', options: 'nope', multiple: 'yes'}))
      .toEqual({id: 2, question: 'Any files?', options: [], multiple: false});
  });

  it('repeats the same question id without dismissing anything', () => {
    const repeated = nextAsk(ask(), {...ask()});
    expect(repeated.pending.id).toBe(7);
    expect(repeated.dismiss).toBeNull();
  });
});

describe('answer payload', () => {
  it('sends the picked labels of a single choice', () => {
    expect(buildAnswer(ask(), ['Postgres'], '')).toEqual(
      {id: 7, picked: ['Postgres'], typed: null, dismissed: false});
    // A single choice never carries two labels, even if both were toggled.
    expect(buildAnswer(ask(), ['Postgres', 'SQLite'], '')).toMatchObject({picked: ['Postgres']});
  });

  it('sends a typed answer without inventing a pick', () => {
    expect(buildAnswer(ask(), [], '  MySQL ')).toEqual(
      {id: 7, picked: [], typed: 'MySQL', dismissed: false});
    expect(buildAnswer({...ask(), multiple: true}, ['SQLite'], 'MySQL')).toEqual(
      {id: 7, picked: ['SQLite'], typed: 'MySQL', dismissed: false});
    expect(canAnswer([], '   ')).toBe(false);
    expect(canAnswer(['SQLite'], '')).toBe(true);
  });

  it('marks closing as dismissed and never as approval', () => {
    expect(dismissAnswer(7)).toEqual({id: 7, picked: [], typed: null, dismissed: true});
  });
});

describe('question bar cursor and picks', () => {
  it('wraps the highlight and starts at the first option', () => {
    expect(moveCursor(-1, 1, 2)).toBe(0);
    expect(moveCursor(-1, -1, 2)).toBe(1);
    expect(moveCursor(1, 1, 2)).toBe(0);
    expect(moveCursor(0, -1, 2)).toBe(1);
    expect(moveCursor(0, 1, 0)).toBe(-1);
  });

  it('toggles a pick for multiple choice and replaces it for single choice', () => {
    expect(togglePick([], 'SQLite', true)).toEqual(['SQLite']);
    expect(togglePick(['SQLite'], 'SQLite', true)).toEqual([]);
    expect(togglePick([], 'SQLite', false)).toEqual(['SQLite']);
    expect(togglePick(['SQLite'], 'Postgres', false)).toEqual(['Postgres']);
    expect(togglePick(['SQLite'], 'SQLite', false)).toEqual([]);
  });
});

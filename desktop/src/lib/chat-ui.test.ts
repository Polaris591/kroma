import { describe, expect, it } from 'vitest';
import { filterSessions, scrollTopAfterPrepend } from './chat-ui';

const sessions = [
  {id: 'first', name: 'Rust Refactor', recent: true},
  {id: 'desktop-2', name: 'UI work', recent: false},
  {id: 'third', name: null, recent: false},
];

describe('Desktop session search', () => {
  it('matches names and IDs without changing ordering', () => {
    expect(filterSessions(sessions, ' rUsT ')).toEqual([sessions[0]]);
    expect(filterSessions(sessions, 'DESKTOP')).toEqual([sessions[1]]);
    expect(filterSessions(sessions, 'none')).toEqual([]);
    expect(filterSessions(sessions, '')).toBe(sessions);
  });
});

describe('Older transcript pagination', () => {
  it('holds the scroll anchor as earlier rows appear above it', () => {
    expect(scrollTopAfterPrepend(320, 1800, 2400)).toBe(920);
    expect(scrollTopAfterPrepend(0, 1800, 2400)).toBe(600);
  });
  it('avoids moving backward if layout height shrinks', () => {
    expect(scrollTopAfterPrepend(140, 1500, 1200)).toBe(140);
  });
});

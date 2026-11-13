import { describe, expect, it } from 'vitest';
import { countGroupSessions, filterSessionGroups, matchedContentIds } from './session-groups';
import type { SessionGroup } from './api';

const groups: SessionGroup[] = [
  { project: 'p://one', is_default: false, sessions: [
    { id: 'a', name: 'Build desktop', recent: true },
    { id: 'b', name: null, recent: false },
  ] },
  { project: 'p://two', is_default: true, sessions: [
    { id: 'a', name: 'Other', recent: false },
  ] },
];

describe('matchedContentIds', () => {
  it('scopes ids by project', () => {
    expect(matchedContentIds([{ project: 'p://one', id: 'b', snippet: '', role: 'user', message_index: 1 }]))
      .toEqual(new Set(['p://one\u0000b']));
  });
});

describe('filterSessionGroups', () => {
  it('returns every session without a query', () => {
    expect(filterSessionGroups(groups, '', new Set())).toBe(groups);
  });

  it('matches titles, ids and content hits', () => {
    const filtered = filterSessionGroups(groups, 'build', new Set());
    expect(filtered[0].sessions.map(s => s.id)).toEqual(['a']);
    expect(filtered[1].sessions).toEqual([]);
    const byContent = filterSessionGroups(groups, 'zzz', new Set(['p://two\u0000a']));
    expect(byContent[1].sessions.map(s => s.id)).toEqual(['a']);
  });

  it('keeps empty groups so the sidebar can hide nothing else', () => {
    expect(filterSessionGroups(groups, 'nothing-here', new Set())).toHaveLength(2);
  });
});

describe('countGroupSessions', () => {
  it('sums visible sessions across groups', () => {
    expect(countGroupSessions(groups)).toBe(3);
    expect(countGroupSessions([])).toBe(0);
  });
});

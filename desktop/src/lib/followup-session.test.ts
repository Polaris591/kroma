import { describe, expect, it } from 'vitest';
import { mergeMigrated, persistQueued, queuedForSession, sessionFollowupKey } from './followup-session';

function fakeStorage() {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => { data.set(key, value); },
    removeItem: (key: string) => { data.delete(key); },
  };
}

describe('sessionFollowupKey', () => {
  it('falls back to the unknown scope for an unsent conversation', () => {
    expect(sessionFollowupKey('p', null)).toContain('unknown');
    expect(sessionFollowupKey('p', 's1')).not.toContain('unknown');
  });
});

describe('queuedForSession', () => {
  it('never restores a queue stored under the unknown placeholder', () => {
    const store = fakeStorage();
    store.setItem(sessionFollowupKey('p', 'unknown'), JSON.stringify([{id: '1', text: 'x'}]));
    expect(queuedForSession(store, 'p', 'unknown')).toEqual([]);
  });

  it('persists and restores a real conversation queue', () => {
    const store = fakeStorage();
    persistQueued(store, 'p', 's1', [{id: '1', text: 'x', paths: []}]);
    expect(queuedForSession(store, 'p', 's1')).toEqual([{id: '1', text: 'x', paths: []}]);
  });

  it('does not write for an unsent conversation', () => {
    const store = fakeStorage();
    persistQueued(store, 'p', undefined, [{id: '1', text: 'x', paths: []}]);
    expect(store.data.size).toBe(0);
  });
});

describe('mergeMigrated', () => {
  it('puts the stored queue first and caps the length', () => {
    const saved = Array.from({length: 12}, (_, i) => ({id: 's' + i, text: 't', paths: []}));
    const merged = mergeMigrated(saved, [{id: 'live', text: 't', paths: []}]);
    expect(merged).toHaveLength(12);
    expect(merged[0].id).toBe('s0');
  });
});

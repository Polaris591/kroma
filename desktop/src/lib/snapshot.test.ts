import { describe, expect, it } from 'vitest';
import { applySnapshot, isActiveTask } from './snapshot';
import type { Bootstrap, SessionSnapshot } from './api';

const snapshot = (id: string, input = 1): SessionSnapshot => ({
  session_id: id,
  messages: [{role: 'assistant', content: 'done'}],
  usage: {
    coding: {calls: 1, input, output: 1, cached: 0, reasoning: 0},
    title: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0},
    apec: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0},
    context_input: input,
    context_estimated: false,
  },
  total_messages: 1,
  context_window: 1000,
  available_input: 900,
  tool_names: [],
});

const bootstrap = (): Bootstrap => ({
  project: '/project',
  ready: true,
  running: false,
  snapshot: snapshot('old', 1),
  sessions: [{id: 'old', name: 'Old', recent: true}],
  profiles: {active: null, profiles: []},
  plugins: [],
  plugin_warnings: [],
  mcp_project: {configured: false, trusted: false},
});

describe('desktop task completion snapshots', () => {
  it('updates the transcript and usage without replacing bootstrap data', () => {
    const before = bootstrap();
    const next = applySnapshot(before, snapshot('old', 42));
    expect(next.snapshot?.usage.coding.input).toBe(42);
    expect(next.sessions).toEqual([{id: 'old', name: 'Old', recent: true}]);
    expect(next.plugins).toBe(before.plugins);
  });

  it('adds a session created by the completed task exactly once', () => {
    const first = applySnapshot(bootstrap(), snapshot('new'));
    expect(first.sessions.map(session => session.id)).toEqual(['new', 'old']);
    const second = applySnapshot(first, snapshot('new', 2));
    expect(second.sessions.map(session => session.id)).toEqual(['new', 'old']);
    expect(second.snapshot?.usage.coding.input).toBe(2);
  });

  it('rejects late events from a superseded task', () => {
    expect(isActiveTask(3, 3)).toBe(true);
    expect(isActiveTask(2, 3)).toBe(false);
  });
});

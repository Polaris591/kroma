import { describe, expect, it } from 'vitest';
import { applySnapshot, createOperationGuard, historyPageBelongsTo, isActiveTask } from './snapshot';
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
  connected_model: 'connected-model',
  snapshot: snapshot('old', 1),
  sessions: [{id: 'old', name: 'Old', recent: true}],
  profiles: {active: null, profiles: []},
  plugins: [],
  plugin_warnings: [],
  mcp_project: {configured: false, trusted: false},
});

const bootstrapFor = (project: string, sessionId: string): Bootstrap => ({
  ...bootstrap(),
  project,
  snapshot: snapshot(sessionId),
  sessions: [{id: sessionId, name: null, recent: true}],
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(r => { resolve = r; });
  return {promise, resolve};
}

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

  it('rejects a history page from another session or superseded task', () => {
    expect(historyPageBelongsTo('B', 'B', 4, 4)).toBe(true);
    expect(historyPageBelongsTo('A', 'B', 4, 4)).toBe(false);
    expect(historyPageBelongsTo('B', 'B', 3, 4)).toBe(false);
    expect(historyPageBelongsTo('B', undefined, 4, 4)).toBe(false);
  });
});

describe('desktop whole-bootstrap operations', () => {
  it('drops a refresh that resolves after a session switch', async () => {
    const guard = createOperationGuard();
    let visible = bootstrapFor('/project-A', 'session-A');

    // The refresh is issued for session A but has not answered yet.
    const refreshing = deferred<Bootstrap>();
    const refreshPending = guard.settleBootstrap(guard.begin(), refreshing.promise);

    const switched = await guard.settleBootstrap(
      guard.begin(), Promise.resolve(bootstrapFor('/project-A', 'session-B')));
    if (switched) visible = switched;

    // The stale bootstrap now answers with the old session.
    refreshing.resolve(bootstrapFor('/project-A', 'session-A'));
    const stale = await refreshPending;
    if (stale) visible = stale;

    expect(stale).toBeNull();
    expect(visible.snapshot?.session_id).toBe('session-B');
  });

  it('drops a bootstrap that resolves after a project switch', async () => {
    const guard = createOperationGuard();
    let visible = bootstrapFor('/project-A', 'session-A');

    const refreshing = deferred<Bootstrap>();
    const refreshPending = guard.settleBootstrap(guard.begin(), refreshing.promise);

    const chosen = await guard.settleBootstrap(
      guard.begin(), Promise.resolve(bootstrapFor('/project-B', 'session-A')));
    if (chosen) visible = chosen;

    refreshing.resolve(bootstrapFor('/project-A', 'session-A'));
    const stale = await refreshPending;
    if (stale) visible = stale;

    expect(stale).toBeNull();
    expect(visible.project).toBe('/project-B');
  });

  it('installs the newest bootstrap and never lets a passive reader win', async () => {
    const guard = createOperationGuard();

    // A rename is a passive reader: it must not supersede a newer switch.
    const renaming = deferred<Bootstrap>();
    const renamePending = guard.settleBootstrap(guard.current(), renaming.promise);
    const switched = await guard.settleBootstrap(
      guard.begin(), Promise.resolve(bootstrapFor('/project-B', 'session-B')));
    renaming.resolve(bootstrapFor('/project-A', 'session-A'));

    expect(switched?.snapshot?.session_id).toBe('session-B');
    expect(await renamePending).toBeNull();

    // The current operation still installs its own result.
    const current = await guard.settleBootstrap(
      guard.begin(), Promise.resolve(bootstrapFor('/project-A', 'session-A')));
    expect(current?.snapshot?.session_id).toBe('session-A');
    expect(current?.project).toBe('/project-A');
  });
});

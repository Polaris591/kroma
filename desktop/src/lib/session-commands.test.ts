import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Bootstrap } from './api';
import type { DesktopHost } from './desktop-host';
import { createOperationGuard } from './snapshot';

const apiMock = vi.hoisted(() => ({
  newSession: vi.fn(),
  bootstrap: vi.fn(),
  renameSession: vi.fn(),
  connect: vi.fn(),
  openProject: vi.fn(),
  exportSession: vi.fn(),
}));
vi.mock('./api', () => ({ api: apiMock }));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn(), save: vi.fn() }));

const { createSessionCommands } = await import('./session-commands');

function bootstrap(sessionId: string, ready = true): Bootstrap {
  return {
    project: 'D:/project', ready, running: false, connected_model: 'model',
    snapshot: {
      session_id: sessionId, messages: [], usage: {coding: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0}, title: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0}, apec: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0}, context_input: null, context_estimated: false},
      context_window: 1000, available_input: 900, tool_names: [],
    },
    sessions: [], profiles: {active: null, profiles: []}, plugins: [], plugin_warnings: [],
    mcp_project: {configured: false, trusted: false},
  };
}

interface Trace { calls: string[] }

function fakeHost(state: Record<string, unknown>, trace: Trace): DesktopHost {
  /** Spread copies values, so every accessor is declared explicitly. */
  return {
    get data() { return state.data as Bootstrap | null; },
    set data(value) { state.data = value; trace.calls.push('data'); },
    get snapshot() { return (state.data as Bootstrap | null)?.snapshot ?? null; },
    get tab() { return state.tab as 'chat'; },
    set tab(value) { state.tab = value; },
    get busy() { return state.busy as boolean; },
    set busy(value) { state.busy = value; trace.calls.push('busy=' + value); },
    get running() { return false; },
    get initialized() { return state.initialized as boolean; },
    set initialized(value) { state.initialized = value; },
    get compacting() { return state.compacting as boolean; },
    set compacting(value) { state.compacting = value; },
    get rewinding() { return state.rewinding as boolean; },
    set rewinding(value) { state.rewinding = value; },
    get rewindTarget() { return null; },
    set rewindTarget(value) { state.rewindTarget = value; },
    get connectedProfile() { return ''; },
    get reasoningLevel() { return 'auto'; },
    get sessionName() { return '会话'; },
    get taskGeneration() { return state.taskGeneration as number; },
    set taskGeneration(value) { state.taskGeneration = value; },
    get mcpDismissed() { return false; },
    set mcpDismissed(value) { state.mcpDismissed = value; },
    guard: createOperationGuard(),
    clearError() { trace.calls.push('clearError'); },
    clearNotice() {},
    setRunning() {},
    resetTaskResult() {},
    clearLiveActivities() {},
    saveDraft() { trace.calls.push('saveDraft'); },
    resetHistoryWindow() { trace.calls.push('resetHistoryWindow'); },
    collapseSidebarOnNarrow() {},
    endRename() { trace.calls.push('endRename'); },
    clearDeletedSessions() {},
    clearWorkspaceReview() {},
    expandProject() {},
    showError(error) { trace.calls.push('error:' + String(error)); },
    toast(message) { trace.calls.push('toast:' + message); },
    syncHistory() { trace.calls.push('syncHistory'); },
    restoreDraft() { trace.calls.push('restoreDraft'); },
    restoreFollowups() { trace.calls.push('restoreFollowups'); },
    refreshReview() {},
    async scrollDown() {},
    async scrollIfBottom() {},
    focusComposer() {},
    setComposerDraft() {},
  } as DesktopHost;
}

function freshHost(trace: Trace, data: Bootstrap | null = bootstrap('s1')) {
  const state: Record<string, unknown> = {data, busy: false, tab: 'chat', taskGeneration: 0, initialized: true, compacting: false, rewinding: false};
  return { state, host: fakeHost(state, trace) };
}

beforeEach(() => {
  for (const fn of Object.values(apiMock)) fn.mockReset();
});

describe('createSessionCommands', () => {
  it('refuses to start a new session while the host is busy', async () => {
    const trace: Trace = {calls: []};
    const { state, host } = freshHost(trace);
    state.busy = true;
    const commands = createSessionCommands(host);
    await commands.newSession();
    expect(apiMock.newSession).not.toHaveBeenCalled();
  });

  it('saves the draft and installs the new bootstrap', async () => {
    const trace: Trace = {calls: []};
    const { host } = freshHost(trace);
    apiMock.newSession.mockResolvedValue(bootstrap('s2'));
    const commands = createSessionCommands(host);
    await commands.newSession();
    expect(host.data?.snapshot?.session_id).toBe('s2');
    expect(trace.calls.indexOf('saveDraft')).toBeLessThan(trace.calls.indexOf('clearError'));
    expect(trace.calls).toContain('syncHistory');
    expect(trace.calls).toContain('restoreDraft');
    expect(host.busy).toBe(false);
  });

  it('keeps the previous bootstrap when the response is superseded', async () => {
    const trace: Trace = {calls: []};
    const { host } = freshHost(trace);
    let resolve: (value: Bootstrap) => void = () => {};
    apiMock.newSession.mockReturnValue(new Promise<Bootstrap>(done => { resolve = done; }));
    const commands = createSessionCommands(host);
    const pending = commands.newSession();
    // A newer operation takes the token before the old response lands.
    host.guard.begin();
    resolve(bootstrap('stale'));
    await pending;
    expect(host.data?.snapshot?.session_id).toBe('s1');
  });

  it('trims a renamed title and ignores a placeholder session', async () => {
    const trace: Trace = {calls: []};
    const { host } = freshHost(trace, bootstrap('unknown'));
    apiMock.renameSession.mockResolvedValue(bootstrap('unknown'));
    const commands = createSessionCommands(host);
    await commands.saveTitle('  新标题  ');
    expect(apiMock.renameSession).not.toHaveBeenCalled();
    host.data = bootstrap('s3');
    await commands.saveTitle('  新标题  ');
    expect(apiMock.renameSession).toHaveBeenCalledWith('s3', '新标题');
    expect(trace.calls).toContain('endRename');
  });
});

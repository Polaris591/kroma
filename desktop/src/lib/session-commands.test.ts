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
  saveProfile: vi.fn(),
  setDefaultProfile: vi.fn(),
  pluginPresentation: vi.fn(),
  pluginAction: vi.fn(),
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
  it('reports connection failure without replacing the current conversation', async () => {
    const trace: Trace = { calls: [] };
    const { host } = freshHost(trace);
    const previous = host.data;
    apiMock.connect.mockRejectedValue(new Error('offline'));
    expect(await createSessionCommands(host).doConnect('model')).toBe(false);
    expect(host.data).toBe(previous);
    expect(host.busy).toBe(false);
    expect(trace.calls).toContain('error:Error: offline');
  });

  it('reports a ready connection and refuses a busy connection', async () => {
    const trace: Trace = { calls: [] };
    const { host, state } = freshHost(trace);
    const commands = createSessionCommands(host);
    apiMock.connect.mockResolvedValue(bootstrap('s1'));
    expect(await commands.doConnect('model')).toBe(true);
    state.busy = true;
    expect(await commands.doConnect('another')).toBe(false);
    expect(apiMock.connect).toHaveBeenCalledOnce();
  });

  it('does not report an unready bootstrap as connected', async () => {
    const { host } = freshHost({ calls: [] });
    apiMock.connect.mockResolvedValue(bootstrap('s1', false));
    expect(await createSessionCommands(host).doConnect()).toBe(false);
  });

  it('reports default-profile failure without showing a success message', async () => {
    const trace: Trace = { calls: [] };
    const { host } = freshHost(trace);
    apiMock.setDefaultProfile.mockRejectedValue(new Error('read-only'));
    expect(await createSessionCommands(host).setDefaultProfile('model')).toBe(false);
    expect(trace.calls).toContain('error:Error: read-only');
    expect(trace.calls.some(call => call.startsWith('toast:'))).toBe(false);
    expect(host.busy).toBe(false);
  });

  it('updates the saved default only after success', async () => {
    const { host, state } = freshHost({ calls: [] });
    const commands = createSessionCommands(host);
    const profiles = { active: 'model', profiles: [] };
    apiMock.setDefaultProfile.mockResolvedValue(profiles);
    expect(await commands.setDefaultProfile('model')).toBe(true);
    expect(host.data?.profiles).toEqual(profiles);
    state.busy = true;
    expect(await commands.setDefaultProfile('another')).toBe(false);
    expect(apiMock.setDefaultProfile).toHaveBeenCalledOnce();
  });

  it('rejects a failed reconnect even if that profile was already connected', async () => {
    const { host } = freshHost({ calls: [] });
    host.data!.connected_profile = 'model';
    apiMock.saveProfile.mockResolvedValue(host.data!.profiles);
    apiMock.connect.mockRejectedValue(new Error('offline'));
    await expect(createSessionCommands(host).addAndConnectModel({
      name: 'model', model: 'model', base_url: 'https://example.invalid/v1', protocol: 'chat', reasoning: 'auto',
    })).rejects.toThrow('配置已保存，但连接未成功');
  });

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

  it('propagates plugin failures so a dialog cannot report false success', async () => {
    const trace: Trace = {calls: []};
    const {host} = freshHost(trace);
    const previous = host.data;
    apiMock.pluginAction.mockRejectedValue(new Error('插件仍被占用'));
    await expect(createSessionCommands(host).pluginAction('uninstall', 'demo')).rejects.toThrow('插件仍被占用');
    expect(host.data).toBe(previous);
    expect(trace.calls).toContain('error:Error: 插件仍被占用');
    expect(trace.calls.some(call => call.startsWith('toast:'))).toBe(false);
  });

  it('starts a plugin retry with fresh feedback instead of leaving the old error visible', async () => {
    const trace: Trace = {calls: []};
    const {host} = freshHost(trace);
    const commands = createSessionCommands(host);
    apiMock.pluginAction.mockRejectedValueOnce(new Error('retry me')).mockResolvedValueOnce({
      plugins: [], restart_required: false, message: 'saved',
    });
    await expect(commands.pluginAction('set', 'demo', undefined, 'key', 'value')).rejects.toThrow('retry me');
    trace.calls.length = 0;
    await commands.pluginAction('set', 'demo', undefined, 'key', 'value');
    expect(trace.calls[0]).toBe('clearError');
    expect(trace.calls).toContain('toast:saved');
    expect(trace.calls.some(call => call.startsWith('error:'))).toBe(false);
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

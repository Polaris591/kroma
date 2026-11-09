import { describe, expect, it } from 'vitest';
import { applySnapshot, applySessionSwitch, sessionPreview, carryOlderHistory, createOperationGuard, historyPageBelongsTo, isActiveTask } from './snapshot';
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

describe('fast conversation switching', () => {
  it('updates the selected session without refetching or replacing stable settings', () => {
    const base = bootstrap();
    const old = {id:'old',name:'Previously Open',recent:true};
    const next = {id:'next',name:'Next Conversation',recent:false};
    const other = {project:'/other',is_default:false,sessions:[{id:'external',name:'External',recent:true}]};
    const data: Bootstrap = {
      ...base,
      sessions:[old,next],
      session_groups:[
        {project:'/project',is_default:false,sessions:[old,next]},other
      ],
    };
    const switched = applySessionSwitch(data, snapshot('next', 12));
    expect(switched.snapshot?.session_id).toBe('next');
    expect(switched.sessions.find(s=>s.id==='old')?.recent).toBe(false);
    expect(switched.sessions.find(s=>s.id==='next')?.recent).toBe(true);
    expect(switched.sessions.find(s=>s.id==='next')?.name).toBe('Next Conversation');
    expect(switched.session_groups?.[0].sessions.find(s=>s.id==='next')?.recent).toBe(true);
    expect(switched.session_groups?.[0].sessions.find(s=>s.id==='old')?.recent).toBe(false);
    expect(switched.session_groups?.[1]).toBe(other);
    expect(switched.profiles).toBe(base.profiles);
    expect(switched.plugins).toBe(base.plugins);
    expect(data.sessions[0].recent).toBe(true);
  });

  it('backfills only unnamed legacy chats, leaving user-defined names intact', () => {
    const base: Bootstrap = {
      ...bootstrap(),
      sessions:[
        {id:'old',name:null,recent:true},
        {id:'manual',name:'Pinned by user',recent:false},
      ],
      session_groups:[{
        project:'/project',is_default:false,
        sessions:[
          {id:'old',name:null,recent:true},
          {id:'manual',name:'Pinned by user',recent:false},
        ],
      }],
    };
    const upgraded = applySessionSwitch(base,snapshot('old'), '修复流式 Markdown');
    expect(upgraded.sessions[0].name).toBe('修复流式 Markdown');
    expect(upgraded.session_groups?.[0].sessions[0].name).toBe('修复流式 Markdown');
    expect(base.sessions[0].name).toBeNull();
    const manual = applySessionSwitch(upgraded,snapshot('manual'),'另一个自动标题');
    expect(manual.sessions[1].name).toBe('Pinned by user');
  });

  it('limits preview data, maintains an exact page cursor and never mutates the original', () => {
    const messages = Array.from({length:180},(_,i)=>({role:'assistant',content:String(i)}));
    const full: SessionSnapshot = {...snapshot('old'),messages,total_messages:360};
    const preview = sessionPreview(full,48);
    expect(preview.messages).toHaveLength(48);
    expect(preview.messages[0].content).toBe('132');
    expect(preview.total_messages).toBe(360);
    expect(preview.next_before).toBe(312);
    expect(preview.history_truncated).toBe(true);
    expect(full.messages).toHaveLength(180);
    expect(sessionPreview({...snapshot('small'),messages:[],total_messages:0}).history_truncated).toBe(false);
  });
});

describe('bounded transcript history windows', () => {
  const page = (id: string, total: number, content: string[]): SessionSnapshot => ({
    ...snapshot(id),
    total_messages: total,
    messages: content.map(text => ({role:'assistant',content:text})),
  });
  it('fills the shifted window gap without repeating the overlap', () => {
    const previous = page('A', 8, ['4','5','6','7']);
    const next = page('A', 10, ['6','7','8','9']);
    const older = [{role:'assistant',content:'2'}, {role:'assistant',content:'3'}];
    expect(carryOlderHistory(older, previous, next).map(x => x.content)).toEqual(['2','3','4','5']);
  });
  it('does not carry messages from another session or a discontinuous snapshot', () => {
    const previous = page('A', 8, ['4','5','6','7']);
    expect(carryOlderHistory([],previous,page('B',10,['6','7','8','9']))).toEqual([]);
    expect(carryOlderHistory([],previous,page('A',20,['18','19']))).toEqual([]);
  });
  it('never creates duplicate rows when recent history already covers the beginning', () => {
    const previous = page('A',4,['0','1','2','3']);
    const next = page('A',5,['1','2','3','4']);
    expect(carryOlderHistory([],previous,next).map(x => x.content)).toEqual(['0']);
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

import { describe, expect, it } from 'vitest';
import { createLatestNavigation, filterSessions, isWorkspaceExpanded, workspaceDisplayName, scrollTopAfterPrepend, sessionLabel, sessionActivityLabel, visibleSessionsForGroup } from './chat-ui';

const sessions = [
  {id: 'first', name: 'Rust Refactor', recent: true},
  {id: 'desktop-2', name: 'UI work', recent: false},
  {id: 'third', name: null, recent: false},
];

describe('Serialized last-click-wins session navigation', () => {
  it('coalesces A -> B -> C while a session load is pending', async () => {
    const visited: string[] = [];
    const errors: unknown[] = [];
    let releaseA!: () => void;
    let finish!: () => void;
    const blockedA = new Promise<void>(resolve => { releaseA = resolve; });
    const finished = new Promise<void>(resolve => { finish = resolve; });
    const nav = createLatestNavigation(
      async (id: string) => {
        visited.push(id);
        if (id === 'A') await blockedA;
      },
      () => finish(),
      error => errors.push(error),
    );
    nav.request('A');
    nav.request('B');
    nav.request('C');
    expect(nav.isActive).toBe(true);
    expect(nav.hasPending).toBe(true);
    expect(visited).toEqual(['A']);
    releaseA();
    await finished;
    expect(visited).toEqual(['A', 'C']);
    expect(nav.isActive).toBe(false);
    expect(nav.hasPending).toBe(false);
    expect(errors).toEqual([]);
  });

  it('does not drop a later click after a failed attempt', async () => {
    const visited: string[] = [];
    const errors: unknown[] = [];
    let releaseA!: () => void;
    let finish!: () => void;
    const blockedA = new Promise<void>(resolve => { releaseA = resolve; });
    const finished = new Promise<void>(resolve => { finish = resolve; });
    const nav = createLatestNavigation(
      async (id: string) => {
        visited.push(id);
        if (id === 'A') {
          await blockedA;
          throw new Error('Session A is locked');
        }
      },
      () => finish(),
      error => errors.push(error),
    );
    nav.request('A');
    nav.request('C');
    releaseA();
    await finished;
    expect(visited).toEqual(['A', 'C']);
    expect(errors).toHaveLength(1);
    expect((errors[0] as Error).message).toContain('locked');
  });
});

describe('Desktop session search', () => {
  it('matches names and IDs without changing ordering', () => {
    expect(filterSessions(sessions, ' rUsT ')).toEqual([sessions[0]]);
    expect(filterSessions(sessions, 'DESKTOP')).toEqual([sessions[1]]);
    expect(filterSessions(sessions, 'none')).toEqual([]);
    expect(filterSessions(sessions, '')).toBe(sessions);
  });
});

describe('Compact conversation navigation', () => {
  it('keeps an older selected chat visible beyond the first page', () => {
    const many = Array.from({length:35}, (_,i) => ({id:'s'+i,name:'会话 '+i,recent:i===0}));
    const visible = visibleSessionsForGroup(many, '', 7, 's31');
    expect(visible).toHaveLength(8);
    expect(visible[0].id).toBe('s31');
    expect(visible[1].id).toBe('s0');
    expect(visible[7].id).toBe('s6');
    expect(new Set(visible.map(s => s.id)).size).toBe(8);
  });

  it('does not filter search results or duplicate an already visible selection', () => {
    expect(visibleSessionsForGroup(sessions,'ui',1,'desktop-2')).toEqual([sessions[1]]);
    expect(visibleSessionsForGroup(sessions,'',2,'desktop-2')).toEqual(sessions.slice(0,2));
  });
});

describe('Workspace display labels', () => {
  it('aliases only the real user home and preserves other folder names', () => {
    const home = 'C:\\Users\\24524';
    expect(workspaceDisplayName(home,home)).toBe('用户主目录');
    expect(workspaceDisplayName('c:/users/24524/',home)).toBe('用户主目录');
    expect(workspaceDisplayName('C:\\Users\\SomeoneElse',home)).toBe('SomeoneElse');
    expect(workspaceDisplayName('D:\\My Games',home)).toBe('My Games');
    expect(workspaceDisplayName('/home/alice/repos/Kroma','/home/alice')).toBe('Kroma');
    expect(workspaceDisplayName(home,null)).toBe('24524');
  });
});

describe('Workspace folder collapse', () => {
  it('allows the current workspace to collapse even when automatically expanded', () => {
    const project = 'D:/Kroma';
    expect(isWorkspaceExpanded({}, project, project)).toBe(true);
    expect(isWorkspaceExpanded({[project]:false}, project, project)).toBe(false);
    expect(isWorkspaceExpanded({[project]:true}, project, '/other')).toBe(true);
  });

  it('allows manually collapsing a search-expanded project', () => {
    const project = 'D:/My Games';
    expect(isWorkspaceExpanded({}, project, '/other', null, 'test')).toBe(true);
    expect(isWorkspaceExpanded({[project]:false}, project, '/other', null, 'test')).toBe(false);
    expect(isWorkspaceExpanded({[project]:true}, project, '/other')).toBe(true);
    expect(isWorkspaceExpanded({}, project, '/other')).toBe(false);
  });
});

describe('Session navigation labels', () => {
  it('shows meaningful titles and avoids exposing raw IDs as labels', () => {
    expect(sessionLabel(sessions[0])).toBe('Rust Refactor');
    expect(sessionLabel(sessions[2])).toBe('新对话');
    expect(sessionLabel({id:'default',name:null,recent:false})).toBe('默认会话');
    expect(sessionLabel({id:'s18e5bc4a7b9',name:null,recent:false})).toBe('新对话');
  });
});

describe('Legacy session activity labels', () => {
  it('provides subtle time cues without exposing internal session IDs', () => {
    const today = new Date(2026, 10, 7, 16, 0);
    const earlier = new Date(2026, 10, 7, 13, 9);
    const yesterday = new Date(2026, 10, 6, 13, 9);
    const threeDaysAgo = new Date(2026, 10, 4, 10, 0);
    const lastMonth = new Date(2026, 8, 1, 10, 0);
    const lastYear = new Date(2025, 5, 3, 10, 0);
    expect(sessionActivityLabel(earlier.getTime()/1000, today)).toBe('13:09');
    expect(sessionActivityLabel(yesterday.getTime()/1000, today)).toBe('昨天');
    expect(sessionActivityLabel(threeDaysAgo.getTime()/1000, today)).toBe('周三');
    expect(sessionActivityLabel(lastMonth.getTime()/1000, today)).toBe('9月1日');
    expect(sessionActivityLabel(lastYear.getTime()/1000, today)).toBe('2025/6/3');
    expect(sessionActivityLabel(null, today)).toBe('');
    expect(sessionActivityLabel(Number.NaN, today)).toBe('');
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

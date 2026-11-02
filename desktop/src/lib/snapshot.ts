import type { Bootstrap, SessionSnapshot } from './api';

export interface OperationGuard {
  /** Start a new operation and return its token; every older token becomes stale. */
  begin(): number;
  /** Token of the newest operation, for readers that must not supersede it. */
  current(): number;
  /** True while `candidate` is still the newest operation. */
  isCurrent(candidate: number): boolean;
  /**
   * Await a request that would replace the whole Bootstrap and return its value
   * only while `candidate` is still newest, otherwise `null`. Rejections from a
   * superseded request still propagate to the caller's `catch`.
   */
  settleBootstrap(candidate: number, request: Promise<Bootstrap>): Promise<Bootstrap | null>;
}

/**
 * Guards async operations that install a whole Bootstrap. A newer operation
 * (refresh, connect, project or session switch) supersedes every older one, so
 * a late response must be dropped instead of overwriting the state the user is
 * now looking at.
 */
export function createOperationGuard(): OperationGuard {
  let generation = 0;
  return {
    begin: () => ++generation,
    current: () => generation,
    isCurrent: candidate => candidate === generation,
    settleBootstrap: async (candidate, request) => {
      const next = await request;
      return candidate === generation ? next : null;
    },
  };
}

/** Apply the one durable snapshot emitted at a task boundary. */
export function applySnapshot(data: Bootstrap, snapshot: SessionSnapshot): Bootstrap {
  const known = data.sessions.some(session => session.id === snapshot.session_id);
  const sessions = known
    ? data.sessions.map(session => session.id === snapshot.session_id
      ? {...session, recent: true} : session)
    : snapshot.session_id && snapshot.session_id !== 'unknown'
      ? [{id: snapshot.session_id, name: null, recent: true}, ...data.sessions]
      : data.sessions;
  return {...data, snapshot, sessions};
}

/** Ignore late events from a task that no longer owns the visible session. */
export function isActiveTask(generation: number, current: number): boolean {
  return generation === current;
}

/**
 * A history page is safe to splice only when both the task generation and the
 * visible session still match the request. Otherwise a slow response can put
 * another conversation's older messages into the transcript.
 */
export function historyPageBelongsTo(
  pageSession: string,
  visibleSession: string | undefined,
  pageGeneration: number,
  currentGeneration: number,
): boolean {
  return pageGeneration === currentGeneration && pageSession === visibleSession;
}

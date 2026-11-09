import type { Bootstrap, SessionSnapshot, Message } from './api';

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

/** Switch a conversation without throwing away stable model, plugin and
 * project metadata. Navigation recency updates immediately in both the flat
 * session list and the grouped sidebar; the host will refresh it on the next
 * full bootstrap or task completion. */
export function applySessionSwitch(data: Bootstrap, snapshot: SessionSnapshot, newTitle?: string | null): Bootstrap {
  const next = applySnapshot(data, snapshot);
  const selected = snapshot.session_id;
  return {
    ...next,
    sessions: next.sessions.map(session => ({
      ...session,
      name: session.id === selected && !session.name && newTitle ? newTitle : session.name,
      recent: session.id === selected,
    })),
    session_groups: next.session_groups?.map(group =>
      group.project === next.project
        ? {...group, sessions: group.sessions.map(session => ({
            ...session,
            name: session.id === selected && !session.name && newTitle ? newTitle : session.name,
            recent: session.id === selected,
          }))}
        : group
    ),
  };
}

/** Cache a small, bounded, read-only preview for instant repeat navigation.
 * The actual Agent session remains authoritative; the preview must never be
 * used to execute tools or to overwrite the durable history. */
export function sessionPreview(snapshot: SessionSnapshot, limit = 64): SessionSnapshot {
  const count = Math.max(1, Math.min(120, Math.floor(limit)));
  const messages = snapshot.messages.slice(-count);
  const total = snapshot.total_messages ?? snapshot.messages.length;
  const before = Math.max(0, total - messages.length);
  return {
    ...snapshot,
    messages,
    total_messages: total,
    history_truncated: before > 0,
    next_before: before > 0 ? before : null,
  };
}

/** Preserve already-loaded older messages when the bounded recent snapshot
 * advances. History pages use absolute indices; joining by array concatenation
 * alone would silently omit messages between the old and new window starts.
 * Never carry pages across sessions or an unexplained history discontinuity.
 */
export function carryOlderHistory(
  previousOlder: Message[],
  previous: SessionSnapshot,
  next: SessionSnapshot,
  maxRetained = 3000,
): Message[] {
  if (previous.session_id !== next.session_id) return [];
  const oldStart = Math.max(0, (previous.total_messages ?? previous.messages.length) - previous.messages.length);
  const newStart = Math.max(0, (next.total_messages ?? next.messages.length) - next.messages.length);
  const firstIndex = Math.max(0, oldStart - previousOlder.length);
  const prefixLength = newStart - firstIndex;
  if (prefixLength <= 0) return [];
  const contiguous = [...previousOlder, ...previous.messages];
  if (prefixLength > contiguous.length) return [];
  return contiguous.slice(Math.max(0, prefixLength - maxRetained), prefixLength);
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

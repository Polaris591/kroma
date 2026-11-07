import type { SessionEntry } from './api';

/** Filter only visible session metadata; no transcript content is indexed. */
export function filterSessions(sessions: SessionEntry[], query: string): SessionEntry[] {
  const normalized = query.trim().toLocaleLowerCase();
  if (!normalized) return sessions;
  return sessions.filter(session =>
    (session.name || '').toLocaleLowerCase().includes(normalized)
    || session.id.toLocaleLowerCase().includes(normalized)
  );
}

/** Keep the same line in view after older history is prepended above it. */
export function scrollTopAfterPrepend(top: number, previousHeight: number, nextHeight: number): number {
  return Math.max(0, top + Math.max(0, nextHeight - previousHeight));
}

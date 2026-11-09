import type { SessionGroup, SessionSearchHit } from './api';
import { filterSessions } from './chat-ui';

/** Session ids whose transcript content matched a local search, scoped by
 * project so the same id in two workspaces never collides. */
export function matchedContentIds(hits: SessionSearchHit[]): Set<string> {
  return new Set(hits.map(hit => hit.project + '\u0000' + hit.id));
}

/** Keep only sessions matching the title/id query or a content hit. Without a
 * query every session stays visible. */
export function filterSessionGroups(
  groups: SessionGroup[], query: string, matched: ReadonlySet<string>,
): SessionGroup[] {
  if (!query.trim()) return groups;
  return groups.map(group => ({
    ...group,
    sessions: group.sessions.filter(session =>
      filterSessions([session], query).length > 0
      || matched.has(group.project + '\u0000' + session.id)),
  }));
}

export function countGroupSessions(groups: SessionGroup[]): number {
  return groups.reduce((count, group) => count + group.sessions.length, 0);
}

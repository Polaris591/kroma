import {
  followupKey, hasPersistedSessionId, loadFollowups, saveFollowups,
  MAX_FOLLOWUPS, type FollowupTask, type QueueStorage,
} from './followups';

/** Follow-ups belong to one project + conversation, not to the whole app. */
export function sessionFollowupKey(project: string, sessionId: string | null | undefined): string {
  return followupKey(project, sessionId || 'unknown');
}

/** 'unknown' is shared by every unsent conversation, so it never restores. */
export function queuedForSession(
  store: QueueStorage | null, project: string, sessionId: string | null | undefined,
): FollowupTask[] {
  if (!hasPersistedSessionId(sessionId)) return [];
  return loadFollowups(store, sessionFollowupKey(project, sessionId));
}

export function persistQueued(
  store: QueueStorage | null, project: string, sessionId: string | null | undefined,
  tasks: FollowupTask[],
): void {
  if (!hasPersistedSessionId(sessionId)) return;
  saveFollowups(store, sessionFollowupKey(project, sessionId), tasks);
}

/** A new conversation receives its real session id during its first run; a
 * queue already stored under that id must survive ahead of the live one. */
export function mergeMigrated(
  saved: readonly FollowupTask[], current: readonly FollowupTask[],
): FollowupTask[] {
  return [...saved, ...current].slice(0, MAX_FOLLOWUPS);
}

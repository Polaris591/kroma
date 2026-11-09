/** Lightweight, per-session Desktop follow-up tasks.
 *
 * These are independent user turns sent through the existing Agent after the
 * current turn finishes. Never auto-replay them on application startup:
 * interrupted work must be resumed deliberately to avoid duplicate edits.
 */
export interface FollowupTask {
  id: string;
  text: string;
}

export const MAX_FOLLOWUPS = 12;
export const MAX_FOLLOWUP_LENGTH = 12000;
const PREFIX = 'kroma.desktop.followups.v1:';

export interface QueueStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export function hasPersistedSessionId(sessionId: string | null | undefined): boolean {
  return !!sessionId && sessionId !== 'unknown';
}

export function followupKey(project: string, sessionId: string): string {
  const slash = project.replace(/\\/g, '/');
  // Windows volumes and UNC paths are case-insensitive; POSIX paths aren't.
  const normalized = /^[a-z]:\//i.test(slash) || slash.startsWith('//')
    ? slash.toLowerCase() : slash;
  return PREFIX + encodeURIComponent(normalized) +
    ':' + encodeURIComponent(sessionId || 'unknown');
}

export function parseFollowups(raw: string | null | undefined): FollowupTask[] {
  if (!raw || raw.length > 200000) return [];
  try {
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value)) return [];
    return value.slice(0, MAX_FOLLOWUPS).flatMap((item: unknown) => {
      if (!item || typeof item !== 'object') return [];
      const record = item as Record<string, unknown>;
      if (typeof record.id !== 'string' || typeof record.text !== 'string' ||
          !record.id || !record.text.trim()) return [];
      return [{
        id: record.id.slice(0, 80),
        text: record.text.slice(0, MAX_FOLLOWUP_LENGTH),
      }];
    });
  } catch {
    return [];
  }
}

export function loadFollowups(store: QueueStorage | null, key: string): FollowupTask[] {
  if (!store) return [];
  try { return parseFollowups(store.getItem(key)); }
  catch { return []; }
}

export function saveFollowups(store: QueueStorage | null, key: string, tasks: FollowupTask[]) {
  if (!store) return;
  try {
    if (tasks.length) store.setItem(key, JSON.stringify(tasks));
    else store.removeItem(key);
  } catch {
    // An inaccessible or full WebView local storage must not break the UI.
  }
}

export function enqueueFollowup(
  tasks: readonly FollowupTask[], text: string, id: string,
): FollowupTask[] | null {
  if (tasks.length >= MAX_FOLLOWUPS || !text.trim() || text.length > MAX_FOLLOWUP_LENGTH) {
    return null;
  }
  return [...tasks, {id, text: text.trim()}];
}

export function shouldStartNextFollowup(
  status: 'completed' | 'cancelled' | 'failed',
  paused: boolean,
  remaining: number,
): boolean {
  return status === 'completed' && !paused && remaining > 0;
}

export function moveFollowup(
  tasks: readonly FollowupTask[], id: string, direction: -1 | 1,
): FollowupTask[] {
  const index = tasks.findIndex(task => task.id === id);
  const target = index + direction;
  if (index < 0 || target < 0 || target >= tasks.length) return [...tasks];
  const result = [...tasks];
  [result[index], result[target]] = [result[target], result[index]];
  return result;
}

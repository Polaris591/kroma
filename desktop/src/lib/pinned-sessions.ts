import type { SessionEntry } from './api';

/** Desktop-local pin preference: which conversations stay at the top of their
 * own workspace. This is a local UI preference only; it is never written to a
 * session, the WAL or anything the CLI shares.
 */
export interface PinStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

const PREFIX = 'kroma.desktop.pinned.v1:';
const MAX_PINS = 500;

/** Windows volumes and UNC paths are case-insensitive; POSIX paths are not.
 * Each workspace owns its storage key, so the same session id in two projects
 * never collides (the same scoping sessionSelectionKey gives the UI). */
function normalizedProject(project: string): string {
  const slash = project.replace(/\\/g, '/');
  const windows = /^[a-z]:\//i.test(slash) || slash.startsWith('//');
  return windows ? slash.toLowerCase() : slash;
}

export function pinnedStorageKey(project: string): string {
  return PREFIX + encodeURIComponent(normalizedProject(project));
}

/** Unknown or malformed entries are dropped, never repaired into another id. */
export function parsePinnedIds(raw: string | null | undefined): string[] {
  if (!raw || raw.length > 200000) return [];
  try {
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value)) return [];
    const seen = new Set<string>();
    for (const item of value) {
      if (typeof item !== 'string') continue;
      const id = item.trim();
      if (!id || id.length > 256 || seen.has(id)) continue;
      seen.add(id);
      if (seen.size >= MAX_PINS) break;
    }
    return [...seen];
  } catch {
    return [];
  }
}

/** An unavailable or corrupt store simply reads as "nothing pinned". */
export function loadPinnedIds(store: PinStorage | null, project: string): string[] {
  if (!store) return [];
  try { return parsePinnedIds(store.getItem(pinnedStorageKey(project))); }
  catch { return []; }
}

/** Returns false when the preference could not be persisted, so the caller can
 * say the pin is only in effect for this window instead of claiming a save. */
export function savePinnedIds(
  store: PinStorage | null, project: string, ids: readonly string[],
): boolean {
  if (!store) return false;
  try {
    if (ids.length) store.setItem(pinnedStorageKey(project), JSON.stringify(ids));
    else store.removeItem(pinnedStorageKey(project));
    return true;
  } catch {
    return false;
  }
}

export function togglePinnedId(ids: readonly string[], id: string): string[] {
  return ids.includes(id) ? ids.filter(item => item !== id) : [...ids, id];
}

/** Pinned conversations lead their own workspace group and stay visible even
 * when the group is folded to its recent `limit`. Pins and the rest each keep
 * the server's recent order. A search only reorders the matches it already
 * has: it never reveals a pinned conversation that does not match. */
export function arrangePinnedSessions(
  sessions: readonly SessionEntry[],
  pinnedIds: readonly string[],
  limit: number,
  selectedId: string | undefined,
  filtered: boolean,
): SessionEntry[] {
  const pinned = new Set(pinnedIds);
  const leading = sessions.filter(session => pinned.has(session.id));
  const rest = sessions.filter(session => !pinned.has(session.id));
  if (filtered) return [...leading, ...rest];
  const visible = rest.slice(0, Math.max(0, limit));
  // The active conversation keeps its existing visibility rule, but it can
  // never be placed above a pinned one.
  if (selectedId && !pinned.has(selectedId) && !visible.some(session => session.id === selectedId)) {
    const active = rest.find(session => session.id === selectedId);
    if (active) visible.unshift(active);
  }
  return [...leading, ...visible];
}

/** Per-session draft storage: the exact text plus the file references the
 * person attached. Nothing here stores bytes; an attachment is only its path,
 * and the backend verifies the file again after a restart.
 *
 * The stored form is versioned. Older builds saved a bare string under the
 * `v1` key; it is still read once and replaced on the next save.
 */
const PREFIX = 'kroma.desktop.draft.v2:';
const LEGACY_PREFIX = 'kroma.desktop.draft.v1:';
export const MAX_DRAFT_LENGTH = 20000;
export const MAX_DRAFT_PATHS = 8;
const MAX_PATH_LENGTH = 4096;

export function draftStorageKey(project: string, sessionId: string): string {
  return PREFIX + encodeURIComponent(project) + ':' + encodeURIComponent(sessionId);
}

function legacyDraftStorageKey(key: string): string {
  return key.replace(PREFIX, LEGACY_PREFIX);
}

export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** A restored draft. `truncated` is true only when older storage held more
 * text than the bound, so the composer can say so instead of pretending the
 * whole draft came back. */
export interface StoredDraft {
  text: string;
  paths: string[];
  truncated: boolean;
}

/** What a save actually did. A refused or shortened write must be visible in
 * the interface, never silently swallowed. */
export type DraftWrite = 'saved' | 'truncated' | 'unavailable';

/** Compare file references without changing the spelling used for IPC. */
export function draftPathKey(path: string): string {
  const slash = path.replace(/\\/g, '/');
  // Windows drive letters and UNC roots are case-insensitive.
  return /^[a-z]:\//i.test(slash) || slash.startsWith('//') ? slash.toLowerCase() : slash;
}

export function cleanDraftPaths(paths: readonly string[]): string[] {
  const seen = new Set<string>();
  const cleaned: string[] = [];
  for (const path of paths) {
    if (typeof path !== 'string' || !path || path.length > MAX_PATH_LENGTH) continue;
    const key = draftPathKey(path);
    if (seen.has(key)) continue;
    seen.add(key);
    cleaned.push(path);
    if (cleaned.length >= MAX_DRAFT_PATHS) break;
  }
  return cleaned;
}

function parseStoredDraft(raw: string | null): StoredDraft | null {
  if (!raw) return null;
  try {
    const value: unknown = JSON.parse(raw);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
    const record = value as Record<string, unknown>;
    if (typeof record.text !== 'string' || !Array.isArray(record.paths)) return null;
    const text = record.text;
    return {
      text: text.slice(0, MAX_DRAFT_LENGTH),
      paths: cleanDraftPaths(record.paths.filter((path): path is string => typeof path === 'string')),
      truncated: text.length > MAX_DRAFT_LENGTH,
    };
  } catch {
    return null;
  }
}

export function readStoredDraft(store: DraftStorage | null, key: string): StoredDraft {
  if (!store) return {text: '', paths: [], truncated: false};
  try {
    const current = parseStoredDraft(store.getItem(key));
    if (current) return current;
    // One-time compatibility: a real conversation may still only have the
    // plain-text draft of the previous version.
    const legacy = store.getItem(legacyDraftStorageKey(key)) || '';
    return {
      text: legacy.slice(0, MAX_DRAFT_LENGTH),
      paths: [],
      truncated: legacy.length > MAX_DRAFT_LENGTH,
    };
  } catch {
    return {text: '', paths: [], truncated: false};
  }
}

export function persistDraft(
  store: DraftStorage | null,
  key: string,
  draft: {text: string; paths: readonly string[]},
): DraftWrite {
  if (!store) return 'unavailable';
  const paths = cleanDraftPaths(draft.paths);
  const truncated = draft.text.length > MAX_DRAFT_LENGTH;
  const text = truncated ? draft.text.slice(0, MAX_DRAFT_LENGTH) : draft.text;
  try {
    if (!text.trim() && !paths.length) {
      store.removeItem(key);
      store.removeItem(legacyDraftStorageKey(key));
      return 'saved';
    }
    store.setItem(key, JSON.stringify({text, paths}));
    // The old plain-text entry may never shadow the versioned draft again.
    store.removeItem(legacyDraftStorageKey(key));
    return truncated ? 'truncated' : 'saved';
  } catch {
    return 'unavailable';
  }
}

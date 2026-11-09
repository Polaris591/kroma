/** Tiny per-session text draft storage. Nothing is sent to the model or a
 * server. File attachments intentionally remain session-local in memory,
 * since absolute file paths can be stale after a restart. */
const PREFIX = 'kroma.desktop.draft.v1:';
const MAX_DRAFT_LENGTH = 20000;

export function draftStorageKey(project: string, sessionId: string): string {
  return PREFIX + encodeURIComponent(project) + ':' + encodeURIComponent(sessionId);
}

export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export function readStoredDraft(store: DraftStorage | null, key: string): string {
  if (!store) return '';
  try {
    return (store.getItem(key) || '').slice(0, MAX_DRAFT_LENGTH);
  } catch {
    return '';
  }
}

export function persistDraftText(store: DraftStorage | null, key: string, text: string): void {
  if (!store) return;
  try {
    if (text.trim()) store.setItem(key, text.slice(0, MAX_DRAFT_LENGTH));
    else store.removeItem(key);
  } catch {
    // Disabled or full WebView storage never prevents typing or navigating.
  }
}

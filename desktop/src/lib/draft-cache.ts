import { persistDraftText, readStoredDraft, type DraftStorage } from './drafts';

export interface CachedDraft {
  text: string;
  paths: string[];
}

/** In-memory drafts are keyed by the conversation the text was typed in, so a
 * switch away and back restores exactly what the user left behind. */
export function memoryDraftKey(project: string, sessionId: string | null | undefined): string {
  return `${project || ''}::${sessionId || 'new'}`;
}

export class DraftCache {
  private entries = new Map<string, CachedDraft>();

  get size(): number {
    return this.entries.size;
  }

  remember(project: string, sessionId: string | null | undefined, draft: CachedDraft): void {
    const key = memoryDraftKey(project, sessionId);
    if (draft.text.trim() || draft.paths.length) this.entries.set(key, draft);
    else this.entries.delete(key);
  }

  recall(project: string, sessionId: string | null | undefined): CachedDraft | undefined {
    return this.entries.get(memoryDraftKey(project, sessionId));
  }

  forget(project: string, sessionId: string | null | undefined): void {
    this.entries.delete(memoryDraftKey(project, sessionId));
  }
}

/** Disabled or sandboxed WebView storage must never break typing or navigation. */
export function browserDraftStorage(): DraftStorage | null {
  try { return window.localStorage; }
  catch { return null; }
}

export { persistDraftText, readStoredDraft };

/** One shared cache for the running app: the shell and the composer must see
 * the same drafts. Tests construct their own DraftCache instead. */
export const appDrafts = new DraftCache();

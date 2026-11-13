import {describe, expect, it} from 'vitest';
import {
  draftPathKey, draftStorageKey, MAX_DRAFT_LENGTH, persistDraft, readStoredDraft,
} from './drafts';

function memoryStorage() {
  const items = new Map<string,string>();
  return {
    items,
    getItem: (key:string) => items.get(key) ?? null,
    setItem: (key:string,value:string) => {items.set(key,value);},
    removeItem: (key:string) => {items.delete(key);},
  };
}

describe('session-local drafts', () => {
  it('separates projects and sessions and round-trips text with attachments', () => {
    const storage = memoryStorage();
    const a = draftStorageKey('D:/Kroma','session-1');
    const b = draftStorageKey('D:/Kroma','session-2');
    const c = draftStorageKey('D:/My Games','session-1');
    expect(new Set([a,b,c]).size).toBe(3);
    persistDraft(storage, a, {text:'修复这个问题', paths:['C:\\tmp\\shot.png']});
    expect(readStoredDraft(storage, a)).toEqual({
      text:'修复这个问题', paths:['C:\\tmp\\shot.png'], truncated:false,
    });
    expect(readStoredDraft(storage, b)).toEqual({text:'', paths:[], truncated:false});
    persistDraft(storage, a, {text:'', paths:[]});
    expect(readStoredDraft(storage, a)).toEqual({text:'', paths:[], truncated:false});
  });

  it('reads a draft written by the previous plain-text version', () => {
    const storage = memoryStorage();
    const key = draftStorageKey('/project','session');
    storage.setItem(key.replace('.v2:', '.v1:'), '旧草稿');
    expect(readStoredDraft(storage, key)).toEqual({text:'旧草稿', paths:[], truncated:false});
    // Saving replaces the legacy entry instead of keeping two sources of truth.
    persistDraft(storage, key, {text:'新草稿', paths:[]});
    expect([...storage.items.keys()].some(entry => entry.includes('.v1:'))).toBe(false);
    expect(readStoredDraft(storage, key).text).toBe('新草稿');
  });

  it('reports a shortened draft instead of silently losing long text', () => {
    const storage = memoryStorage();
    const key = draftStorageKey('/project','session');
    expect(persistDraft(storage, key, {text:'a'.repeat(MAX_DRAFT_LENGTH + 20), paths:[]}))
      .toBe('truncated');
    expect(readStoredDraft(storage, key).text).toHaveLength(MAX_DRAFT_LENGTH);
  });

  it('deduplicates paths by Windows/POSIX convention and keeps at most eight', () => {
    const storage = memoryStorage();
    const key = draftStorageKey('/project','session');
    expect(draftPathKey('D:\\Kroma\\a.png')).toBe(draftPathKey('d:/kroma/a.png'));
    expect(draftPathKey('/A/B')).not.toBe(draftPathKey('/a/b'));
    const paths = Array.from({length:12}, (_,i) => `/tmp/${i}.bin`);
    persistDraft(storage, key, {text:'x', paths:[...paths, '/tmp/0.bin']});
    expect(readStoredDraft(storage, key).paths).toHaveLength(8);
  });

  it('never throws on unavailable or full local storage', () => {
    const unavailable = {
      getItem: (_key:string):string|null => {throw new Error('blocked');},
      setItem: (_key:string,_value:string):void => {throw new Error('full');},
      removeItem: (_key:string):void => {throw new Error('blocked');},
    };
    const key = draftStorageKey('/project','session');
    expect(readStoredDraft(unavailable, key)).toEqual({text:'', paths:[], truncated:false});
    expect(persistDraft(unavailable, key, {text:'hello', paths:[]})).toBe('unavailable');
    expect(readStoredDraft(null, key)).toEqual({text:'', paths:[], truncated:false});
  });

  it('ignores a corrupt versioned entry instead of losing the input path', () => {
    const storage = memoryStorage();
    const key = draftStorageKey('/project','session');
    storage.setItem(key, '{not json');
    expect(readStoredDraft(storage, key)).toEqual({text:'', paths:[], truncated:false});
    storage.setItem(key, JSON.stringify({text:'ok', paths:['/tmp/a', 5, null]}));
    expect(readStoredDraft(storage, key)).toEqual({text:'ok', paths:['/tmp/a'], truncated:false});
  });
});

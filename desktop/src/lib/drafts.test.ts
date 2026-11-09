import {describe,expect,it} from 'vitest';
import {draftStorageKey,persistDraftText,readStoredDraft} from './drafts';

function memoryStorage() {
  const items = new Map<string,string>();
  return {
    getItem: (key:string) => items.get(key) ?? null,
    setItem: (key:string,value:string) => {items.set(key,value);},
    removeItem: (key:string) => {items.delete(key);},
  };
}

describe('session-local text drafts', () => {
  it('separates projects and sessions without requiring a database', () => {
    const storage = memoryStorage();
    const a = draftStorageKey('D:/Kroma','session-1');
    const b = draftStorageKey('D:/Kroma','session-2');
    const c = draftStorageKey('D:/My Games','session-1');
    expect(new Set([a,b,c]).size).toBe(3);
    persistDraftText(storage,a,'修复这个问题');
    expect(readStoredDraft(storage,a)).toBe('修复这个问题');
    expect(readStoredDraft(storage,b)).toBe('');
    persistDraftText(storage,a,'');
    expect(readStoredDraft(storage,a)).toBe('');
  });

  it('limits storage size and tolerates unavailable or full local storage', () => {
    const storage = memoryStorage();
    const key=draftStorageKey('/project','session');
    persistDraftText(storage,key,'a'.repeat(40000));
    expect(readStoredDraft(storage,key)).toHaveLength(20000);
    const unavailable = {
      getItem: (_key:string):string|null => {throw new Error('blocked');},
      setItem: (_key:string,_value:string):void => {throw new Error('full');},
      removeItem: (_key:string):void => {throw new Error('blocked');},
    };
    expect(readStoredDraft(unavailable,key)).toBe('');
    expect(() => persistDraftText(unavailable,key,'hello')).not.toThrow();
    expect(readStoredDraft(null,key)).toBe('');
  });
});

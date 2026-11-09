import {describe, expect, it} from 'vitest';
import {
  enqueueFollowup, followupKey, hasPersistedSessionId, loadFollowups, moveFollowup, shouldStartNextFollowup,
  parseFollowups, saveFollowups, MAX_FOLLOWUPS,
} from './followups';

function store() {
  const memory = new Map<string,string>();
  return {
    getItem: (key:string) => memory.get(key) ?? null,
    setItem: (key:string, value:string) => {memory.set(key,value);},
    removeItem: (key:string) => {memory.delete(key);},
  };
}

describe('follow-up tasks', () => {
  it('never persists unnamed pre-session tasks under a reusable placeholder ID', () => {
    expect(hasPersistedSessionId('unknown')).toBe(false);
    expect(hasPersistedSessionId(null)).toBe(false);
    expect(hasPersistedSessionId('')).toBe(false);
    expect(hasPersistedSessionId('real-session-id')).toBe(true);
  });

  it('never mixes different projects or conversations', () => {
    const project = 'D:\\Kroma';
    expect(followupKey(project,'a')).toBe(followupKey('d:/kroma','a'));
    expect(followupKey(project,'a')).not.toBe(followupKey(project,'b'));
    expect(followupKey(project,'a')).not.toBe(followupKey('D:/Other','a'));
    expect(followupKey('/home/Project','a')).not.toBe(followupKey('/home/project','a'));
  });

  it('stores, restores and clears tasks without replaying them automatically', () => {
    const storage = store();
    const key = followupKey('D:/Kroma','session');
    const first = enqueueFollowup([],'  add tests  ','task-1');
    expect(first).toEqual([{id:'task-1',text:'add tests'}]);
    saveFollowups(storage,key,first!);
    expect(loadFollowups(storage,key)).toEqual(first);
    saveFollowups(storage,key,[]);
    expect(loadFollowups(storage,key)).toEqual([]);
  });

  it('never advances the queue after cancellation, failure, pause, or exhaustion', () => {
    expect(shouldStartNextFollowup('completed',false,2)).toBe(true);
    expect(shouldStartNextFollowup('completed',true,2)).toBe(false);
    expect(shouldStartNextFollowup('failed',false,2)).toBe(false);
    expect(shouldStartNextFollowup('cancelled',false,2)).toBe(false);
    expect(shouldStartNextFollowup('completed',false,0)).toBe(false);
  });

  it('supports reordering without mutating or duplicating jobs', () => {
    const a = [{id:'1',text:'one'},{id:'2',text:'two'},{id:'3',text:'three'}];
    expect(moveFollowup(a,'3',-1).map(item=>item.id)).toEqual(['1','3','2']);
    expect(moveFollowup(a,'1',-1)).toEqual(a);
    expect(moveFollowup(a,'1',1).map(item=>item.id)).toEqual(['2','1','3']);
    expect(a.map(item=>item.id)).toEqual(['1','2','3']);
  });

  it('rejects malicious/oversized/unexpected persisted content', () => {
    expect(parseFollowups('invalid JSON')).toEqual([]);
    expect(parseFollowups('{"text":"not a list"}')).toEqual([]);
    expect(parseFollowups(JSON.stringify([{id:1,text:'wrong'},null,{id:'x',text:'  '}]))).toEqual([]);
    expect(parseFollowups('a'.repeat(250000))).toEqual([]);
    expect(enqueueFollowup([], ' ', 'id')).toBeNull();
    expect(enqueueFollowup([], 'x'.repeat(12001), 'id')).toBeNull();
    const queue = Array.from({length:MAX_FOLLOWUPS},(_,i)=>({id:String(i),text:'do'}));
    expect(enqueueFollowup(queue,'overflow','next')).toBeNull();
  });

  it('tolerates private-mode storage errors', () => {
    const blocked = {
      getItem: (_key:string): string|null => {throw new Error('blocked');},
      setItem: (_key:string,_value:string):void => {throw new Error('blocked');},
      removeItem: (_key:string):void => {throw new Error('blocked');},
    };
    expect(loadFollowups(blocked,'queue')).toEqual([]);
    expect(() => saveFollowups(blocked,'queue',[{id:'a',text:'job'}])).not.toThrow();
    expect(() => saveFollowups(blocked,'queue',[])).not.toThrow();
  });
});

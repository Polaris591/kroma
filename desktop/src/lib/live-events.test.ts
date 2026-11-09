import {describe, expect, it} from 'vitest';
import {foldLiveEvents, type LiveStreamState} from './live-events';
import {latestStreamingText} from './transcript';
import type {DesktopEvent} from './api';

const empty = (): LiveStreamState => ({activities: [], sequence: 0, modelStartIndex: 0});

describe('Native streaming batch reducer', () => {
  it('preserves model/tool order and updates each tool at its original position', () => {
    const events: DesktopEvent[] = [
      {kind:'model_started'},
      {kind:'reasoning_delta', text:'先检查'},
      {kind:'reasoning_delta', text:'目录'},
      {kind:'text_delta', text:'准备'},
      {kind:'text_delta', text:'构建'},
      {kind:'tool_started',id:'tool-1',name:'bash',arguments:'{"command":"test"}'},
      {kind:'tool_output',id:'tool-1',text:'pass'},
      {kind:'text_delta',text:'等待测试'},
      {kind:'tool_output',id:'tool-1',text:'ed'},
      {kind:'tool_finished',id:'tool-1',result:'not used',failed:false},
      {kind:'text_delta',text:'全部完成'},
    ];
    const result = foldLiveEvents(empty(),events);
    expect(result.warnings).toEqual([]);
    expect(result.state.activities.map(a=>a.kind)).toEqual([
      'reasoning','text','tool','text',
    ]);
    expect(result.state.activities[0].text).toBe('先检查目录');
    expect(result.state.activities[1].text).toBe('准备构建');
    expect(result.state.activities[2]).toMatchObject({
      name:'bash',output:'passed',done:true,failed:false,
    });
    expect(latestStreamingText(result.state.activities)).toBe('等待测试全部完成');
  });

  it('merges hundreds of deltas per batch without changing earlier state', () => {
    const first = foldLiveEvents(empty(),[{kind:'text_delta',text:'Hello'}]);
    const next = foldLiveEvents(first.state,
      Array.from({length:1200},()=>({kind:'text_delta',text:'X'})));
    expect(first.state.activities[0].text).toBe('Hello');
    expect(next.state.activities).toHaveLength(1);
    expect(next.state.activities[0].text).toBe('Hello'+'X'.repeat(1200));
    expect(next.state.sequence).toBe(first.state.sequence);
    expect(next.state.activities[0]).not.toBe(first.state.activities[0]);
  });

  it('handles concurrent tools, failure status, warning and final-only reasoning', () => {
    const begin = foldLiveEvents(empty(),[
      {kind:'model_started'},
      {kind:'tool_started',id:'one',name:'shell'},
      {kind:'tool_started',id:'two',name:'web_search'},
      {kind:'tool_output',id:'two',text:'search'},
      {kind:'tool_finished',id:'one',result:'failed command',failed:true},
      {kind:'warning',message:'budget low'},
      {kind:'model_finished',reasoning:'final reasoning'},
    ]);
    expect(begin.warnings).toEqual(['budget low']);
    expect(begin.state.activities.map(a=>a.kind)).toEqual(['tool','tool','reasoning']);
    expect(begin.state.activities[0]).toMatchObject({done:true,failed:true,output:'failed command'});
    expect(begin.state.activities[1]).toMatchObject({done:false,output:'search'});
    const final = foldLiveEvents(begin.state,[{kind:'model_finished',reasoning:'repeat'}]);
    expect(final.state.activities.filter(a=>a.kind==='reasoning')).toHaveLength(1);
  });

  it('caps oversized live text and tool output without losing sequence order', () => {
    const result = foldLiveEvents(empty(),[
      {kind:'text_delta',text:'a'.repeat(180000)},
      {kind:'tool_started',id:'t',name:'bash'},
      {kind:'tool_output',id:'t',text:'b'.repeat(45000)},
      {kind:'tool_finished',id:'t',result:'ignored'},
    ]);
    expect(result.state.activities[0].text).toHaveLength(160000);
    expect(result.state.activities[1].output).toHaveLength(32000);
    expect(result.state.activities[1].done).toBe(true);
  });

  it('returns the same activity reference when a batch contains only status information', () => {
    const first = foldLiveEvents(empty(),[{kind:'text_delta',text:'text'}]);
    const second = foldLiveEvents(first.state,[{kind:'warning',message:'check rate limits'}]);
    expect(second.state.activities).toBe(first.state.activities);
    expect(second.warnings).toEqual(['check rate limits']);
  });
});

describe('Live activity keys', () => {
  it('keeps text and numeric tool ids in separate, unique namespaces', () => {
    const result = foldLiveEvents(empty(), [
      {kind:'text_delta', text:'一'},
      {kind:'tool_started', id: 7 as unknown as string, name:'bash'},
      {kind:'tool_output', id: 7 as unknown as string, text:'x'},
      {kind:'tool_finished', id: 7 as unknown as string},
      {kind:'text_delta', text:'二'},
    ]);
    const ids = result.state.activities.map(a => a.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids[0]).toBe('text:1');
    expect(ids[1]).toBe('tool:7:2');
    expect(result.state.activities[1]).toMatchObject({callId:'7', output:'x', done:true});
    expect(result.state.activities[2]).toMatchObject({kind:'text', text:'二'});
    expect(result.state.activities[2].id).not.toBe(ids[0]);
  });

  it('keeps a tool id reused by a later turn distinct and does not rewrite the finished tool', () => {
    const first = foldLiveEvents(empty(), [
      {kind:'tool_started', id:'call-1', name:'bash'},
      {kind:'tool_output', id:'call-1', text:'one'},
      {kind:'tool_finished', id:'call-1'},
    ]);
    const second = foldLiveEvents(first.state, [
      {kind:'tool_started', id:'call-1', name:'bash'},
      {kind:'tool_output', id:'call-1', text:'two'},
      {kind:'tool_finished', id:'call-1'},
    ]);
    const tools = second.state.activities;
    expect(tools).toHaveLength(2);
    expect(tools[0].id).not.toBe(tools[1].id);
    expect(tools[0].output).toBe('one');
    expect(tools[1].output).toBe('two');
    expect(tools.every(tool => tool.done)).toBe(true);
  });
});

describe('Live resync', () => {
  it('reports a dropped batch as a warning and requests a history refetch', () => {
    const result = foldLiveEvents(empty(), [
      {kind:'text_delta', text:'部分输出'},
      {kind:'resync_required', dropped: 42},
    ]);
    expect(result.resync).toBe(true);
    expect(result.warnings).toHaveLength(1);
    expect(result.warnings[0]).toContain('42');
    expect(result.state.activities).toHaveLength(1);
  });

  it('stays quiet without a resync event', () => {
    const result = foldLiveEvents(empty(), [{kind:'text_delta', text:'ok'}]);
    expect(result.resync).toBe(false);
    expect(result.warnings).toEqual([]);
  });
});

describe('Goal projection inside a stream batch', () => {
  it('surfaces the host goal event without touching the transcript', () => {
    const result = foldLiveEvents(empty(), [
      {kind:'text_delta', text:'处理中'},
      {kind:'task_control', owner:'kroma-goal', id:'goal-7', revision:4,
        status:'paused', reason:'user_stop', summary:'把登录页改成深色'},
    ]);
    expect(result.goal).toMatchObject({
      owner:'kroma-goal', id:'goal-7', revision:4, status:'paused', reason:'user_stop',
    });
    expect(result.state.activities).toHaveLength(1);
    expect(result.state.activities[0].text).toBe('处理中');
    expect(result.warnings).toEqual([]);
  });

  it('keeps the last goal of a batch and reports none otherwise', () => {
    const batch = foldLiveEvents(empty(), [
      {kind:'task_control', id:'goal-7', status:'running', revision:1},
      {kind:'task_control', id:'goal-7', status:'completed', revision:2, reason:'completed'},
    ]);
    expect(batch.goal).toMatchObject({status:'completed', revision:2});
    expect(foldLiveEvents(empty(), [{kind:'text_delta', text:'ok'}]).goal).toBeNull();
  });
});

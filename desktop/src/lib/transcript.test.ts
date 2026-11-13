import { describe, expect, it } from 'vitest';
import type { Message } from './api';
import { marked } from 'marked';
import { groupTranscript, latestStreamingText, summarizeTurn, visibleTranscriptStart } from './transcript';

const user = (content: string): Message => ({role:'user',content});
const assistant = (content: string, calls: Array<{id:string;function:{name:string;arguments:string}}> = []): Message =>
  ({role:'assistant',content,tool_calls:calls});
const tool = (id: string, content: string): Message =>
  ({role:'tool',tool_call_id:id,content});
const call = (id:string,name:string) => ({id,function:{name,arguments:'{}'}});

describe('foundation durable inputs', () => {
  it('keeps reference and continuation inputs out of user turns and joins display fragments', () => {
    const history: Message[] = [user('write'),
      {...assistant('A'), provider_items:[{type:'kroma_output_fragment'}]},
      {...user('reference'), provider_items:[{type:'kroma_context_reference'}]},
      {...user('continue'), provider_items:[{type:'kroma_continuation'}]}, assistant('B')];
    const turns = groupTranscript(history);
    expect(turns).toHaveLength(1);
    expect(summarizeTurn(turns[0]).answer?.text).toBe('AB');
    expect(visibleTranscriptStart(history,2)).toBe(0);
    expect(history[4].content).toBe('B');
  });
  it('renders a preview while retaining the full post-hook tool result', () => {
    const result: Message = {...tool('t','complete evidence'), provider_items:[
      {type:'kroma_tool_projection',preview:'bounded preview'}]};
    const [turn] = groupTranscript([user('run'),assistant('',[call('t','tool')]),result]);
    expect(turn.activities[0]).toMatchObject({kind:'tool',output:'bounded preview'});
    expect(result.content).toBe('complete evidence');
  });
});

describe('Live Markdown', () => {
  it('retains streamed markdown text while later tool events arrive', () => {
    const incoming = '## 进度更新\n\n**关键结果**\n\n- 文件一\n- 文件二';
    const activities = [
      {kind:'reasoning' as const,id:'1',text:'分析中'},
      {kind:'text' as const,id:'2',text:incoming},
      {kind:'tool' as const,id:'t3',text:'',name:'bash',done:false},
    ];
    expect(latestStreamingText(activities)).toBe(incoming);
    expect(marked.parse(latestStreamingText(activities))).toContain('<h2>');
    expect(marked.parse(latestStreamingText(activities))).toContain('<strong>');
    expect(marked.parse(latestStreamingText(activities))).toContain('<li>');
    expect(latestStreamingText([...activities,{kind:'text',id:'4',text:'新的回复'}])).toBe('新的回复');
  });

  it('allows a Markdown code fence before its closing delimiter arrives', () => {
    const partial = '```ts\nconst completed = false;\n';
    expect(marked.parse(partial)).toContain('<pre>');
    expect(marked.parse(partial)).toContain('const completed = false;');
    expect(latestStreamingText([])).toBe('');
  });
});

describe('Desktop transcript rendering', () => {
  it('groups multi-step model and tool output under one user turn in chronological order', () => {
    const turns = groupTranscript([
      user('report'),
      {...assistant('',[call('a','web_search'),call('b','shell')]),reasoning_content:'plan'},
      tool('b','shell result'),tool('a','search result'),
      assistant('interim',[call('c','web_fetch')]),
      tool('c','fetched'),assistant('final answer'),
      user('follow up'), assistant('next answer')
    ]);
    expect(turns).toHaveLength(2);
    expect(turns[0].activities.map(a => a.kind === 'tool' ? a.name : a.text))
      .toEqual(['', 'web_search', 'shell', 'interim', 'web_fetch', 'final answer']);
    expect(turns[0].activities.filter(a => a.kind === 'tool').map(a => a.output))
      .toEqual(['search result','shell result','fetched']);
    expect(turns[1].user?.content).toBe('follow up');
    expect(turns[1].activities).toHaveLength(1);
  });

  it('preserves unmatched tool records without reassigning them to a different turn', () => {
    const turns = groupTranscript([tool('orphan','orphan output'),user('next'),tool('orphan','late output')],15);
    expect(turns).toHaveLength(2);
    expect(turns[0].activities[0]).toMatchObject({kind:'tool',output:'orphan output'});
    expect(turns[1].activities[0]).toMatchObject({kind:'tool',output:'late output'});
    expect(turns[1].id).toBe(16);
  });

  it('surfaces the last answer and keeps ordered steps available for expansion', () => {
    const [turn] = groupTranscript([
      user('run tests'),
      {...assistant('',[call('t1','shell')]), reasoning_content:'plan'},
      {...tool('t1','failed'), provider_items:[{type:'kroma_tool_status',is_error:true}]},
      assistant('try alternate command', [call('t2','shell')]),
      tool('t2','passed'),
      assistant('Tests passed after retry.')
    ]);
    const summary = summarizeTurn(turn);
    expect(summary.answer?.text).toBe('Tests passed after retry.');
    expect(summary.toolCount).toBe(2);
    expect(summary.failures).toBe(1);
    expect(summary.process.map(s => s.kind === 'tool' ? s.output : s.text))
      .toEqual(['','failed','try alternate command','passed']);
  });

  it('does not invent a final answer for interrupted tool-only work', () => {
    const [turn] = groupTranscript([user('compile'),assistant('',[call('t','shell')]),tool('t','building...')]);
    const summary = summarizeTurn(turn);
    expect(summary.answer).toBeNull();
    expect(summary.process).toHaveLength(1);
    expect(summary.toolCount).toBe(1);
  });

  it('begins ordinary history pages on their preceding user turn', () => {
    const history = [user('old'),assistant('old answer'),user('new'),assistant('',[call('x','shell')]),tool('x','ok'),assistant('done')];
    expect(visibleTranscriptStart(history,2)).toBe(2);
    expect(groupTranscript(history.slice(2),2)[0].activities).toHaveLength(2);
    expect(visibleTranscriptStart(history,100)).toBe(0);
  });

  it('bounds a single very long tool turn without losing older history access', () => {
    const history = [
      user('compile the whole repository'),
      ...Array.from({length:1200}, (_,index): Message =>
        index % 2 === 0 ? assistant('tool progress') : tool('legacy','result'))
    ];
    const start = visibleTranscriptStart(history,72);
    expect(start).toBe(history.length - 72);
    expect(history.slice(start)).toHaveLength(72);
    expect(groupTranscript(history.slice(start),start).length).toBeGreaterThan(0);
    expect(visibleTranscriptStart(history,history.length)).toBe(0);
  });
});

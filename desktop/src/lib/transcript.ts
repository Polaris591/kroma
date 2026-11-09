import type { Message } from './api';

export type LiveActivity = {
  kind: 'text' | 'reasoning' | 'tool';
  /** Unique UI key; never the provider call id (see liveActivityKey). */
  id: string;
  /** Provider tool-call id, kept only to join streamed tool output. */
  callId?: string;
  text: string;
  name?: string;
  args?: string;
  output?: string;
  failed?: boolean;
  done?: boolean;
};

/** The most recent piece of assistant narration remains visible while
 * tools run. Only the live rendering is derived; the full event sequence
 * stays available in the execution log. */
export function latestStreamingText(activities: LiveActivity[]): string {
  for (let index = activities.length - 1; index >= 0; index--) {
    const activity = activities[index];
    if (activity.kind === 'text' && activity.text) return activity.text;
  }
  return '';
}

export type TranscriptActivity =
  | {kind: 'assistant'; id: number; text: string; reasoning: string}
  | {kind: 'tool'; id: number; name: string; args: string; output: string | null; failed: boolean};

export type TranscriptTurn = {
  id: number;
  user: Message | null;
  activities: TranscriptActivity[];
};

/** Keep the WAL message order. Tool results are joined to their initiating
 * call by tool_call_id, rather than displayed as a second assistant turn.
 * A standalone tool result (e.g. an old page split) is still visible.
 */
export function groupTranscript(messages: Message[], offset = 0): TranscriptTurn[] {
  const turns: TranscriptTurn[] = [];
  const pending = new Map<string, Extract<TranscriptActivity, {kind:'tool'}>>();
  let active: TranscriptTurn | undefined;
  const ensureTurn = (id: number): TranscriptTurn => {
    if (!active) {
      active = {id, user: null, activities: []};
      turns.push(active);
    }
    return active;
  };
  messages.forEach((message, localIndex) => {
    const id = offset + localIndex;
    if (message.role === 'system') return;
    if (message.role === 'user') {
      active = {id, user: message, activities: []};
      turns.push(active);
      pending.clear();
      return;
    }
    const turn = ensureTurn(id);
    if (message.role === 'tool') {
      const matching = message.tool_call_id ? pending.get(message.tool_call_id) : undefined;
      const failed = !!message.provider_items?.some(
        item => item.type === 'kroma_tool_status' && item.is_error === true
      );
      if (matching) {
        matching.output = message.content ?? '';
        matching.failed = failed;
        pending.delete(message.tool_call_id!);
      } else {
        turn.activities.push({
          kind:'tool', id, name:'工具结果', args:'',
          output:message.content ?? '', failed
        });
      }
      return;
    }
    const text = (message.content ?? '').replace(/[\uE000-\uF8FF]/g, '');
    const reasoning = message.reasoning_content ?? '';
    if (text.trim() || reasoning.trim()) {
      turn.activities.push({kind:'assistant', id, text, reasoning});
    }
    for (const [index, call] of (message.tool_calls ?? []).entries()) {
      const item: Extract<TranscriptActivity, {kind:'tool'}> = {
        kind:'tool', id:id * 100 + index, name:call.function?.name || '调用工具',
        args:call.function?.arguments || '', output:null, failed:false
      };
      turn.activities.push(item);
      if (call.id) pending.set(call.id, item);
    }
  });
  return turns;
}

/** Finished history: surface the final answer immediately, while preserving
 * the full ordered sequence (including intermediate commentary) on expansion.
 * No task-success claim is inferred from the final text or tool exit codes. */
export function summarizeTurn(turn: TranscriptTurn): {
  answer: Extract<TranscriptActivity, {kind: 'assistant'}> | null;
  process: TranscriptActivity[];
  toolCount: number;
  failures: number;
} {
  const last = turn.activities[turn.activities.length - 1];
  const answer = last?.kind === 'assistant' && last.text.trim() ? last : null;
  const process = answer ? turn.activities.slice(0, -1) : turn.activities;
  const tools = process.filter((step): step is Extract<TranscriptActivity, {kind: 'tool'}> => step.kind === 'tool');
  return {answer, process, toolCount: tools.length, failures: tools.filter(step => step.failed).length};
}

/** Prefer the start of a user turn when showing recent history, but never
 * defeat the bounded transcript window just because a single coding task
 * produced hundreds of model/tool messages. The UI can load or reveal older
 * steps on demand; groupTranscript also handles a partial leading tool turn.
 */
export function visibleTranscriptStart(messages: Message[], limit: number): number {
  const requested = Math.max(1, Math.floor(limit));
  const first = Math.max(0, messages.length - requested);
  if (!first) return 0;
  // A short backtrack keeps normal conversations intact without forcing a
  // thousand-tool run to render in full when just the last page was requested.
  const backtrack = Math.min(160, Math.max(48, requested));
  const earliest = Math.max(0, first - backtrack);
  for (let index = first; index >= earliest; index--) {
    if (messages[index].role === 'user') return index;
  }
  return first;
}

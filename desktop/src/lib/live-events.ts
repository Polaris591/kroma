import type { DesktopEvent } from './api';
import type { LiveActivity } from './transcript';
import { goalFromEvent, type GoalState } from './goal';

export interface LiveStreamState {
  activities: LiveActivity[];
  sequence: number;
  modelStartIndex: number;
}

type Pending = {kind: 'text' | 'reasoning' | 'tool_output'; id?: string; text: string};

/**
 * Key for one live activity. Text, reasoning and tool entries never share a
 * namespace, and a tool key carries its sequence because providers routinely
 * reuse a call id (or hand out plain numbers) across turns.
 */
export function liveActivityKey(sequence: number, kind: 'text' | 'reasoning' | 'tool', callId = ''): string {
  return kind === 'tool' ? `tool:${callId}:${sequence}` : `${kind}:${sequence}`;
}

/**
 * Fold a coalesced native stream batch into one UI update.
 *
 * Tool results are updated in place in their original position. Text and
 * output deltas are combined before concatenation, so a batch with hundreds
 * of chunks does not repeatedly copy the entire growing response.
 *
 * This is a UI-only projection; the authoritative history remains in the WAL.
 */
export function foldLiveEvents(
  current: LiveStreamState,
  events: readonly DesktopEvent[],
): {state: LiveStreamState; warnings: string[]; resync: boolean; goal: GoalState | null} {
  const activities = [...current.activities];
  const cloned = new Set<number>();
  const toolPositions = new Map<string, number[]>();
  let sequence = current.sequence;
  let modelStartIndex = current.modelStartIndex;
  let changed = false;
  const warnings: string[] = [];
  // A dropped batch means this projection is no longer contiguous, so the
  // caller must refetch authoritative history instead of trusting it.
  let resync = false;
  // The last goal projection in this batch. Goal state is host-published, so a
  // batch that only reports a goal must not create a transcript entry.
  let goal: GoalState | null = null;
  let pending: Pending | null = null;

  // Positions are indexed by the provider's call id, never by the UI key.
  for (let index = 0; index < activities.length; index++) {
    const a = activities[index];
    if (a.kind === 'tool') {
      const callId = a.callId ?? a.id;
      const matches = toolPositions.get(callId) ?? [];
      matches.push(index);
      toolPositions.set(callId, matches);
    }
  }

  // A provider id can be reused by a later turn. Updates then belong to the
  // tool that is still running, never to the already finished one.
  const targets = (callId: string): number[] => {
    const matches = toolPositions.get(callId) ?? [];
    const live = matches.filter(index => !activities[index].done);
    return live.length ? live : matches.slice(-1);
  };

  const writable = (index: number): LiveActivity => {
    if (!cloned.has(index)) {
      activities[index] = {...activities[index]};
      cloned.add(index);
    }
    changed = true;
    return activities[index];
  };

  const appendText = (kind: 'text' | 'reasoning', delta: string) => {
    if (!delta) return;
    const lastIndex = activities.length - 1;
    const last = activities[lastIndex];
    if (last?.kind === kind) {
      const target = writable(lastIndex);
      target.text = (target.text + delta).slice(-160000);
    } else {
      activities.push({kind, id: liveActivityKey(++sequence, kind), text: delta.slice(-160000)});
      changed = true;
    }
  };

  const flush = () => {
    if (!pending) return;
    const batch = pending;
    pending = null;
    if (batch.kind === 'tool_output') {
      for (const index of targets(batch.id ?? '')) {
        const tool = writable(index);
        tool.output = ((tool.output ?? '') + batch.text).slice(-32000);
      }
    } else {
      appendText(batch.kind, batch.text);
    }
  };

  const accumulate = (kind: Pending['kind'], text: string, id?: string) => {
    if (!text) return;
    if (pending?.kind === kind && pending.id === id) {
      pending.text += text;
    } else {
      flush();
      pending = {kind, id, text};
    }
  };

  for (const event of events) {
    switch (event.kind) {
      case 'text_delta':
        accumulate('text', event.text ?? '');
        break;
      case 'reasoning_delta':
        accumulate('reasoning', event.text ?? '');
        break;
      case 'tool_output':
        accumulate('tool_output', event.text ?? '', String(event.id));
        break;
      default:
        flush();
        switch (event.kind) {
          case 'warning':
            warnings.push(String(event.message ?? '运行警告'));
            break;
          case 'model_started':
            modelStartIndex = activities.length;
            break;
          case 'model_finished':
            // Some providers report reasoning only after generation finishes.
            // Never insert it twice when a streaming reasoning delta exists.
            if (event.reasoning &&
                !activities.slice(modelStartIndex).some(a =>
                  a.kind === 'reasoning' && a.text)) {
              appendText('reasoning', String(event.reasoning));
            }
            break;
          case 'tool_started': {
            const callId = String(event.id);
            const index = activities.length;
            activities.push({
              kind: 'tool', id: liveActivityKey(++sequence, 'tool', callId), callId,
              name: String(event.name),
              text: '', args: String(event.arguments ?? ''),
              output: '', done: false, failed: false,
            });
            const matches = toolPositions.get(callId) ?? [];
            matches.push(index);
            toolPositions.set(callId, matches);
            changed = true;
            break;
          }
          case 'resync_required': {
            // The native host dropped live events it could not coalesce. Say
            // so instead of showing a silently gapped timeline.
            const dropped = Number(event.dropped ?? 0);
            resync = true;
            warnings.push(dropped > 0
              ? `实时事件积压，已跳过 ${dropped} 条；正在重新同步会话历史`
              : '实时事件积压；正在重新同步会话历史');
            break;
          }
          case 'task_control': {
            goal = goalFromEvent(event) ?? goal;
            break;
          }
          case 'tool_finished': {
            for (const index of targets(String(event.id))) {
              const tool = writable(index);
              tool.output = tool.output || String(event.result ?? '');
              tool.done = true;
              tool.failed = !!event.failed;
            }
            break;
          }
        }
        break;
    }
  }
  flush();
  return {
    state: {
      activities: changed ? activities : current.activities,
      sequence,
      modelStartIndex,
    },
    warnings,
    resync,
    goal,
  };
}

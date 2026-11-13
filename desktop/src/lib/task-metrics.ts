import type { ContextReport, DesktopEvent, SessionSnapshot, SessionUsage, Usage } from './api';

export interface TaskMetrics {
  context: ContextReport | null;
  window: number;
  available: number;
  usage: SessionUsage;
}

const emptyUsage = (): Usage => ({calls: 0, input: 0, output: 0, cached: 0, reasoning: 0});
const counter = (value: unknown): number | null => typeof value === 'number'
  && Number.isFinite(value) && value >= 0 ? Math.floor(value) : null;

export function createTaskMetrics(snapshot: SessionSnapshot | null | undefined): TaskMetrics {
  return {
    context: snapshot?.context ?? null,
    window: snapshot?.context_window ?? 0,
    available: snapshot?.available_input ?? 0,
    usage: snapshot?.usage ?? {
      coding: emptyUsage(), title: emptyUsage(), apec: emptyUsage(),
      context_input: null, context_estimated: true,
    },
  };
}

/** Per-task observations, never a replacement for the durable transcript.
 * ContextPrepared is the admitted request estimate, not accumulated billing.
 * Model usage is added once per response; cached input is already in input.
 */
export function foldTaskMetrics(current: TaskMetrics, events: readonly DesktopEvent[]): TaskMetrics {
  let next = current;
  for (const event of events) {
    if (event.kind === 'context') {
      const estimated = counter(event.estimated);
      const available = counter(event.available);
      if (estimated === null || available === null) continue;
      const window = current.context?.window || current.window;
      next = {
        ...next, available,
        context: window > 0 ? {
          window, available_input: available,
          output_reserve: current.context?.output_reserve ?? Math.max(0, window - available),
          // The event reports the actual request total, not its composition.
          // Never keep stale pre-task slices or fabricate message tokens.
          segments: [], estimated_input: estimated, basis: 'request',
        } : null,
        usage: {...next.usage, context_input: estimated, context_estimated: true},
      };
    } else if (['model_finished', 'failed_request_usage', 'compaction_usage'].includes(event.kind)) {
      const group = event.kind === 'compaction_usage' ? 'apec' : 'coding';
      const calls = group === 'apec' ? counter(event.requests) ?? 0 : 1;
      if (!calls) continue;
      const before = next.usage[group];
      next = {...next, usage: {...next.usage, [group]: {
        ...before, calls: before.calls + calls,
        input: before.input + (counter(event.input) ?? 0),
        output: before.output + (counter(event.output) ?? 0),
        cached: before.cached + (counter(event.cached) ?? 0),
        reasoning: before.reasoning + (counter(event.reasoning_tokens) ?? 0),
      }}};
    }
  }
  return next;
}

/** Only the owning running task may overlay its metadata. Final snapshots and
 * every other conversation remain authoritative, with their message identity
 * intact. No streamed text is promoted to history here.
 */
export function projectTaskMetrics(snapshot: SessionSnapshot | null, metrics: TaskMetrics | undefined, active: boolean): SessionSnapshot | null {
  if (!snapshot || !metrics || !active) return snapshot;
  return {...snapshot, context: metrics.context, usage: metrics.usage,
    context_window: metrics.window, available_input: metrics.available};
}

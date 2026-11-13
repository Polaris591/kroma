import { describe, expect, it } from 'vitest';
import type { DesktopEvent, SessionSnapshot } from './api';
import { contextDisplay } from './context-display';
import { createTaskMetrics, foldTaskMetrics, projectTaskMetrics } from './task-metrics';

const snapshot = (): SessionSnapshot => ({
  session_id: 'A', messages: [{role: 'user', content: 'keep history'}],
  context_window: 500000, available_input: 467232, tool_names: ['shell'],
  context: {window: 500000, available_input: 467232, output_reserve: 32768,
    segments: [{key: 'system', tokens: 5700}]},
  usage: {coding: {calls: 2, input: 1000, output: 100, cached: 400, reasoning: 20},
    title: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0},
    apec: {calls: 0, input: 0, output: 0, cached: 0, reasoning: 0},
    context_input: 5700, context_estimated: true},
});

describe('live task metrics', () => {
  it('uses the admitted request estimate instead of frozen pre-task slices', () => {
    const before = snapshot();
    const next = foldTaskMetrics(createTaskMetrics(before), [{kind: 'context', estimated: 75000, available: 467232}]);
    expect(contextDisplay(next.context, next.window)).toMatchObject({used: 75000, percent: 15, free: 392232});
    expect(next.context?.segments).toEqual([]);
    expect(next.context?.basis).toBe('request');
    expect(before.context?.segments).toEqual([{key: 'system', tokens: 5700}]);
  });

  it('updates every request and accepts a real zero estimate', () => {
    const first = foldTaskMetrics(createTaskMetrics(snapshot()), [{kind: 'context', estimated: 100000, available: 467232}]);
    const next = foldTaskMetrics(first, [{kind: 'context', estimated: 0, available: 467232}]);
    expect(contextDisplay(next.context, next.window).used).toBe(0);
    expect(next.usage.context_input).toBe(0);
  });

  it('adds reported usage without adding cache subsets to input', () => {
    const next = foldTaskMetrics(createTaskMetrics(snapshot()), [
      {kind: 'model_finished', input: 12000, output: 800, cached: 9000, reasoning_tokens: 300},
      {kind: 'model_finished', input: 14000, output: 900, cached: 10000},
    ]);
    expect(next.usage.coding).toMatchObject({calls: 4, input: 27000, output: 1800, cached: 19400, reasoning: 320});
  });

  it('keeps failed attempts and compaction usage in their own meters', () => {
    const next = foldTaskMetrics(createTaskMetrics(snapshot()), [
      {kind: 'failed_request_usage', input: 100, output: 20},
      {kind: 'compaction_usage', requests: 2, input: 5000, output: 300, cached: 1000},
    ]);
    expect(next.usage.coding.input).toBe(1100);
    expect(next.usage.coding.calls).toBe(3);
    expect(next.usage.apec).toMatchObject({calls: 2, input: 5000, output: 300, cached: 1000});
    expect(next.usage.title.calls).toBe(0);
  });

  it('does not turn missing provider counters into estimated billing', () => {
    const next = foldTaskMetrics(createTaskMetrics(snapshot()), [{kind: 'model_finished', input: null, output: null}]);
    expect(next.usage.coding.input).toBe(1000);
    expect(next.usage.coding.output).toBe(100);
    expect(next.usage.coding.calls).toBe(3);
  });

  it('ignores malformed context numbers and does not invent capacity', () => {
    const initial = createTaskMetrics(snapshot());
    for (const estimated of [NaN, Infinity, -1, null, '100']) {
      expect(foldTaskMetrics(initial, [{kind: 'context', estimated, available: 467232}])).toBe(initial);
    }
    const missing = foldTaskMetrics(createTaskMetrics(null), [{kind: 'context', estimated: 10000, available: 20000}]);
    expect(missing.context).toBeNull();
    expect(missing.window).toBe(0);
    expect(missing.usage.context_input).toBe(10000);
  });

  it('does not change metrics for text and tool progress alone', () => {
    const initial = createTaskMetrics(snapshot());
    const events: DesktopEvent[] = [{kind: 'text_delta', text: 'hello'}, {kind: 'tool_finished', id: '1'}];
    expect(foldTaskMetrics(initial, events)).toBe(initial);
  });

  it('overlays metadata only and never promotes streamed text into history', () => {
    const before = snapshot();
    const metrics = foldTaskMetrics(createTaskMetrics(before), [{kind: 'context', estimated: 100000, available: 467232}]);
    const projected = projectTaskMetrics(before, metrics, true)!;
    expect(projected.messages).toBe(before.messages);
    expect(projected.session_id).toBe('A');
    expect(projected.context?.estimated_input).toBe(100000);
    expect(projected.tool_names).toBe(before.tool_names);
  });

  it('lets final snapshots and non-owning conversations win unchanged', () => {
    const final = snapshot();
    const metrics = createTaskMetrics(snapshot());
    expect(projectTaskMetrics(final, metrics, false)).toBe(final);
    expect(projectTaskMetrics(final, undefined, true)).toBe(final);
    expect(projectTaskMetrics(null, metrics, true)).toBeNull();
  });
});

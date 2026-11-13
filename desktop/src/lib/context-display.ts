import type { ContextReport } from './api';

export function contextDisplay(report: ContextReport | null, total: number | null) {
  const capacity = Math.max(0, report?.window ?? total ?? 0);
  const segments = (report?.segments ?? []).filter(segment => segment.tokens > 0);
  // A configured window or input allowance says nothing about the amount of
  // conversation in it. Missing measurements must not appear as zero usage.
  const used = report
    ? report.estimated_input ?? segments.reduce((sum, segment) => sum + segment.tokens, 0) : null;
  const reserve = report ? Math.max(0, report.output_reserve) : null;
  const inputLimit = report
    ? Math.max(0, Math.min(report.available_input, capacity - (reserve ?? 0)))
    : null;
  return {
    capacity, segments, used, reserve,
    free: inputLimit !== null && used !== null ? Math.max(0, inputLimit - used) : null,
    percent: used !== null && capacity > 0 ? Math.round(used / capacity * 100) : null,
    fraction: used !== null && capacity > 0 ? Math.min(1, used / capacity) : 0,
  };
}

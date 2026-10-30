/** Small, pure formatting helpers for status metrics. Keep values honest. */
export function compactCount(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value) || value < 0) return '—';
  return value >= 1_000_000 ? (value / 1_000_000).toFixed(1) + 'm'
    : value >= 1000 ? (value / 1000).toFixed(1) + 'k' : String(value);
}

export function cachePercentage(input: number | null | undefined, cached: number | null | undefined): string {
  if (input == null || cached == null || !Number.isFinite(input) || !Number.isFinite(cached) || input <= 0 || cached < 0) return '—';
  if (cached >= input) return cached === input ? '100% (reported)' : '—';
  const pct = cached / input * 100;
  return pct >= 99.95 ? '<100%' : pct.toFixed(1) + '%';
}

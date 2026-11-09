/** UI-only rendering budget for the currently streaming Markdown segment.
 * The Agent stream and WAL are never throttled or truncated. */
export function markdownRefreshDelay(length: number): number {
  if (length > 64000) return 180;
  if (length > 16000) return 100;
  return 50;
}

/** Bulk selection is scoped by project, so the same id in two projects never
 * collides. Keys are "<project>\u0000<id>". */
const SEPARATOR = '\u0000';

export function sessionSelectionKey(project: string, id: string): string {
  return project + SEPARATOR + id;
}

export function parseSelectionKey(key: string): { project: string; id: string } | null {
  const split = key.indexOf(SEPARATOR);
  if (split < 0) return null;
  return { project: key.slice(0, split), id: key.slice(split + 1) };
}

export function toggleSelection(selected: Set<string>, project: string, id: string): Set<string> {
  const next = new Set(selected);
  const key = sessionSelectionKey(project, id);
  if (next.has(key)) next.delete(key); else next.add(key);
  return next;
}

/** Shift-click adds the whole visible run between the anchor and the target. */
export function extendSelection(
  selected: Set<string>, ids: string[], project: string, anchorId: string, targetId: string,
): Set<string> {
  const from = ids.indexOf(anchorId);
  const to = ids.indexOf(targetId);
  if (from < 0 || to < 0) return selected;
  const next = new Set(selected);
  for (const id of ids.slice(Math.min(from, to), Math.max(from, to) + 1)) {
    next.add(sessionSelectionKey(project, id));
  }
  return next;
}

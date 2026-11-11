// One plugin question at a time, shown in the composer slot. Nothing here
// knows which plugin asked: a stream message becomes an answer payload, and
// closing the bar is never approval.
export interface AskQuestion {
  id: number;
  question: string;
  options: string[];
  multiple: boolean;
}

export interface AskAnswer {
  id: number;
  picked: string[];
  typed: string | null;
  dismissed: boolean;
}

/// The payload comes from a plugin, so a malformed message is ignored instead
/// of leaving an unanswerable bar on the composer.
export function readAsk(value: unknown): AskQuestion | null {
  if (!value || typeof value !== 'object') return null;
  const source = value as Record<string, unknown>;
  const id = Number(source.id);
  const question = typeof source.question === 'string' ? source.question.trim() : '';
  if (!question || !Number.isFinite(id)) return null;
  const options = Array.isArray(source.options)
    ? source.options.filter((option): option is string => typeof option === 'string' && !!option.trim())
    : [];
  return {id, question, options, multiple: source.multiple === true};
}

/// A newer question replaces the open one, which is answered `dismissed` first
/// so no plugin waits for a bar that is already gone.
export function nextAsk(current: AskQuestion | null, incoming: AskQuestion):
  {pending: AskQuestion; dismiss: number | null} {
  const superseded = current && current.id !== incoming.id ? current.id : null;
  return {pending: incoming, dismiss: superseded};
}

/// ↑/↓ over the options; -1 means nothing is highlighted yet.
export function moveCursor(cursor: number, delta: number, count: number): number {
  if (count < 1) return -1;
  if (cursor < 0) return delta >= 0 ? 0 : count - 1;
  return ((cursor + delta) % count + count) % count;
}

/// Single choice replaces the pick; `multiple` toggles the label.
export function togglePick(picked: string[], label: string, multiple: boolean): string[] {
  if (multiple) return picked.includes(label) ? picked.filter(item => item !== label) : [...picked, label];
  return picked.includes(label) ? [] : [label];
}

/// A typed answer always travels in `typed`, so the plugin knows the person
/// wrote it instead of picking an option.
export function buildAnswer(ask: AskQuestion, picked: string[], typed: string): AskAnswer {
  const text = typed.trim();
  return {
    id: ask.id,
    picked: ask.multiple ? [...picked] : picked.slice(0, 1),
    typed: text || null,
    dismissed: false,
  };
}

/// Closing without answering. Never approval.
export function dismissAnswer(id: number): AskAnswer {
  return {id, picked: [], typed: null, dismissed: true};
}

/// Whether confirming would send anything at all.
export function canAnswer(picked: string[], typed: string): boolean {
  return picked.length > 0 || typed.trim().length > 0;
}

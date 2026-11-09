import type { CheckpointTurn, Message, TurnDiffSummary } from './api';

/** Snapshots are taken per tool-using task, so a rendered turn is paired with
 * the newest snapshot whose transcript anchor falls inside it. */
export function turnNumberByUserIndex(
  turns: CheckpointTurn[], messages: Message[],
): Map<number, number> {
  const map = new Map<number, number>();
  if (!turns.length) return map;
  const userIndices: number[] = [];
  messages.forEach((message, index) => { if (message.role === 'user') userIndices.push(index); });
  for (const entry of turns) {
    if (entry.anchor == null) continue;
    let chosen: number | null = null;
    for (const index of userIndices) {
      if (index <= entry.anchor) chosen = index; else break;
    }
    if (chosen !== null) map.set(chosen, entry.turn);
  }
  return map;
}

/** Only offer a turn action when the index really addresses a user message in
 * the Agent's own list; a stale or shifted index must never rewind the wrong place. */
export function canActOnTurn(messages: Message[], index: number): boolean {
  return index >= 0 && index < messages.length && messages[index].role === 'user';
}

export function turnDiffFor(
  diffs: Map<number, TurnDiffSummary>, turn: number | null | undefined,
): TurnDiffSummary | null {
  if (turn == null) return null;
  return diffs.get(turn) ?? null;
}

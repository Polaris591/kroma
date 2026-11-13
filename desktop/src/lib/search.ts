/** In-conversation search over the text the transcript actually renders.
 *
 * Only the person's prompts and assistant answer bodies are searched. Tool
 * output, system prompts, reasoning and host-generated reference messages are
 * excluded, so every hit has a visible target. Nothing here reads a file or
 * calls the model.
 */
import type { Message } from './api';
import { isUserMessage } from './transcript';

export interface FindHit {
  /** Absolute index in the complete conversation, not in the loaded window. */
  messageIndex: number;
  role: 'user' | 'assistant';
}

const PRIVATE_USE = /[\uE000-\uF8FF]/g;

/** The rendered text of one message, or null when the transcript hides it. */
export function findableText(message: Message): {role: 'user' | 'assistant'; text: string} | null {
  const items = message.provider_items ?? [];
  if (items.some(item => item.type === 'kroma_context_reference' || item.type === 'kroma_continuation')) {
    return null;
  }
  const text = (message.content ?? '').replace(PRIVATE_USE, '');
  if (!text.trim()) return null;
  if (isUserMessage(message)) return {role: 'user', text};
  if (message.role === 'assistant') return {role: 'assistant', text};
  return null;
}

/** Every message whose visible text contains `query`, oldest first. The
 * comparison is case-insensitive for Latin text and substring-based, so a
 * mixed Chinese/English phrase matches exactly as typed (never a regex). */
export function findInMessages(
  messages: readonly Message[], query: string, offset = 0,
): FindHit[] {
  const term = query.trim().toLowerCase();
  if (!term) return [];
  const hits: FindHit[] = [];
  messages.forEach((message, index) => {
    const visible = findableText(message);
    if (!visible) return;
    if (visible.text.toLowerCase().includes(term)) {
      hits.push({messageIndex: offset + index, role: visible.role});
    }
  });
  return hits;
}

/** Clamp a remembered position onto a recomputed result set. */
export function clampHit(hits: readonly FindHit[], current: number): number {
  if (!hits.length) return -1;
  return Math.min(Math.max(current, 0), hits.length - 1);
}

export function stepHit(hits: readonly FindHit[], current: number, direction: -1 | 1): number {
  if (!hits.length) return -1;
  const start = current < 0 ? (direction > 0 ? -1 : 0) : current;
  return (start + direction + hits.length) % hits.length;
}

/** An honest statement of how much of the conversation was searched. A local
 * search never scans the disk, so it cannot claim the whole history. */
export function findScopeNote(
  loadedMessages: number, hasOlderHistory: boolean, running: boolean,
): string {
  const range = hasOlderHistory
    ? `仅在已加载的 ${loadedMessages} 条消息中查找；更早历史尚未加载`
    : `已检查这段会话的 ${loadedMessages} 条已加载消息`;
  return running ? `${range}。任务进行中，不会触发新的模型或工具调用` : range;
}

export function findCountLabel(hits: readonly FindHit[], current: number): string {
  if (!hits.length) return '0';
  return `${current + 1}/${hits.length}`;
}

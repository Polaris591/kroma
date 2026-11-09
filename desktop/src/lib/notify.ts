import type { Message } from './api';
import type { GoalState } from './goal';

/** One short line of the latest answer, used as a desktop notification body. */
export function latestAnswerLine(messages: Message[]): string {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message.role === 'assistant' && message.content?.trim()) {
      return message.content.trim().split('\n')[0].slice(0, 60);
    }
  }
  return '任务已完成';
}

/** The notification a finished task deserves, or null when it needs none.
 * A cancelled task is the user's own decision and stays silent. */
export function taskNotification(
  status: string, sessionName: string, messages: Message[], goal?: GoalState | null,
): { title: string; body: string } | null {
  if (status === 'cancelled') return null;
  const title = sessionName || 'Kroma';
  if (goal?.status === 'paused') {
    return {title: `${title} · 目标已暂停`, body: '进度已保留，打开窗口查看原因并继续'};
  }
  return status === 'failed'
    ? { title: `${title} · 执行失败`, body: '本轮执行失败，打开窗口查看原因' }
    : { title, body: latestAnswerLine(messages) };
}

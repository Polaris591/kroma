/** Everything the composer needs to decide whether it may send right now. */
export interface ComposerGate {
  ready: boolean;
  busy: boolean;
  running: boolean;
  /** A context compaction owns the runtime; sending would race it. */
  compacting: boolean;
  hasContent: boolean;
}

export const COMPACTING_TIP = '正在压缩上下文，完成后再发送';
export const COMPACTING_PLACEHOLDER = '正在压缩上下文，请稍候…';

/** What pressing send/Enter would do in the current state. */
export type ComposerAction = 'compact-wait' | 'steer' | 'send' | 'blocked';

/** Sending needs a connected, idle host and something to send. */
export function canCompose(state: ComposerGate): boolean {
  return state.ready && !state.busy && !state.running && !state.compacting && state.hasContent;
}

/** One user-visible explanation for the composer, never a silent dead button. */
export function composerAction(state: ComposerGate): ComposerAction {
  if (state.compacting) return 'compact-wait';
  if (state.running) return state.hasContent ? 'steer' : 'blocked';
  return canCompose(state) ? 'send' : 'blocked';
}

export function composerPlaceholder(state: ComposerGate): string {
  if (state.compacting) return COMPACTING_PLACEHOLDER;
  if (state.running) return '向正在运行的任务补充指令…';
  return state.ready ? '告诉 Kroma 你想做什么…' : '连接模型后开始对话';
}

/** The single hint line under the composer. File navigation wins while the
 * suggestion list is open, because Enter means "insert" there, not "send". */
export function composerTip(
  action: ComposerAction, running: boolean, hasFileSuggestions: boolean,
): string {
  if (hasFileSuggestions) return 'Enter 插入 · Esc 关闭 · ↑↓ 切换';
  if (action === 'compact-wait') return COMPACTING_TIP;
  return running ? 'Enter 插入 · Ctrl+Enter 排队' : 'Enter 发送 · Shift+Enter 换行 · @ 引用文件';
}

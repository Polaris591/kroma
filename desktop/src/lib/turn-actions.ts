import { api, type Bootstrap, type TurnDiffSummary } from './api';
import { compactCount } from './format';
import { rewindTargetFor, type DesktopHost } from './desktop-host';

/** Rewind, edit, fork and context compaction. Each one replaces the whole
 * Bootstrap, so they share one busy/restore sequence. */
export function createTurnActions(host: DesktopHost) {
  async function applyTurnChange(
    apply: () => Promise<{view: Bootstrap}>, success: (payload: {saved?: string}) => void,
  ) {
    host.rewinding = true;
    host.busy = true;
    host.clearError();
    try {
      const payload = await apply();
      host.data = payload.view;
      host.resetGoal();
      host.syncHistory();
      host.restoreDraft();
      host.restoreFollowups();
      await host.scrollDown();
      success(payload as {saved?: string});
    } catch (failure) {
      host.showError(failure);
    } finally {
      host.rewinding = false;
      host.busy = false;
    }
  }

  function askRewind(messageIndex: number, snapshotTurn: number | null, diff: TurnDiffSummary | null) {
    if (host.busy || host.running) return;
    host.rewindTarget = rewindTargetFor(messageIndex, snapshotTurn, diff);
  }

  async function confirmRewind(restoreFiles: boolean) {
    const target = host.rewindTarget;
    if (!target || host.rewinding) return;
    host.rewindTarget = null;
    await applyTurnChange(
      () => api.rewindTurn(target.messageIndex, target.snapshotTurn, restoreFiles),
      payload => {
        const saved = payload.saved;
        host.toast(saved ? `已回退 · 之后的对话保存在分支 ${saved.slice(0, 12)}…` : '已回退');
      },
    );
  }

  async function compactNow() {
    if (host.compacting || host.busy || host.running) return;
    host.compacting = true;
    host.clearError();
    try {
      const result = await api.compactContext('');
      host.data = result.view;
      host.syncHistory();
      host.toast(result.compacted
        ? `上下文已压缩 · 约 ${compactCount(result.before)} → ${compactCount(result.after)} tokens`
        : '没有可压缩的已完成历史');
    } catch (failure) {
      host.showError(failure);
    } finally {
      host.compacting = false;
    }
  }

  /** Editing reuses the rewind path: the conversation returns to that prompt
   * and the text is placed back in the composer. Files are left alone, because
   * only the prompt is being reconsidered here. */
  async function editFromTurn(messageIndex: number, text: string) {
    if (host.busy || host.running) return;
    await applyTurnChange(
      () => api.rewindTurn(messageIndex, null, false),
      payload => {
        host.setComposerDraft(text);
        host.toast(payload.saved
          ? `已回到这条提问 · 原先进度保存在分支 ${payload.saved.slice(0, 12)}…`
          : '已回到这条提问');
      },
    );
  }

  async function forkFromTurn(messageIndex: number) {
    if (host.busy || host.running) return;
    await applyTurnChange(
      () => api.forkTurn(messageIndex),
      () => host.toast('已从该轮分叉为新会话'),
    );
  }

  return { askRewind, confirmRewind, compactNow, editFromTurn, forkFromTurn };
}

import { open, save } from '@tauri-apps/plugin-dialog';
import { api, type Bootstrap, type ProfileInput } from './api';
import type { DesktopHost } from './desktop-host';
import { exportFileStem } from './session-export';
import {
  backupFileStem, backupReportMessage, importReportMessage, type ImportedSummary,
} from './session-backup';
import { desktopCommandMessage } from './native-error';
import { applyPresentation, clearSessionPluginUi, loadPresentation } from './plugin-ui';

/** Bootstrap lifecycle and every command that installs a whole Bootstrap or
 * touches model/plugin configuration. All state stays in the shell. */
export function createSessionCommands(host: DesktopHost) {
  async function doConnect(name?: string): Promise<boolean> {
    if (host.busy || host.running) return false;
    host.taskGeneration += 1;
    const generation = host.guard.begin();
    const previousId = host.snapshot?.session_id;
    const previousProject = host.data?.project;
    host.busy = true;
    host.clearError();
    host.clearNotice();
    try {
      const next = await host.guard.settleBootstrap(generation, api.connect(name));
      if (!next) return false;
      host.data = next;
      void loadPresentation();
      if (next.project !== previousProject || next.snapshot?.session_id !== previousId) {
        host.syncHistory();
        host.restoreDraft();
        host.restoreFollowups();
      }
      host.setRunning(false);
      host.clearLiveActivities();
      host.refreshReview();
      host.tab = 'chat';
      await host.scrollIfBottom();
      if (!host.initialized) host.focusComposer();
      return next.ready;
    } catch (failure) { host.showError(failure); return false; }
    finally { if (host.guard.isCurrent(generation)) host.busy = false; }
  }

  async function chooseProject(path?: string) {
    if (host.busy) return;
    host.resetTaskResult();
    // Own the dialog and project switch under one operation token.
    host.busy = true;
    host.clearError();
    const generation = host.guard.begin();
    try {
      const selected = path ?? await open({directory: true, multiple: false, title: '选择工作目录'});
      if (!host.guard.isCurrent(generation) || typeof selected !== 'string' || selected === host.data?.project) return;
      host.saveDraft();
      host.taskGeneration += 1;
      const next = await host.guard.settleBootstrap(generation, api.openProject(selected));
      if (!next) return;
      host.data = next;
      clearSessionPluginUi();
      applyPresentation(null);
      host.expandProject(selected);
      host.clearWorkspaceReview();
      host.resetHistoryWindow();
      host.syncHistory();
      host.clearLiveActivities();
      host.restoreDraft();
      host.restoreFollowups();
      host.refreshReview();
      // Release this owner before the connection operation takes its own token.
      host.busy = false;
      if (host.data.profiles.profiles.length || host.data.plugins.some(plugin => plugin.enabled && plugin.has_model)) {
        await doConnect();
      } else {
        host.tab = 'settings';
      }
      host.collapseSidebarOnNarrow();
    } catch (failure) { host.showError(failure); }
    finally { if (host.guard.isCurrent(generation)) host.busy = false; }
  }

  async function newSession() {
    if (host.busy || host.compacting) return;
    host.resetTaskResult();
    host.saveDraft();
    host.taskGeneration += 1;
    const generation = host.guard.begin();
    host.busy = true;
    host.clearError();
    try {
      const next = await host.guard.settleBootstrap(generation, api.newSession());
      if (!next) return;
      host.data = next;
      clearSessionPluginUi();
      applyPresentation(null);
      void loadPresentation();
      host.syncHistory();
      host.clearLiveActivities();
      host.restoreDraft();
      host.restoreFollowups();
      host.tab = 'chat';
      host.collapseSidebarOnNarrow();
      await host.scrollDown();
      host.focusComposer();
    } catch (failure) { host.showError(failure); }
    finally { if (host.guard.isCurrent(generation)) host.busy = false; }
  }

  async function exportCurrentSession() {
    const id = host.data?.snapshot?.session_id;
    if (!host.data?.ready || !id || id === 'unknown' || host.busy || host.running) return;
    host.busy = true;
    try {
      const destination = await save({
        title: '导出 Kroma 会话为 Markdown',
        defaultPath: exportFileStem(host.sessionName),
        filters: [{name:'Markdown',extensions:['md']}],
      });
      if (!destination) return;
      await api.exportSession(id, destination);
      host.toast('会话已导出；附件仅列信息，不含原文件，也不包含系统提示词、工具日志和模型思考');
    } catch (failure) { host.showError(failure); }
    finally { host.busy = false; }
  }

  /** A complete local backup. It is deliberately a separate entry from the
   * Markdown export: only this one can be restored. */
  async function backupCurrentSession() {
    const id = host.data?.snapshot?.session_id;
    if (!host.data?.ready || !id || id === 'unknown' || host.busy || host.running) return;
    host.busy = true;
    host.clearError();
    try {
      const destination = await save({
        title: '备份完整 Kroma 会话',
        defaultPath: backupFileStem(host.sessionName),
        filters: [{name: 'Kroma 会话备份', extensions: ['kroma-backup']}],
      });
      if (!destination) return;
      const report = await api.exportSessionBackup(id, destination);
      host.toast(backupReportMessage(report));
    } catch (failure) {
      host.showError(new Error(desktopCommandMessage('会话备份', failure)));
    } finally { host.busy = false; }
  }

  /** Restore a backup into the current workspace as a new conversation. */
  async function importSessionBackup() {
    if (host.running || host.busy || host.compacting || !host.data) return;
    host.clearError();
    const generation = host.guard.begin();
    host.busy = true;
    try {
      const source = await open({
        multiple: false,
        title: '选择 Kroma 会话备份',
        filters: [{name: 'Kroma 会话备份', extensions: ['kroma-backup']}],
      });
      if (typeof source !== 'string' || !source) return;
      const result = await api.importSessionBackup(source);
      if (!host.guard.isCurrent(generation)) return;
      const imported: ImportedSummary | undefined = result.imported;
      const warning = result.open_warning ?? '';
      const next: Bootstrap = result;
      host.data = next;
      host.taskGeneration += 1;
      clearSessionPluginUi();
      applyPresentation(null);
      void loadPresentation();
      host.syncHistory();
      host.clearLiveActivities();
      host.restoreDraft();
      host.restoreFollowups();
      host.tab = 'chat';
      await host.scrollDown();
      host.focusComposer();
      host.toast(imported
        ? importReportMessage(imported) + (warning ? `；${warning}` : '')
        : (warning || '已导入会话备份'));
    } catch (failure) {
      host.showError(new Error(desktopCommandMessage('导入会话备份', failure)));
    } finally {
      if (host.guard.isCurrent(generation)) host.busy = false;
    }
  }

  async function saveTitle(title: string) {
    const target = host.data?.snapshot?.session_id;
    const trimmed = title.trim();
    if (host.busy || host.running || !target || !trimmed || target === 'unknown') return;
    // Lock navigation while the mutation is in flight; the backend also
    // verifies the exact target ID to reject stale or out-of-order requests.
    const generation = host.guard.begin();
    host.busy = true;
    host.clearError();
    try {
      const next = await host.guard.settleBootstrap(
        generation, api.renameSession(target, trimmed)
      );
      if (!next) return;
      host.data = next;
      host.endRename();
    } catch (failure) { host.showError(failure); }
    finally { if (host.guard.isCurrent(generation)) host.busy = false; }
  }

  async function undoDelete(targets: Array<{project: string; id: string; title: string}>) {
    if (!targets.length || host.busy || host.running) return;
    host.busy = true;
    let restored = 0;
    let undoFailures = 0;
    try {
      for (const target of targets) {
        try {
          await api.restoreDeletedSession(target.project, target.id, target.title);
          restored += 1;
        } catch {
          undoFailures += 1;
        }
      }
      host.clearDeletedSessions();
      try { host.data = await api.bootstrap(); } catch { /* keep the current view */ }
      host.toast(undoFailures
        ? `已恢复 ${restored} 个会话，${undoFailures} 个失败`
        : `已恢复 ${restored} 个会话`);
    } finally {
      host.busy = false;
    }
  }

  async function refresh() {
    if (!host.initialized || host.busy || host.running) return;
    const generation = host.guard.begin();
    const previousId = host.snapshot?.session_id;
    try {
      const next = await host.guard.settleBootstrap(generation, api.bootstrap());
      if (!next) return;
      host.data = next;
      if (previousId !== next.snapshot?.session_id) {
        host.syncHistory();
        host.restoreFollowups();
      }
      void host.scrollIfBottom();
    } catch (failure) {
      if (host.guard.isCurrent(generation)) host.showError(failure);
    }
  }

  async function initialize() {
    const generation = host.guard.begin();
    try {
      const next = await host.guard.settleBootstrap(generation, api.bootstrap());
      if (!next) return;
      host.data = next;
      host.syncHistory();
      void loadPresentation();
      host.restoreFollowups();
      if (host.data.profiles.profiles.length || host.data.plugins.some(plugin => plugin.enabled && plugin.has_model)) {
        await doConnect();
      } else host.tab = 'settings';
    } catch (failure) { host.showError(failure); }
    finally { host.initialized = true; }
  }

  async function changeModel(name: string) {
    if (!name || name === host.connectedProfile || host.busy || host.running) return;
    await doConnect(name);
  }

  async function changeReasoning(level: string) {
    if (host.busy || host.running || !host.data?.ready || level === host.reasoningLevel) return;
    const generation = host.guard.begin();
    host.busy = true;
    host.clearError();
    try {
      const next = await host.guard.settleBootstrap(generation, api.setReasoning(level));
      if (next) host.data = next;
    } catch (failure) { host.showError(failure); }
    finally { if (host.guard.isCurrent(generation)) host.busy = false; }
  }

  async function saveProfile(profile: ProfileInput) {
    try {
      const profiles = await api.saveProfile(profile);
      if (host.data) host.data = {...host.data, profiles};
      host.toast('配置已保存');
    } catch (failure) { host.showError(failure); throw failure; }
  }

  async function addAndConnectModel(profile: ProfileInput) {
    if (host.running || host.busy) throw new Error('请在当前任务结束后添加模型');
    await saveProfile(profile);
    const connected = await doConnect(profile.name);
    if (!connected || host.data?.connected_profile !== profile.name) {
      throw new Error('配置已保存，但连接未成功。请检查接口地址、模型 ID 和密钥。');
    }
  }

  async function setDefaultProfile(name: string): Promise<boolean> {
    if (!host.data || host.running || host.busy) return false;
    host.busy = true;
    try {
      const profiles = await api.setDefaultProfile(name);
      host.data = {...host.data, profiles};
      host.toast('已设置 CLI / TUI / Desktop 的默认模型');
      return true;
    } catch (failure) { host.showError(failure); return false; }
    finally { host.busy = false; }
  }

  async function pluginAction(action: string, id?: string, path?: string, key?: string, value?: unknown) {
    host.clearError();
    try {
      const result = await api.pluginAction(action, id, path, key, value);
      if (host.data) host.data = {...host.data, plugins: result.plugins};
      // Installing or enabling a plugin changes what it declares, so the
      // interface reloads that declaration with the same action.
      await loadPresentation();
      host.toast(result.restart_required ? (result.warning || '需要重新连接模型后生效') : result.message);
    } catch (failure) { host.showError(failure); throw failure; }
  }

  async function installPlugin() {
    try {
      const dir = await open({directory: true, title: '选择包含 plugin.toml 的插件文件夹'});
      if (typeof dir === 'string') await pluginAction('install', undefined, dir);
    } catch (failure) { host.showError(failure); throw failure; }
  }

  return {
    doConnect, chooseProject, newSession, exportCurrentSession,
    backupCurrentSession, importSessionBackup,
    saveTitle, undoDelete, refresh, initialize,
    changeModel, changeReasoning, addAndConnectModel, setDefaultProfile,
    saveProfile, pluginAction, installPlugin,
  };
}

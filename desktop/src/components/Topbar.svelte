<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { popover } from '../lib/popover';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { Archive, ArchiveRestore, Copy, Download, Minus, MoreHorizontal, PanelLeft, Puzzle, Settings2, SlidersHorizontal, Square, X } from '@lucide/svelte';
  import type { ProfileSummary } from '../lib/api';
  import type { Tab } from '../lib/ui-types';
  import ModelPicker from './ModelPicker.svelte';

  import { desktopSurfaces } from '../lib/plugin-ui';
  export let tab: Tab = 'chat';
  export let currentSessionName = '';

  export let hasSession = false;
  export let rename = false;
  export let newTitle = '';
  export let sidebarVisible = true;
  export let busy = false;
  export let running = false;
  export let ready = false;
  export let profiles: ProfileSummary[] = [];
  export let connectedProfile = '';
  export let modelName = '';
  export let reasoningLevel = 'auto';
  export let reasoningLevels: string[] = [];
  export let thinkingEnabled = false;
  export let onToggleSidebar: () => void;
  export let onStartRename: () => void;
  export let onNewTitleChange: (value: string) => void;
  export let onSaveTitle: () => void;
  export let onCancelRename: () => void;
  export let onWindowError: (failure: unknown) => void;
  export let onExport: () => void;
  export let onBackup: () => void;
  export let onImportBackup: () => void;
  export let onSelectModel: (name: string) => Promise<void>;
  export let onAddModel: () => void;
  export let onManageModels: () => void;
  export let onSessionSettings: () => void;
  export let onSelectReasoning: (level: string) => Promise<void>;

  // The window draws its own title bar, so these three buttons are the system
  // chrome Windows no longer provides. A browser preview has no window to drive.
  const appWindow = getCurrentWindow();
  let maximized = false;
  let titleInput: HTMLInputElement;
  let actionsOpen = false;
  let actionsTrigger: HTMLButtonElement;
  $: if (rename) void focusTitle();
  $: if (busy || running || tab !== 'chat') actionsOpen = false;

  async function focusTitle() {
    await tick();
    if (rename) { titleInput?.focus(); titleInput?.select(); }
  }

  function runAction(action: () => void) {
    actionsOpen = false;
    actionsTrigger?.focus();
    action();
  }

  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    async function syncMaximized() {
      const next = await appWindow.isMaximized();
      if (!disposed) maximized = next;
    }
    void (async () => {
      try {
        await syncMaximized();
        if (disposed) return;
        const stop = await appWindow.onResized(() => {
          void syncMaximized().catch(reportWindowFailure);
        });
        if (disposed) stop();
        else unlisten = stop;
      } catch { /* browser preview has no native window */ }
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  function minimizeWindow() { void appWindow.minimize().catch(reportWindowFailure); }
  function toggleMaximize() { void appWindow.toggleMaximize().catch(reportWindowFailure); }
  function closeWindow() { void appWindow.close().catch(reportWindowFailure); }
  /// A refused window command must not look like a dead button.
  function reportWindowFailure(failure: unknown) {
    onWindowError(failure);
  }
</script>

<header class="topbar" data-tauri-drag-region="deep">
  <div class="breadcrumb">
    <button class="sidebar-toggle icon-button" aria-label={sidebarVisible ? '收起侧边栏' : '展开侧边栏'}
      title={sidebarVisible ? '收起侧边栏 · Ctrl+B' : '展开侧边栏 · Ctrl+B'}
      onclick={onToggleSidebar}>
      <PanelLeft size={17}/>
    </button>
    {#if tab === 'chat'}
      {#if rename}
        <input class="rename-input" value={newTitle} bind:this={titleInput} aria-label="会话名称" maxlength="80"
          oninput={(event) => onNewTitleChange(event.currentTarget.value)}
          onkeydown={(event) => event.key === 'Enter' && !event.isComposing && onSaveTitle()} />
        <button class="small secondary" disabled={busy} onclick={onSaveTitle}>完成</button>
        <button class="icon-button" aria-label="取消重命名" onclick={onCancelRename}><X size={15}/></button>
      {:else}
        <button class="title-button" title="点击重命名会话" onclick={onStartRename} disabled={!hasSession}>
          {currentSessionName}
        </button>
        <div class="session-actions" data-tauri-drag-region="false">
          <button class="icon-button" aria-label="会话操作" title="导出、备份与导入"
            aria-expanded={actionsOpen} aria-haspopup="dialog" bind:this={actionsTrigger}
            disabled={busy||running} onclick={() => actionsOpen = !actionsOpen}><MoreHorizontal size={17}/></button>
          {#if actionsOpen}
            <div class="session-actions-menu" role="dialog" aria-label="会话操作" tabindex="-1"
              use:popover={{trigger: actionsTrigger, onClose: () => actionsOpen = false}}>
              {#if hasSession}
                <button onclick={() => runAction(onExport)} aria-label="导出会话"><Download size={16}/>
                  <span>导出 Markdown<small>可读文字，不含附件原文件</small></span></button>
                <button onclick={() => runAction(onBackup)} aria-label="备份会话"><Archive size={16}/>
                  <span>备份完整会话<small>包含附件，可导入恢复</small></span></button>
              {/if}
              <button onclick={() => runAction(onImportBackup)} aria-label="导入会话备份"><ArchiveRestore size={16}/>
                <span>导入会话备份<small>恢复成新会话，不覆盖现有记录</small></span></button>
            </div>
          {/if}
        </div>
      {/if}
    {:else if tab === 'settings'}<Settings2 size={16}/> 设置
    {:else}{@const pluginTabId = tab.slice('plugin:'.length)}{@const surface = $desktopSurfaces.find(s => s.plugin_id === pluginTabId)}<Puzzle size={16}/> {surface?.title || pluginTabId}{/if}
  </div>
  <div class="topbar-right">
    {#if tab === 'chat'}
      <button class="session-settings-button" title="当前会话启用的能力" aria-label="会话设置"
        disabled={!ready || busy || running} onclick={onSessionSettings}><SlidersHorizontal size={15}/><span>会话设置</span></button>
      <ModelPicker {profiles} connected={connectedProfile}
        currentModel={modelName} disabled={busy||running} onSelect={onSelectModel}
        onAdd={onAddModel} onManage={onManageModels}
        {reasoningLevel} {reasoningLevels} reasoningDisabled={!ready||busy||running||!thinkingEnabled}
        {onSelectReasoning}/>
    {/if}
  </div>
  <div class="window-controls">
    <button class="window-control" aria-label="最小化" title="最小化" onclick={minimizeWindow}><Minus size={15}/></button>
    <button class="window-control" aria-label={maximized ? '向下还原' : '最大化'}
      title={maximized ? '向下还原' : '最大化'} onclick={toggleMaximize}>
      {#if maximized}<Copy size={13}/>{:else}<Square size={12}/>{/if}
    </button>
    <button class="window-control close" aria-label="关闭窗口" title="关闭窗口" onclick={closeWindow}><X size={15}/></button>
  </div>
</header>

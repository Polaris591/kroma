<script lang="ts">
  import { Download, PanelLeft, Puzzle, Settings2 } from '@lucide/svelte';
  import type { ProfileSummary } from '../lib/api';
  import type { Tab } from '../lib/ui-types';
  import ModelPicker from './ModelPicker.svelte';
  import ThinkingPicker from './ThinkingPicker.svelte';

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
  export let onExport: () => void;
  export let onSelectModel: (name: string) => Promise<void>;
  export let onAddModel: () => void;
  export let onManageModels: () => void;
  export let onSelectReasoning: (level: string) => Promise<void>;
</script>

<header class="topbar">
  <div class="breadcrumb">
    <button class="sidebar-toggle icon-button" aria-label={sidebarVisible ? '收起侧边栏' : '展开侧边栏'}
      title={sidebarVisible ? '收起侧边栏 · Ctrl+B' : '展开侧边栏 · Ctrl+B'}
      onclick={onToggleSidebar}>
      <PanelLeft size={17}/>
    </button>
    {#if tab === 'chat'}
      {#if rename}
        <input class="rename-input" value={newTitle}
          oninput={(event) => onNewTitleChange(event.currentTarget.value)}
          onkeydown={(event) => event.key === 'Enter' && onSaveTitle()} />
        <button class="small secondary" onclick={onSaveTitle}>完成</button>
      {:else}
        <button class="title-button" title="点击重命名会话" onclick={onStartRename} disabled={!hasSession}>
          {currentSessionName}
        </button>
        {#if hasSession}
          <button class="title-export icon-button" title="导出完整会话 Markdown"
            aria-label="导出会话" disabled={busy||running}
            onclick={onExport}><Download size={15}/></button>
        {/if}
      {/if}
    {:else if tab === 'settings'}<Settings2 size={16}/> 模型与设置
    {:else}<Puzzle size={16}/> 插件管理{/if}
  </div>
  <div class="topbar-right">
    {#if tab === 'chat'}
      <ModelPicker {profiles} connected={connectedProfile}
        currentModel={modelName} disabled={busy||running} onSelect={onSelectModel}
        onAdd={onAddModel} onManage={onManageModels}/>
      <ThinkingPicker value={reasoningLevel} levels={reasoningLevels}
        disabled={!ready||busy||running||!thinkingEnabled} onSelect={onSelectReasoning}/>
    {/if}
  </div>
</header>

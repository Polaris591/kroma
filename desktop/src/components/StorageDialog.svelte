<script lang="ts">
  import { HardDrive } from '@lucide/svelte';
  import { workspaceDisplayName } from '../lib/chat-ui';
  export let rows: Array<{project: string; sessions: number; empty: number; bytes: number; empty_ids: string[]}> = [];
  export let busy = false;
  export let running = false;
  export let userHome: string | null | undefined = null;
  export let onClose: () => void;
  export let onPurge: () => void;
</script>

<div class="dialog-overlay" role="presentation">
  <div class="dialog-dim"></div>
  <div class="dialog-panel abilities-panel" role="dialog" aria-modal="true" aria-labelledby="storage-heading">
    <div class="dialog-heading">
      <span class="dialog-emblem"><HardDrive size={18}/></span>
      <div><h2 id="storage-heading">本机数据</h2><p>只统计本机会话文件，不上传任何内容</p></div>
    </div>
    {#if busy}
      <p class="dialog-description">正在统计…</p>
    {:else}
      <div class="storage-table">
        {#each rows as row (row.project)}
          <div class="storage-row">
            <span class="storage-name" title={row.project}>{workspaceDisplayName(row.project, userHome)}</span>
            <span class="storage-num">{row.sessions} 个会话</span>
            <span class="storage-num" class:warn={row.empty > 0}>{row.empty} 个空会话</span>
            <span class="storage-size">{(row.bytes / 1048576).toFixed(1)} MB</span>
          </div>
        {/each}
      </div>
      <p class="dialog-description">空会话指没有任何用户消息的对话（多是点「新建会话」后没用过的）。清理后仍可从回收目录恢复。</p>
    {/if}
    <div class="dialog-actions">
      <button class="dialog-secondary" onclick={onClose}>关闭</button>
      <button class="dialog-danger" disabled={busy || running || !rows.some(row => row.empty)}
        onclick={onPurge}>
        清理全部空会话
      </button>
    </div>
  </div>
</div>

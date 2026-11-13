<script lang="ts">
  import { modal } from '../lib/modal';
  import { Undo2 } from '@lucide/svelte';
  export let files = 0;
  export let added = 0;
  export let removed = 0;
  export let busy = false;
  export let error = '';
  let attempted = false;
  function confirm(restoreFiles: boolean) {
    attempted = true;
    onConfirm(restoreFiles);
  }
  export let onCancel: () => void;
  export let onConfirm: (restoreFiles: boolean) => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="rewind-heading"
  use:modal={{ onClose: onCancel, busy, initialFocus: '.dialog-secondary' }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel">
    <div class="dialog-heading">
      <span class="dialog-emblem"><Undo2 size={19}/></span>
      <div><h2 id="rewind-heading">回到这一轮之前</h2><p>之后的对话会移出本会话</p></div>
    </div>
    <p class="dialog-description">
      被移出的内容会原样保存为一个新分支，随时可以切回去，不会丢失。
      {#if files}
        <br/>本轮改动 <strong>{files} 个文件（+{added} −{removed}）</strong>，
        还原会把它们恢复到该任务开始前的状态。
      {:else}
        <br/>这一轮没有文件改动，只有对话会被回退。
      {/if}
    </p>
    {#if attempted && !busy && error}<p class="dialog-error" role="alert">{error}</p>{/if}
    <div class="dialog-actions">
      <button class="dialog-secondary" disabled={busy} onclick={onCancel}>取消</button>
      <button class="dialog-secondary" disabled={busy} onclick={() => confirm(false)}>{busy ? '正在回退…' : '只回退对话'}</button>
      {#if files}
        <button class="dialog-primary" disabled={busy} onclick={() => confirm(true)}>
          {busy ? '正在回退…' : '回退并还原文件'}
        </button>
      {/if}
    </div>
  </div>
</dialog>

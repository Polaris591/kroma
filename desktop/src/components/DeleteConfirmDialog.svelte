<script lang="ts">
  import { onMount } from 'svelte';
  import { Trash2 } from '@lucide/svelte';
  export let title = '';
  export let busy = false;
  export let onCancel: () => void;
  export let onConfirm: () => void;
  let cancelButton: HTMLButtonElement;
  // Destructive actions start on the safe choice, never on delete.
  onMount(() => cancelButton?.focus());
</script>

<div class="dialog-overlay" role="presentation">
  <div class="dialog-dim"></div>
  <div class="dialog-panel delete-confirm" role="dialog" aria-modal="true" aria-labelledby="delete-heading">
    <div class="dialog-heading"><span class="dialog-emblem danger"><Trash2 size={19}/></span>
      <div><h2 id="delete-heading">删除此会话？</h2><p>{title}</p></div>
    </div>
    <p class="dialog-description">该会话将移入本地回收目录，不会删除工作目录中的代码文件。其他会话不受影响。</p>
    <div class="dialog-actions">
      <button class="dialog-secondary" bind:this={cancelButton} disabled={busy} onclick={onCancel}>取消</button>
      <button class="dialog-danger" disabled={busy} onclick={onConfirm}>
        {busy ? '正在删除…' : '删除会话'}
      </button>
    </div>
  </div>
</div>

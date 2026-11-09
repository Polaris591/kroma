<script lang="ts">
  import { AlertCircle, X } from '@lucide/svelte';
  export let error = '';
  export let notice = '';
  export let undoAvailable = false;
  export let busy = false;
  export let running = false;
  export let onDismissError: () => void;
  export let onDismissNotice: () => void;
  export let onUndo: () => void;
</script>

{#if error}
  <div class="banner error-banner" role="alert"><AlertCircle size={16}/><span>{error}</span><button onclick={onDismissError} title="关闭"><X size={16}/></button></div>
{:else if notice}
  <div class="banner notice-banner" role="status">
    <span>{notice}</span>
    {#if undoAvailable}
      <button class="notice-undo" disabled={busy||running} onclick={onUndo}>
        撤销删除
      </button>
    {/if}
    <button aria-label="关闭提示" onclick={onDismissNotice}><X size={15}/></button>
  </div>
{/if}

<script lang="ts">
  import { modal } from '../lib/modal';
  import { Trash2 } from '@lucide/svelte';
  export let title = '';
  export let heading = '删除此会话？';
  export let description = '该会话将移入本地回收目录，不会删除工作目录中的代码文件。其他会话不受影响。';
  export let confirmLabel = '删除会话';
  export let busy = false;
  export let error = '';
  export let onCancel: () => void;
  export let onConfirm: () => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="delete-heading"
  use:modal={{ onClose: onCancel, busy, initialFocus: '.dialog-secondary' }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel delete-confirm">
    <div class="dialog-heading"><span class="dialog-emblem danger"><Trash2 size={19}/></span>
      <div><h2 id="delete-heading">{heading}</h2><p>{title}</p></div>
    </div>
    <p class="dialog-description">{description}</p>
    {#if error}<p class="dialog-error" role="alert">{error}</p>{/if}
    <div class="dialog-actions">
      <button class="dialog-secondary" disabled={busy} onclick={onCancel}>取消</button>
      <button class="dialog-danger" disabled={busy} onclick={onConfirm}>
        {busy ? '正在删除…' : confirmLabel}
      </button>
    </div>
  </div>
</dialog>

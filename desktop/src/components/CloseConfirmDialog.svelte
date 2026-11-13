<script lang="ts">
  import { TriangleAlert } from '@lucide/svelte';
  import { modal } from '../lib/modal';

  export let closing = false;
  export let onCancel: () => void;
  export let onConfirm: () => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="close-confirm-heading"
  use:modal={{ onClose: onCancel, busy: closing, initialFocus: '.dialog-secondary' }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel close-confirm">
    <div class="dialog-heading"><span class="dialog-emblem danger"><TriangleAlert size={19}/></span>
      <div>
        <h2 id="close-confirm-heading">任务仍在执行</h2>
        <p>关闭窗口会停止这次任务</p>
      </div>
    </div>
    <p class="dialog-description">已经完成的工具调用和文件修改不会撤销，尚未完成的步骤会被中断。关掉的只是窗口，会话记录仍然保留。</p>
    <div class="dialog-actions">
      <button class="dialog-secondary" disabled={closing} onclick={onCancel}>继续执行</button>
      <button class="dialog-danger" disabled={closing} onclick={onConfirm}>
        {closing ? '正在关闭…' : '关闭并中断'}
      </button>
    </div>
  </div>
</dialog>

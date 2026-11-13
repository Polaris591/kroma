<script lang="ts">
  import { Archive, ShieldAlert } from '@lucide/svelte';
  import { modal } from '../lib/modal';
  import { BACKUP_WARNING } from '../lib/session-backup';

  export let busy = false;
  export let onCancel: () => void;
  export let onConfirm: () => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="backup-export-heading"
  use:modal={{ onClose: onCancel, busy }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel backup-import">
    <div class="dialog-heading"><span class="dialog-emblem"><ShieldAlert size={19}/></span>
      <div>
        <h2 id="backup-export-heading">备份完整会话</h2>
        <p>与 Markdown 导出不同，备份可以恢复</p>
      </div>
    </div>
    <p class="dialog-description">{BACKUP_WARNING}</p>
    <p class="dialog-description">Markdown 导出只包含可读文字和附件名称；这里的备份包含恢复所需的全部记录，请按敏感文件保存。</p>
    <div class="dialog-actions">
      <button class="dialog-secondary" disabled={busy} onclick={onCancel}>取消</button>
      <button class="primary" disabled={busy} onclick={onConfirm}>
        <Archive size={15}/>{busy ? '正在备份…' : '继续备份…'}
      </button>
    </div>
  </div>
</dialog>

<script lang="ts">
  import { ArchiveRestore, TriangleAlert } from '@lucide/svelte';
  import { modal } from '../lib/modal';
  import { BACKUP_IMPORT_WARNING } from '../lib/session-backup';

  export let busy = false;
  export let onCancel: () => void;
  export let onConfirm: () => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="backup-import-heading"
  use:modal={{ onClose: onCancel, busy }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel backup-import">
    <div class="dialog-heading"><span class="dialog-emblem"><TriangleAlert size={19}/></span>
      <div>
        <h2 id="backup-import-heading">导入 Kroma 会话备份</h2>
        <p>备份会恢复成当前工作区里的新会话</p>
      </div>
    </div>
    <p class="dialog-description">{BACKUP_IMPORT_WARNING}</p>
    <div class="dialog-actions">
      <button class="dialog-secondary" disabled={busy} onclick={onCancel}>取消</button>
      <button class="primary" disabled={busy} onclick={onConfirm}>
        <ArchiveRestore size={15}/>{busy ? '正在导入…' : '选择备份文件…'}
      </button>
    </div>
  </div>
</dialog>

<script lang="ts">
  import { X } from '@lucide/svelte';
  import { modal } from '../lib/modal';
  import { attachmentName, previewErrorMessage } from '../lib/attachments';
  import type { AttachmentOrigin } from '../lib/api';
  import AttachmentImage from './AttachmentImage.svelte';

  export let origin: AttachmentOrigin;
  export let path: string;
  export let name = '';
  export let onClose: () => void;

  let error = '';
  // Bumped by 重试 to remount the image and run one new bounded read.
  let attempt = 0;
  $: label = name || attachmentName(path);

  function retry() {
    error = '';
    attempt += 1;
  }
</script>

<dialog class="panel-overlay" aria-label={`图片预览：${label}`} use:modal={{ onClose }}>
  <button class="panel-scrim" tabindex="-1" aria-hidden="true" onclick={onClose}></button>
  <div class="panel-card attachment-preview-card">
    <div class="panel-head">
      <h3>{label}</h3>
      <button class="icon-button" aria-label="关闭图片预览" onclick={onClose}><X size={16}/></button>
    </div>
    {#if error}
      <div class="attachment-preview-error" role="alert">
        <p>{error}</p>
        <button class="quiet-action" onclick={retry}>重试</button>
      </div>
    {:else}
      <div class="attachment-preview-body">
        {#key attempt}
          <AttachmentImage {origin} {path} alt={label} className="attachment-preview-image"
            onError={(failure) => error = previewErrorMessage(failure)}/>
        {/key}
      </div>
    {/if}
  </div>
</dialog>

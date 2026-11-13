<script lang="ts">
  import { onDestroy } from 'svelte';
  import { api, type AttachmentOrigin } from '../lib/api';
  import { imageMime } from '../lib/attachments';

  export let path = '';
  export let origin: AttachmentOrigin = 'draft';
  export let alt = '';
  export let className = '';
  /** Called once per failed load. The parent decides whether that is a visible
   * error (the preview dialog) or a silent fallback (a draft chip). */
  export let onError: ((failure: unknown) => void) | null = null;

  let url = '';
  let failed = false;
  // Replace and unmount both retire a read still in flight, so a late answer
  // can neither revive an old image nor leak its Blob URL.
  let request = 0;
  let retired = false;
  let shown = '';

  $: if (path && `${origin}\n${path}` !== shown) void load();

  function release() {
    if (url) URL.revokeObjectURL(url);
    url = '';
  }

  /** The backend verifies the magic, not the pixels: a truncated PNG passes
   * the read and then fails to decode. Report it once against the URL that is
   * still current, release it, and never retry on its own. */
  function decodeFailed(broken: string) {
    if (retired || broken !== url) return;
    release();
    failed = true;
    onError?.(new Error('图片内容损坏或无法解码'));
  }

  async function load() {
    const current = ++request;
    release();
    failed = false;
    shown = `${origin}\n${path}`;
    try {
      const bytes = await api.previewImage(origin, path);
      const mime = imageMime(bytes);
      if (!mime) throw new Error('只支持 PNG/JPEG/WebP 图片预览');
      if (retired || current !== request) return;
      url = URL.createObjectURL(new Blob([bytes], { type: mime }));
    } catch (failure) {
      if (retired || current !== request) return;
      failed = true;
      onError?.(failure);
    }
  }

  onDestroy(() => {
    retired = true;
    request += 1;
    release();
  });
</script>

{#if url}
  <img class={className} src={url} {alt} onerror={(event) => decodeFailed((event.currentTarget as HTMLImageElement).src)}/>
{:else if !failed}
  <span class={className} aria-hidden="true"></span>
{:else}
  <slot></slot>
{/if}

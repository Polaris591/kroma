<script lang="ts">
  import DOMPurify from 'dompurify';
  import { marked } from 'marked';
  import { invoke } from '@tauri-apps/api/core';
  export let content = '';
  marked.setOptions({ gfm: true, breaks: false });
  $: html = DOMPurify.sanitize(marked.parse(content || '') as string, {
    USE_PROFILES: { html: true }
  });
  function followLink(event: MouseEvent) {
    const target = event.target as Element | null;
    const anchor = target?.closest?.('a');
    if (!anchor) return;
    // Never let model-authored links navigate the app WebView. Only explicit
    // HTTP(S) links are handed to the operating system browser.
    event.preventDefault();
    const href = anchor.getAttribute('href') || '';
    if (/^https?:\/\//i.test(href)) void invoke('open_external', { url: href });
  }
</script>

<!-- The click handler only intercepts links inside rendered Markdown; it is
     not a replacement for keyboard navigation of an interactive control. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="markdown" role="document" onclick={followLink}>{@html html}</div>

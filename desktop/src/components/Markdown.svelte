<script lang="ts">
  import DOMPurify from 'dompurify';
  import { marked } from 'marked';
  import { invoke } from '@tauri-apps/api/core';
  import { decorateCitations } from '../lib/citations';
  export let content = '';
  let linkError = '';
  marked.setOptions({ gfm: true, breaks: false });

  // Citation chips are applied to sanitized HTML before it is inserted, so
  // the component never has to post-process the live DOM.
  function render(source: string): string {
    const clean = DOMPurify.sanitize(marked.parse(source || '') as string, {
      USE_PROFILES: { html: true }
    });
    if (!clean) return '';
    const holder = document.createElement('div');
    holder.innerHTML = clean;
    decorateCitations(holder);
    // The link is never opened in place, but the referrer must not leak if a
    // platform decides to handle it anyway.
    for (const anchor of holder.querySelectorAll('a[href]')) {
      anchor.setAttribute('rel', 'noreferrer noopener');
    }
    return holder.innerHTML;
  }

  $: html = render(content);

  function followLink(event: MouseEvent) {
    const target = event.target as Element | null;
    const anchor = target?.closest?.('a');
    if (!anchor) return;
    // Never let model-authored links navigate the app WebView. Only explicit
    // HTTP(S) links are handed to the operating system browser.
    event.preventDefault();
    const href = anchor.getAttribute('href') || '';
    if (!/^https?:\/\//i.test(href)) return;
    // A silent failure would look like a dead link; say what to do instead.
    void invoke('open_external', { url: href })
      .then(() => { linkError = ''; })
      .catch(() => { linkError = '无法打开此链接，请手动复制到浏览器'; });
  }
</script>

<!-- The click handlers only intercept links inside rendered Markdown; they are
     not a replacement for keyboard navigation of an interactive control. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="markdown" role="document" onclick={followLink} onauxclick={followLink}>{@html html}</div>
{#if linkError}
  <p class="link-error" role="alert">{linkError}</p>
{/if}

<script lang="ts">
  import { afterUpdate } from 'svelte';
  import DOMPurify from 'dompurify';
  import { marked } from 'marked';
  import { invoke } from '@tauri-apps/api/core';
  import { decorateCitations } from '../lib/citations';
  export let content = '';
  let linkError = '';
  let host: HTMLDivElement;
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

  // The copy control belongs to the host, not the model's Markdown, so it is
  // attached to the sanitized result with DOM APIs. Each control captures its
  // own code node, so a copy reads and reports only for that block.
  async function copyCode(button: HTMLButtonElement, code: Element, label: HTMLElement) {
    try {
      await navigator.clipboard.writeText(code.textContent ?? '');
      // A re-render replaces the block's nodes; a late receipt must not speak
      // for the control the person now sees.
      if (button.isConnected && code.isConnected) label.textContent = '已复制';
    } catch {
      if (button.isConnected && code.isConnected) label.textContent = '复制失败，重试';
    }
  }

  // The fence language marked wrote as `language-<name>`. It is only shown as
  // plain text and never guessed: an undeclared block stays unlabelled.
  function codeLanguage(code: Element): string {
    for (const name of Array.from(code.classList)) {
      if (name.startsWith('language-')) return name.slice('language-'.length).trim();
    }
    return '';
  }

  function addCopyControls(root: HTMLElement) {
    for (const pre of root.querySelectorAll('pre')) {
      const code = pre.firstElementChild;
      if (!code || code.tagName !== 'CODE') continue;
      // Re-rendering already replaces the whole block; never add a second one.
      if (pre.parentElement?.classList.contains('code-block')) continue;
      const block = document.createElement('div');
      block.className = 'code-block';
      pre.replaceWith(block);
      const bar = document.createElement('div');
      bar.className = 'code-block-bar';
      const language = codeLanguage(code);
      pre.tabIndex = 0;
      pre.setAttribute('role', 'region');
      pre.setAttribute('aria-label', language ? `${language} 代码块` : '代码块');
      if (language) {
        const tag = document.createElement('span');
        tag.className = 'code-lang';
        tag.textContent = language;
        tag.title = language;
        bar.append(tag);
      }
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'code-copy';
      button.title = '复制这段代码';
      const label = document.createElement('span');
      label.setAttribute('aria-live', 'polite');
      label.textContent = '复制代码';
      button.append(label);
      button.addEventListener('click', () => { void copyCode(button, code, label); });
      bar.append(button);
      block.append(bar, pre);
    }
  }

  afterUpdate(() => addCopyControls(host));

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
<div class="markdown" role="document" bind:this={host} onclick={followLink} onauxclick={followLink}>{@html html}</div>
{#if linkError}
  <p class="link-error" role="alert">{linkError}</p>
{/if}

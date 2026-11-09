<script lang="ts">
  import { onDestroy } from 'svelte';
  import Markdown from './Markdown.svelte';
  import { markdownRefreshDelay } from '../lib/streaming';

  export let content = '';

  let displayed = '';
  let pending: ReturnType<typeof setTimeout> | null = null;

  // Keep the text stream immediate in the Agent state, but don't repeatedly
  // parse and sanitize an increasingly large Markdown document for every
  // token. A scheduled flush always uses the latest text rather than
  // queuing stale intermediate versions.
  $: if (content !== displayed && pending === null) {
    pending = setTimeout(() => {
      displayed = content;
      pending = null;
    }, markdownRefreshDelay(content.length));
  }

  onDestroy(() => {
    if (pending !== null) clearTimeout(pending);
  });
</script>

<Markdown content={displayed}/>

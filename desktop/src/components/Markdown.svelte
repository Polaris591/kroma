<script lang="ts">
  import DOMPurify from 'dompurify';
  import { marked } from 'marked';
  export let content = '';
  marked.setOptions({ gfm: true, breaks: false });
  $: html = DOMPurify.sanitize(marked.parse(content || '') as string, {
    USE_PROFILES: { html: true }
  });
</script>

<div class="markdown">{@html html}</div>

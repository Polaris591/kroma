<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { api, type DesktopBundle, type DesktopSurface } from '../lib/api';
  import { desktopBusMessage, pluginColors } from '../lib/plugin-ui';

  export let surface: DesktopSurface;
  export let sessionId = '';
  export let project = '';
  export let fullHeight = false;
  export let onRunTask: (text: string) => void;
  export let onSetComposer: (text: string, mode: string) => void;
  export let onNotify: (message: string, level: string) => void;
  export let onError: (failure: unknown) => void;

  let bundle: DesktopBundle | null = null;
  let loadError = '';
  let loading = false;
  let iframeEl: HTMLIFrameElement | null = null;
  let loadedId = '';

  $: if (surface.plugin_id !== loadedId) {
    loadedId = surface.plugin_id;
    void loadBundle(surface.plugin_id);
  }

  async function loadBundle(id: string) {
    loading = true;
    loadError = '';
    try {
      const next = await api.pluginDesktopBundle(id);
      if (loadedId === id) bundle = next;
    } catch (failure) {
      if (loadedId === id) loadError = failure instanceof Error ? failure.message : String(failure);
    } finally {
      if (loadedId === id) loading = false;
    }
  }

  function postToFrame(message: Record<string, unknown>) {
    iframeEl?.contentWindow?.postMessage(
      { __kroma_host: true, plugin_id: surface.plugin_id, ...message },
      '*',
    );
  }

  function pushContext() {
    postToFrame({
      type: 'event',
      payload: {
        kind: 'context',
        session_id: sessionId,
        project,
        colors: $pluginColors,
      },
    });
  }

  $: if (bundle && iframeEl && (sessionId || project || $pluginColors)) {
    pushContext();
  }

  $: if ($desktopBusMessage && $desktopBusMessage.plugin_id === surface.plugin_id) {
    postToFrame({ type: 'event', payload: $desktopBusMessage.payload });
  }

  async function onWindowMessage(event: MessageEvent) {
    if (!iframeEl || event.source !== iframeEl.contentWindow) return;
    const msg = event.data;
    if (!msg || msg.__kroma_plugin !== true || msg.plugin_id !== surface.plugin_id) return;
    if (msg.type === 'request') {
      try {
        const result = await api.pluginUiMessage(surface.plugin_id, msg.payload ?? null);
        postToFrame({ type: 'response', id: msg.id, ok: true, result });
      } catch (failure) {
        const err = failure instanceof Error ? failure.message : String(failure);
        postToFrame({ type: 'response', id: msg.id, ok: false, error: err });
        onError(failure);
      }
    } else if (msg.type === 'run_task') {
      const text = typeof msg.text === 'string' ? msg.text.trim() : '';
      if (text) onRunTask(text);
    } else if (msg.type === 'set_composer') {
      onSetComposer(typeof msg.text === 'string' ? msg.text : '', typeof msg.mode === 'string' ? msg.mode : 'replace');
    } else if (msg.type === 'notify') {
      const message = typeof msg.message === 'string' ? msg.message : '';
      if (message) onNotify(message, typeof msg.level === 'string' ? msg.level : 'info');
    }
  }

  onMount(() => {
    window.addEventListener('message', onWindowMessage);
  });
  onDestroy(() => {
    window.removeEventListener('message', onWindowMessage);
  });
</script>

<div class="plugin-desktop-frame" class:full-height={fullHeight}
  style:height={!fullHeight && surface.height ? `${surface.height}px` : undefined}>
  {#if loadError}
    <div class="plugin-desktop-error">{loadError}</div>
  {:else if loading && !bundle}
    <div class="plugin-desktop-loading">正在加载 {surface.title}…</div>
  {:else if bundle}
    <iframe
      bind:this={iframeEl}
      class="plugin-desktop-iframe"
      title={bundle.title}
      sandbox="allow-scripts"
      srcdoc={bundle.html}
      onload={() => pushContext()}
    ></iframe>
  {/if}
</div>

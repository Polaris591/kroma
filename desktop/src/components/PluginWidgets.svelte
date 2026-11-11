<script lang="ts">
  import type { PluginWidget, WidgetPlacement } from '../lib/api';
  import { pluginWidgets } from '../lib/plugin-ui';
  import PanelBlocksView from './PanelBlocksView.svelte';

  export let placement: WidgetPlacement;
  export let busy = false;
  export let onAction: (pluginId: string, widgetId: string, actionId: string) => void;

  $: items = $pluginWidgets.filter((widget: PluginWidget) => widget.placement === placement);
</script>

{#if items.length}
  <div class={`plugin-widgets-slot slot-${placement}`} aria-label={`插件挂件 (${placement})`}>
    {#each items as widget (`${widget.plugin_id}:${widget.id}`)}
      <section class="plugin-widget-card">
        {#if widget.title}
          <header class="plugin-widget-head">
            <strong>{widget.title}</strong>
            <span>{widget.plugin_id}</span>
          </header>
        {/if}
        <PanelBlocksView
          blocks={widget.blocks}
          actions={widget.actions}
          {busy}
          onAction={(actionId) => onAction(widget.plugin_id, widget.id, actionId)} />
      </section>
    {/each}
  </div>
{/if}

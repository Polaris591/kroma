<script lang="ts">
  import { progressPercent, tableGrid, type PanelBlock } from '../lib/panel-blocks';
  import type { PluginUiAction } from '../lib/api';

  export let blocks: PanelBlock[] = [];
  export let actions: PluginUiAction[] = [];
  export let busy = false;
  export let onAction: ((actionId: string) => void) | undefined = undefined;
</script>

{#if blocks.length}
  <div class="panel-blocks">
    {#each blocks as block}
      {#if block.type === 'heading'}
        <h4 class="panel-heading">{block.text}</h4>
      {:else if block.type === 'text'}
        <p class="panel-text">{block.text}</p>
      {:else if block.type === 'list'}
        <ul class="panel-list">
          {#each block.items as item}<li>{item}</li>{/each}
        </ul>
      {:else if block.type === 'keyvalue'}
        <dl class="panel-kv">
          {#each block.rows as row}<dt>{row.key}</dt><dd>{row.value}</dd>{/each}
        </dl>
      {:else if block.type === 'progress'}
        {@const percent = progressPercent(block.value, block.max)}
        <div class="panel-progress">
          <div class="panel-progress-label">
            <span>{block.label}</span><span>{percent}%</span>
          </div>
          <div class="panel-progress-track">
            <span class="panel-progress-fill" style:width={`${percent}%`}></span>
          </div>
        </div>
      {:else if block.type === 'table'}
        {@const grid = tableGrid(block.headers, block.rows)}
        <div class="panel-table-wrap">
          <table class="panel-table">
            <thead><tr>{#each grid.headers as header}<th>{header}</th>{/each}</tr></thead>
            <tbody>
              {#each grid.rows as row}<tr>{#each row as cell}<td>{cell}</td>{/each}</tr>{/each}
            </tbody>
          </table>
        </div>
      {:else if block.type === 'divider'}
        <div class="panel-divider"></div>
      {/if}
    {/each}
  </div>
{/if}
{#if actions.length && onAction}
  <div class="widget-actions">
    {#each actions as action (action.id)}
      <button
        class="small"
        class:primary={action.primary}
        class:danger={action.danger}
        class:secondary={!action.primary}
        disabled={busy}
        onclick={() => onAction?.(action.id)}>
        {action.label}
      </button>
    {/each}
  </div>
{/if}

<script lang="ts">
  export let value = 'auto';
  export let levels: string[] = [];
  export let disabled = false;
  export let onSelect: (level: string) => Promise<void>;

  // Order follows the configured levels; the current value is always offered,
  // even if the profile's level list no longer contains it.
  const labels: Record<string, string> = {auto:'自动',off:'关闭',minimal:'最省',low:'低',medium:'中',high:'高',xhigh:'很高',max:'极高'};
  $: options = Array.from(new Set([...levels, value]));

  async function choose(level: string) {
    if (level !== value && !disabled) await onSelect(level);
  }
</script>

<div class="thinking-control" role="group" aria-label="当前临时思考强度" data-tauri-drag-region="false">
  <div class="model-menu-heading"><strong>思考强度</strong><span>仅当前任务</span></div>
  <div class="thinking-options">
    {#each options as level (level)}
      <button class="thinking-menu-row" class:selected={level === value}
        aria-pressed={level === value} {disabled} onclick={() => void choose(level)}>
        <span>{labels[level] || level.toUpperCase()}</span>
      </button>
    {/each}
  </div>
  <p class="thinking-hint">越高越慢，越细致；不修改模型默认配置。</p>
</div>

<style>
  .thinking-control { margin-top: 6px; border-top: 1px solid var(--hairline); }
  .thinking-options { display: flex; flex-wrap: wrap; gap: 3px; }
  .thinking-menu-row { flex: 1 1 52px; justify-content: center; min-width: 0; padding-inline: 6px; }
  .thinking-menu-row span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>

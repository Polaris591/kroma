<script lang="ts">
  import { BrainCircuit, ChevronDown, Check } from '@lucide/svelte';
  export let value = 'auto';
  export let levels: string[] = [];
  export let disabled = false;
  export let onSelect: (level: string) => Promise<void>;
  let expanded = false;
  // Everyday wording: the trigger must read as a description of effort,
  // not as an unexplained number or an ambiguous "最大".
  const labels: Record<string,string> = {auto:'自动',off:'关闭',minimal:'最省',low:'低',medium:'中',high:'高',xhigh:'很高',max:'极高'};
  function close(){ expanded=false; }
  async function choose(level:string){close(); if (level!==value && !disabled) await onSelect(level);}
</script>
<svelte:window onkeydown={e => {if(e.key === 'Escape')close();}}/>
<div class="thinking-control">
  {#if expanded}<button class="picker-scrim" aria-label="关闭思考等级菜单" onclick={close}></button>{/if}
  <button class="thinking-control-trigger" aria-expanded={expanded} disabled={disabled} onclick={() => expanded=!expanded}
    title="思考等级：越高越慢，但更细致。只影响当前任务，不修改已保存的模型配置。"><BrainCircuit size={15}/><span class="thinking-level">{labels[value] || value.toUpperCase()}</span><ChevronDown size={13}/></button>
  {#if expanded}
    <div class="thinking-menu" role="group" aria-label="思考等级">
      <div class="model-menu-heading"><strong>思考等级</strong></div>
      {#each levels as level (level)}
        <button class="thinking-menu-row" class:selected={level===value} onclick={() => void choose(level)}>
          <span>{labels[level] || level.toUpperCase()}</span>{#if level===value}<Check size={15}/>{/if}
        </button>
      {/each}
      <p class="thinking-hint">越高越慢，越细致</p>
    </div>
  {/if}
</div>

<script lang="ts">
  import { Terminal, ChevronRight } from '@lucide/svelte';
  import { toolLabel, toolLabels } from '../lib/plugin-ui';

  export let name: string;
  export let args = '';
  export let output: string | null = null;
  export let failed = false;
  let expanded = false;
</script>

<details class="tool-result" class:failed
  ontoggle={event => expanded = event.currentTarget.open}>
  <summary>
    <Terminal size={15}/>
    <span class="tool-name">{toolLabel(name, $toolLabels)}</span>
    <span class="tool-state" class:error={failed} class:pending={output === null}>
      {failed ? '失败' : output === null ? '无结果记录' : '完成'}
    </span>
    <ChevronRight size={14} class="disclosure"/>
  </summary>
  {#if expanded}
    {#if args}<pre class="tool-arguments">{args}</pre>{/if}
    {#if output !== null}
      <pre>{output.slice(0, 30000)}{output.length > 30000 ? '\n[此处仅显示前 30000 个字符]' : ''}</pre>
    {/if}
  {/if}
</details>

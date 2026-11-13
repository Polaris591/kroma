<script lang="ts">
  import { modal } from '../lib/modal';
  import { Wrench } from '@lucide/svelte';
  export let toolNames: string[] = [];
  export let skills: Array<{name: string; description?: string}> = [];
  export let onClose: () => void;
</script>

<dialog class="dialog-overlay" aria-labelledby="abilities-heading" use:modal={{ onClose }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel abilities-panel">
    <div class="dialog-heading">
      <span class="dialog-emblem"><Wrench size={18}/></span>
      <div><h2 id="abilities-heading">本会话已加载的能力</h2>
        <p>{toolNames.length} 个工具 · {skills.length} 个技能</p></div>
    </div>
    {#if skills.length}
      <div class="abilities-section">技能</div>
      {#each skills as skill (skill.name)}
        <div class="ability-row"><strong>{skill.name}</strong>{#if skill.description}<small>{skill.description}</small>{/if}</div>
      {/each}
    {/if}
    <div class="abilities-section">工具</div>
    <div class="abilities-tools">
      {#each toolNames as name (name)}
        <span class="ability-chip">{name}</span>
      {/each}
    </div>
    <div class="dialog-actions">
      <button class="dialog-secondary" onclick={onClose}>关闭</button>
    </div>
  </div>
</dialog>

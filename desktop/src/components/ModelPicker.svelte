<script lang="ts">
  import { tick } from 'svelte';
  import { Cpu, ChevronDown, Check, Plus, Settings2, Search } from '@lucide/svelte';
  import type { ProfileSummary } from '../lib/api';
  export let profiles: ProfileSummary[] = [];
  export let connected: string = '';
  export let currentModel: string = '';
  export let disabled = false;
  export let onSelect: (name: string) => Promise<void>;
  export let onAdd: () => void;
  export let onManage: () => void;
  let expanded = false;
  let query = '';
  let searchInput: HTMLInputElement;
  function toggle() {
    expanded = !expanded;
    if(expanded) void tick().then(() => searchInput?.focus());
  }
  $: selected = profiles.find(profile => profile.name === connected);
  $: filtered = profiles.filter(p => (p.name + ' ' + p.model).toLowerCase().includes(query.trim().toLowerCase()));
  function close() {expanded = false; query = '';}
  async function choose(name: string) {
    close();
    if (name === connected || disabled) return;
    await onSelect(name);
  }
</script>

<svelte:window onkeydown={e => {if (e.key === 'Escape' && expanded) {e.stopPropagation();close();}}}/>
<div class="model-control">
  {#if expanded}<button class="picker-scrim" aria-label="关闭模型选择" onclick={close}></button>{/if}
  <button class="model-control-trigger" class:open={expanded} aria-expanded={expanded} aria-label="切换模型"
    disabled={disabled} title={selected ? selected.name + ' · ' + selected.model : currentModel}
    onclick={toggle}>
    <Cpu size={15}/><span class="model-control-name">{selected?.model || currentModel || '选择模型'}</span><ChevronDown size={14}/>
  </button>
  {#if expanded}
    <div class="model-menu" role="group" aria-label="已配置模型">
      <div class="model-menu-heading"><strong>选择模型</strong><span>{profiles.length} 个已配置</span></div>
      {#if profiles.length > 4}
        <div class="model-menu-search"><Search size={14}/><input placeholder="搜索模型…" aria-label="搜索模型"
          bind:this={searchInput} bind:value={query}/></div>
      {/if}
      <div class="model-menu-list">
        {#each filtered as profile (profile.name)}
          <button class="model-menu-row" class:selected={connected === profile.name}
            onclick={() => void choose(profile.name)}>
            <span class="model-identity"><strong>{profile.name}</strong><small>{profile.model}</small></span>
            {#if connected === profile.name}<Check size={16} class="model-selected"/>{/if}
          </button>
        {/each}
        {#if !filtered.length}<p class="model-menu-empty">没有匹配的模型</p>{/if}
      </div>
      <div class="model-menu-footer">
        <button class="model-add-action" onclick={() => {close();onAdd();}}><Plus size={15}/> 添加模型</button>
        <button class="model-settings-action" title="管理所有模型配置" aria-label="模型设置" onclick={() => {close();onManage();}}><Settings2 size={16}/></button>
      </div>
    </div>
  {/if}
</div>
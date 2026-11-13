<script lang="ts">
  import { tick } from 'svelte';
  import { Cpu, ChevronDown, Check, Plus, Settings2, Search } from '@lucide/svelte';
  import type { ProfileSummary } from '../lib/api';
  import ThinkingPicker from './ThinkingPicker.svelte';
  export let profiles: ProfileSummary[] = [];
  export let connected: string = '';
  export let currentModel: string = '';
  export let disabled = false;
  export let onSelect: (name: string) => Promise<void>;
  export let onAdd: () => void;
  export let onManage: () => void;
  export let reasoningLevel = 'auto';
  export let reasoningLevels: string[] = [];
  export let reasoningDisabled = true;
  export let onSelectReasoning: (level: string) => Promise<void> = async () => {};

  let expanded = false;
  let query = '';
  let control: HTMLDivElement;
  let trigger: HTMLButtonElement;
  let menu: HTMLDivElement | undefined;
  let searchInput: HTMLInputElement | undefined;

  $: selected = profiles.find(profile => profile.name === connected);
  $: filtered = profiles.filter(p => (p.name + ' ' + p.model).toLowerCase().includes(query.trim().toLowerCase()));
  $: if (disabled && expanded) close(false);

  async function toggle() {
    if (expanded) {
      close();
      return;
    }
    if (disabled) return;
    expanded = true;
    await tick();
    if (!expanded || !menu?.isConnected) return;
    const target = searchInput
      ?? menu.querySelector<HTMLButtonElement>('.model-menu-row[aria-pressed="true"]:not(:disabled)')
      ?? menu.querySelector<HTMLButtonElement>('.model-menu-row:not(:disabled)')
      ?? menu.querySelector<HTMLButtonElement>('.model-add-action:not(:disabled)')
      ?? menu;
    target.focus();
  }

  function close(restoreFocus = true) {
    if (!expanded) return;
    expanded = false;
    query = '';
    if (restoreFocus) trigger?.focus();
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key !== 'Escape' || event.isComposing || !expanded || document.querySelector('dialog:modal')) return;
    event.preventDefault();
    event.stopPropagation();
    close();
  }

  function handleFocusOut(event: FocusEvent) {
    if (expanded && !control.contains(event.relatedTarget as Node | null)) close(false);
  }

  async function choose(name: string) {
    close();
    if (name === connected || disabled) return;
    await onSelect(name);
  }

  function openSettings(action: () => void) {
    close();
    if (!disabled) action();
  }
</script>

<svelte:window onkeydowncapture={handleKeydown}/>
<div class="model-control" data-tauri-drag-region="false" bind:this={control} onfocusout={handleFocusOut}>
  {#if expanded}<button class="picker-scrim" tabindex="-1" aria-label="关闭模型与思考设置" onclick={() => close()}></button>{/if}
  <button class="model-control-trigger" class:open={expanded} aria-expanded={expanded}
    aria-haspopup="dialog" aria-label="选择模型与思考强度" bind:this={trigger}
    disabled={disabled} title={selected ? selected.name + ' · ' + selected.model : currentModel || '选择模型与思考强度'}
    onclick={() => void toggle()}>
    <Cpu size={15}/><span class="model-control-name">{selected?.model || currentModel || '选择模型'}</span><ChevronDown size={14}/>
  </button>
  {#if expanded}
    <div class="model-menu" role="dialog" aria-label="模型与思考设置" tabindex="-1" bind:this={menu}>
      <div class="model-menu-heading"><strong>选择模型</strong><span>{profiles.length} 个已配置</span></div>
      {#if profiles.length > 4}
        <div class="model-menu-search"><Search size={14}/><input placeholder="搜索模型…" aria-label="搜索模型"
          bind:this={searchInput} bind:value={query}/></div>
      {/if}
      <div class="model-menu-list" role="group" aria-label="已配置模型">
        {#each filtered as profile (profile.name)}
          <button class="model-menu-row" class:selected={connected === profile.name}
            aria-pressed={connected === profile.name} disabled={disabled}
            onclick={() => void choose(profile.name)}>
            <span class="model-identity"><strong>{profile.name}</strong><small>{profile.model}</small></span>
            {#if connected === profile.name}<Check size={16} class="model-selected"/>{/if}
          </button>
        {/each}
        {#if !filtered.length}<p class="model-menu-empty">{profiles.length ? '没有匹配的模型' : '尚未添加模型'}</p>{/if}
      </div>
      <ThinkingPicker value={reasoningLevel} levels={reasoningLevels}
        disabled={disabled||reasoningDisabled} onSelect={onSelectReasoning}/>
      <div class="model-menu-footer">
        <button class="model-add-action" disabled={disabled} onclick={() => openSettings(onAdd)}><Plus size={15}/> 添加模型</button>
        <button class="model-settings-action" disabled={disabled} title="管理所有模型配置" aria-label="管理模型配置" onclick={() => openSettings(onManage)}><Settings2 size={16}/></button>
      </div>
    </div>
  {/if}
</div>

<style>
  .model-control-trigger { max-width: min(280px, 35vw); }
  .model-control-name { min-width: 0; }
  .model-menu {
    width: min(330px, calc(100vw - 24px));
    max-height: calc(100dvh - 72px);
    overflow-y: auto;
  }
  @media (max-width: 600px) {
    .model-menu { position: fixed; top: 60px; right: 12px; }
  }
</style>

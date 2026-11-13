<script lang="ts">
  import type { Bootstrap, ProfileInput, CapabilityKey } from '../lib/api';
  import { BookOpenText, Cpu, Network, Puzzle, Wrench } from '@lucide/svelte';
  import SettingsView from './SettingsView.svelte';
  import PluginsView from './PluginsView.svelte';
  import CapabilitySettings from './CapabilitySettings.svelte';

  export let data: Bootstrap;
  export let section: 'models' | CapabilityKey = 'models';
  export let disabled = false;
  export let onSaving: (value: boolean) => void;
  export let onSaved: () => Promise<void>;
  export let onSave: (input: ProfileInput) => Promise<void>;
  export let onConnect: (name: string) => Promise<void>;
  export let onSetDefault: (name: string) => Promise<void>;
  export let onAdd: () => void;
  export let onOpenStorage: () => void;
  export let onPluginAction: (action: string, id?: string, path?: string, key?: string, value?: unknown) => Promise<void>;
  export let onInstallPlugin: () => Promise<void>;
  export let onOpenDesktopPanel: (id: string) => void;
  const sections = [
    {id: 'models', label: '模型', icon: Cpu}, {id: 'tools', label: '工具', icon: Wrench},
    {id: 'mcp', label: 'MCP', icon: Network}, {id: 'skills', label: 'Skills', icon: BookOpenText},
    {id: 'plugins', label: '插件', icon: Puzzle},
  ] as const;
</script>

<div class="settings-shell">
  <nav class="settings-navigation" aria-label="设置分类">
    {#each sections as item}
      {@const Icon = item.icon}
      <button class:active={section === item.id} aria-current={section === item.id ? 'page' : undefined}
        disabled={disabled} onclick={() => section = item.id}><Icon size={15}/><span>{item.label}</span></button>
    {/each}
  </nav>
  {#if section === 'models'}
    <SettingsView data={data.profiles} connected={data.ready ? data.connected_profile ?? '' : ''}
      {onSave} {onConnect} {onSetDefault} {onAdd} {onOpenStorage}/>
  {:else if section === 'plugins'}
    <PluginsView plugins={data.plugins} warnings={data.plugin_warnings ?? []} {disabled} onAction={onPluginAction}
      onInstall={onInstallPlugin} {onOpenDesktopPanel}/>
  {:else}
    {#key `${data.project}:${data.snapshot?.session_id ?? ''}:${section}`}
      <CapabilitySettings category={section} {disabled} {onSaving} {onSaved}/>
    {/key}
  {/if}
</div>

<style>
  .settings-shell { max-width: 1000px; margin: 0 auto; }
  .settings-navigation { display: flex; gap: 6px; margin: 0 0 32px; padding-bottom: 10px; border-bottom: 1px solid var(--line); }
  .settings-navigation button { position: relative; display: inline-flex; align-items: center; justify-content: center; gap: 8px; min-height: 36px; padding: 7px 13px; border-radius: var(--radius-md); color: var(--muted); font-size: 12px; white-space: nowrap; }
  .settings-navigation button:hover:not(:disabled) { background: var(--wash-1); color: var(--text); }
  .settings-navigation button.active { color: var(--text); background: var(--wash-1); font-weight: 600; }
  .settings-navigation button.active:after { position: absolute; content: ''; right: 13px; bottom: -11px; left: 13px; height: 2px; border-radius: 2px; background: var(--accent); }
  @media (max-width: 600px) {
    .settings-navigation { gap: 2px; margin-bottom: 26px; }
    .settings-navigation button { flex: 1; gap: 5px; padding-inline: 5px; }
    .settings-navigation button.active:after { left: 8px; right: 8px; }
  }
</style>

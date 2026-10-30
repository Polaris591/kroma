<script lang="ts">
  import type { PluginSummary } from '../lib/api';
  import { Puzzle, Plus, RotateCw, Trash2, SlidersHorizontal, ChevronRight } from '@lucide/svelte';
  export let plugins: PluginSummary[];
  export let onAction: (action: string, id?: string, path?: string, key?: string, value?: unknown) => Promise<void>;
  export let onInstall: () => Promise<void>;
  let selected: string | null = null;
  let editing: Record<string, unknown> = {};
  $: current = plugins.find(p => p.id === selected) ?? null;
  function open(plugin: PluginSummary) {
    selected = plugin.id;
    editing = {...plugin.settings};
  }
  async function saveSetting(key: string) {
    if (!current) return;
    await onAction('set', current.id, undefined, key, editing[key]);
  }
</script>

<section class="plugins-page">
  <div class="section-heading">
    <div><h2>插件</h2><p>同一个插件系统，同时服务于 CLI、TUI 和 Desktop。</p></div>
    <div class="section-buttons">
      <button class="secondary small" onclick={onInstall}><Plus size={15}/> 安装本地插件</button>
      <button class="icon-button" title="从配置重新加载" onclick={() => onAction('reload')}><RotateCw size={16}/></button>
    </div>
  </div>
  {#if plugins.length === 0}
    <div class="empty-panel"><Puzzle size={27}/><h3>还没有插件</h3><p>从本地文件夹安装插件。插件默认不启用。</p></div>
  {:else}
    <div class="plugin-layout">
      <div class="plugin-list">
        {#each plugins as plugin (plugin.id)}
          <button class="plugin-row" class:active={selected === plugin.id} onclick={() => open(plugin)}>
            <div class="plugin-icon"><Puzzle size={16}/></div>
            <div class="plugin-summary">
              <strong>{plugin.name}</strong><span>{plugin.description || plugin.id}</span>
            </div>
            <span class:enabled={plugin.enabled} class="plugin-dot"></span>
            <ChevronRight size={14}/>
          </button>
        {/each}
      </div>
      <div class="plugin-inspector">
        {#if current}
          <div class="inspector-title"><h3>{current.name}</h3><span>v{current.version || '1'} · {current.id}</span></div>
          <div class="inspector-toggle">
            <span>启用插件</span>
            <button class:enabled={current.enabled} class="switch" aria-label="切换插件" aria-pressed={current.enabled}
              onclick={() => onAction(current!.enabled ? 'disable' : 'enable', current!.id)}><span></span></button>
          </div>
          {#if Object.keys(current.settings).length}
            <div class="plugin-config-head"><SlidersHorizontal size={15}/> 插件设置</div>
            <div class="config-fields">
              {#each Object.entries(current.settings) as [key,value]}
                <label>
                  <span>{key}</span>
                  {#if typeof value === 'boolean'}
                    <input type="checkbox" checked={Boolean(editing[key])}
                      onchange={async e => {editing[key] = e.currentTarget.checked; await saveSetting(key)}} />
                  {:else if typeof value === 'number'}
                    <input type="number" value={String(editing[key] ?? '')}
                      onchange={async e => {editing[key] = Number(e.currentTarget.value); await saveSetting(key)}} />
                  {:else}
                    <div class="config-input"><input value={String(editing[key] ?? '')}
                      onchange={e => editing[key] = e.currentTarget.value} />
                      <button class="small secondary" onclick={() => saveSetting(key)}>保存</button></div>
                  {/if}
                </label>
              {/each}
            </div>
          {:else}
            <p class="muted">此插件没有声明设置字段。</p>
          {/if}
          {#if current.has_model || current.has_context}
            <p class="subtle-note">修改模型或上下文插件后需要重新连接模型才能生效。</p>
          {/if}
          {#if !current.project}
            <button class="danger secondary" onclick={() => {
              if (confirm('永久卸载此插件？插件数据保留。')) onAction('uninstall', current!.id)
            }}><Trash2 size={15}/> 卸载</button>
          {/if}
        {:else}
          <div class="empty-inspector"><SlidersHorizontal size={25}/><p>选择插件以查看状态和设置</p></div>
        {/if}
      </div>
    </div>
  {/if}
</section>

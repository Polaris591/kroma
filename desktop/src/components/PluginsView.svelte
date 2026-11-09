<script lang="ts">
  import type { PluginSummary } from '../lib/api';
  import { api } from '../lib/api';
  import { pluginColors, pluginPanels, slotColor } from '../lib/plugin-ui';
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
    for (const key of plugin.secret_settings ?? []) editing[key] = '';
  }
  async function saveSetting(key: string) {
    if (!current) return;
    const value = editing[key];
    const secret = current.secret_settings?.includes(key) ?? false;
    if (secret && (value === '' || value === '••••')) return;
    await onAction('set', current.id, undefined, key, value);
    if (secret) editing[key] = '';
  }
  async function clearSetting(key: string) {
    if (!current) return;
    await onAction('set', current.id, undefined, key, null);
    editing[key] = '';
  }
  function secretIsSet(key: string) {
    return current?.set_secrets?.includes(key) ?? false;
  }
  // A panel body is produced by the plugin itself (`title\n\nbody`), so the
  // interface never guesses what a plugin wants to show.
  let panel: {title: string; body: string} | null = null;
  $: panels = current ? $pluginPanels.filter(entry => entry.id === current!.id) : [];
  // A declared theme paints the plugin's own surface, not the whole window.
  $: panelColors = [
    slotColor("background", $pluginColors) && ("background:" + slotColor("background", $pluginColors)),
    slotColor("text", $pluginColors) && ("color:" + slotColor("text", $pluginColors)),
    slotColor("border", $pluginColors) && ("border-color:" + slotColor("border", $pluginColors)),
  ].filter(Boolean).join(";");
  async function openPanel(id: string) {
    try {
      const content = await api.pluginPanel(id);
      const [title, ...rest] = content.split('\n\n');
      panel = {title: title.trim() || current?.name || '插件面板', body: rest.join('\n\n') || title};
    } catch (failure) {
      panel = {title: current?.name || '插件面板', body: String(failure)};
    }
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
            <span class="plugin-state" class:enabled={plugin.enabled}>{plugin.enabled ? '已启用' : '未启用'}</span>
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
                      placeholder={current.secret_settings?.includes(key)
                        ? (secretIsSet(key) ? '已设置；留空保持不变' : '未设置')
                        : ''}
                      onchange={e => editing[key] = e.currentTarget.value} />
                      <button class="small secondary" onclick={() => saveSetting(key)}>保存</button>
                      {#if current.secret_settings?.includes(key) && secretIsSet(key)}
                        <button class="small secondary" title="移除已保存的值"
                          onclick={() => clearSetting(key)}>清除</button>
                      {/if}
                    </div>
                  {/if}
                </label>
              {/each}
            </div>
          {:else}
            <p class="muted">此插件没有声明设置字段。</p>
          {/if}
          {#if panels.length}
            <div class="plugin-config-head"><Puzzle size={15}/> 插件面板</div>
            <div class="panel-buttons">
              {#each panels as entry (entry.id)}
                <button class="secondary small" onclick={() => void openPanel(entry.id)}>
                  {entry.title || entry.id}
                </button>
              {/each}
            </div>
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
  {#if panel}
    <div class="panel-overlay">
      <button class="panel-scrim" aria-label="关闭面板" onclick={() => panel = null}></button>
      <div class="panel-card" role="dialog" aria-modal="true" aria-label={panel.title} style={panelColors}>
        <div class="panel-head">
          <h3>{panel.title}</h3>
          <button class="icon-button" aria-label="关闭面板" onclick={() => panel = null}>×</button>
        </div>
        <pre class="panel-body">{panel.body}</pre>
      </div>
    </div>
  {/if}
</section>

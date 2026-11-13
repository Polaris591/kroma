<script lang="ts">
  import { onDestroy } from 'svelte';
  import { modal } from '../lib/modal';
  import type { PluginSummary } from '../lib/api';
  import { api } from '../lib/api';
  import { pluginColors, pluginPanels, slotColor } from '../lib/plugin-ui';
  import type { PanelBlock } from '../lib/panel-blocks';
  import PanelBlocksView from './PanelBlocksView.svelte';
  import SettingsDiagnostics from './SettingsDiagnostics.svelte';
  import { Puzzle, Plus, RotateCw, Trash2, SlidersHorizontal, ChevronRight } from '@lucide/svelte';
  export let plugins: PluginSummary[];
  export let warnings: string[] = [];
  export let disabled = false;
  export let onAction: (action: string, id?: string, path?: string, key?: string, value?: unknown) => Promise<void>;
  export let onInstall: () => Promise<void>;
  export let onOpenDesktopPanel: ((id: string) => void) | undefined = undefined;
  let selected: string | null = null;
  let editing: Record<string, unknown> = {};
  let busyKey: string | null = null;
  let uninstallConfirm = false;
  let uninstallBusy = false;
  let actionError = '';
  let actionMessage = '';
  let panelLoading = '';
  let panelRequest = 0;
  // Unsaved secrets live only in this mounted settings page, never in local
  // storage. Switching inspectors must not silently erase a failed save.
  const secretDrafts = new Map<string, Record<string, unknown>>();
  onDestroy(() => { panelRequest += 1; secretDrafts.clear(); });
  $: current = plugins.find(p => p.id === selected) ?? null;
  $: if (!busyKey && !uninstallBusy && plugins.length && !plugins.some(plugin => plugin.id === selected)) selected = open(plugins[0]);
  function open(plugin: PluginSummary) {
    if (busyKey || uninstallBusy || selected === plugin.id) return selected;
    if (current) secretDrafts.set(current.id, Object.fromEntries(
      (current.secret_settings ?? []).map(key => [key, editing[key] ?? '']),
    ));
    panelRequest += 1;
    panelLoading = '';
    actionError = '';
    actionMessage = '';
    uninstallConfirm = false;
    const retained = secretDrafts.get(plugin.id);
    editing = {...plugin.settings};
    for (const key of plugin.secret_settings ?? []) editing[key] = retained?.[key] ?? '';
    return plugin.id;
  }
  async function runPluginAction(action: string, id?: string, path?: string, key?: string, value?: unknown) {
    if (disabled || busyKey) return false;
    busyKey = `${action}:${id ?? ''}:${key ?? ''}`;
    actionError = '';
    actionMessage = '';
    try {
      await onAction(action, id, path, key, value);
      actionMessage = key ? `${value === null ? '已清除' : '已保存'} ${key}`
        : action === 'reload' ? '插件列表已重新加载'
        : action === 'enable' ? '已默认启用此插件'
        : action === 'disable' ? '已关闭此插件的默认启用'
        : action === 'uninstall' ? '插件已卸载，已保存的数据仍保留' : '操作已完成';
      return true;
    } catch (failure) {
      actionError = failure instanceof Error ? failure.message : String(failure);
      return false;
    } finally { busyKey = null; }
  }
  async function install() {
    if (disabled || busyKey) return;
    busyKey = 'install:';
    actionError = '';
    actionMessage = '';
    try { await onInstall(); }
    catch (failure) { actionError = failure instanceof Error ? failure.message : String(failure); }
    finally { busyKey = null; }
  }
  async function saveSetting(key: string) {
    if (!current || disabled || busyKey) return;
    const id = current.id;
    const value = editing[key];
    const secret = current.secret_settings?.includes(key) ?? false;
    if (secret ? value === '' || value === '••••' : value === current.settings[key]) return;
    const saved = await runPluginAction('set', id, undefined, key, value);
    if (current?.id !== id) return;
    if (saved && secret) editing[key] = '';
    else if (!saved && typeof value === 'boolean') editing[key] = current.settings[key];
  }
  async function clearSetting(key: string) {
    if (!current || disabled || busyKey) return;
    if (await runPluginAction('set', current.id, undefined, key, null)) editing[key] = '';
  }
  async function confirmUninstall() {
    if (!current || disabled || busyKey || uninstallBusy) return;
    const id = current.id;
    uninstallBusy = true;
    try {
      if (await runPluginAction('uninstall', id)) {
        secretDrafts.delete(id);
        uninstallConfirm = false;
        selected = null;
      }
    } finally { uninstallBusy = false; }
  }
  function secretIsSet(key: string) {
    return current?.set_secrets?.includes(key) ?? false;
  }
  // A panel body is produced by the plugin itself. A current host returns
  // structured blocks; an older one only knows `title\n\nbody`, which still
  // renders as plain text, so the interface never has to guess.
  let panel: {title: string; blocks: PanelBlock[] | null; body: string} | null = null;
  $: panels = current ? $pluginPanels.filter(entry => entry.id === current!.id) : [];
  // A declared theme paints the plugin's own surface, not the whole window.
  $: panelColors = [
    slotColor("background", $pluginColors) && ("background:" + slotColor("background", $pluginColors)),
    slotColor("text", $pluginColors) && ("color:" + slotColor("text", $pluginColors)),
    slotColor("border", $pluginColors) && ("border-color:" + slotColor("border", $pluginColors)),
  ].filter(Boolean).join(";");
  async function openPanel(id: string) {
    const request = ++panelRequest;
    const name = current?.name || '插件面板';
    panelLoading = id;
    try {
      const view = await api.pluginPanelView(id);
      if (request === panelRequest) panel = {
        title: view?.title || name,
        blocks: Array.isArray(view?.blocks) ? view.blocks : [], body: '',
      };
    } catch {
      if (request !== panelRequest) return;
      try {
        const content = await api.pluginPanel(id);
        const [title, ...rest] = content.split('\n\n');
        if (request === panelRequest) panel = {title: title.trim() || name, blocks: null,
          body: rest.join('\n\n') || title};
      } catch (failure) {
        if (request === panelRequest) actionError = String(failure);
      }
    } finally { if (request === panelRequest) panelLoading = ''; }
  }
</script>

<section class="plugins-page">
  <div class="section-heading">
    <div><h2>插件</h2><p>选择插件，设置默认开关或修改配置。会话里的单独选择优先。</p></div>
    <div class="section-buttons">
      <button class="primary small" disabled={disabled || busyKey !== null} onclick={() => void install()}><Plus size={15}/> {busyKey === 'install:' ? '正在安装…' : '安装本地插件'}</button>
      <button class="icon-button" title="从配置重新加载" aria-label="从配置重新加载" aria-busy={busyKey?.startsWith('reload')}
        disabled={disabled || busyKey !== null} onclick={() => void runPluginAction('reload')}>
        <RotateCw size={16} class={busyKey?.startsWith('reload') ? 'spin' : undefined}/></button>
    </div>
  </div>
  <SettingsDiagnostics messages={warnings} title="插件检查提示"
    hint="检查插件来源或清单。重复项会被跳过，其他插件仍可管理；无需因此删除整个插件目录。"/>
  {#if !uninstallConfirm && actionError}<p class="settings-error" role="alert">{actionError}</p>{/if}
  <p class="settings-hint" role="status">{busyKey ? '正在处理，请稍候…' : actionMessage}</p>
  {#if plugins.length === 0}
    <div class="empty-panel"><Puzzle size={27}/><h3>还没有插件</h3><p>从本地文件夹安装插件。插件默认不启用。</p></div>
  {:else}
    <div class="plugin-layout">
      <div class="plugin-list">
        <div class="plugin-list-heading"><span>已安装</span><small>{plugins.length}</small></div>
        {#each plugins as plugin (plugin.id)}
          <button class="plugin-row" class:active={selected === plugin.id} aria-pressed={selected === plugin.id}
            disabled={busyKey !== null || uninstallBusy} onclick={() => selected = open(plugin)}>
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
          <div class="inspector-title"><span class="inspector-plugin-icon"><Puzzle size={20}/></span><div><h3>{current.name}</h3><span>v{current.version || '1'} · {current.id}</span></div></div>
          {#if current.description}<p class="inspector-description">{current.description}</p>{/if}
          <div class="inspector-toggle">
            <div><span>默认启用</span><small>会话中的单独设置优先</small></div>
            <button class:enabled={current.enabled} class="switch" aria-label="切换插件" aria-pressed={current.enabled}
              disabled={disabled || busyKey !== null}
              onclick={() => void runPluginAction(current!.enabled ? 'disable' : 'enable', current!.id)}><span></span></button>
          </div>
          {#if Object.keys(current.settings).length}
            <div class="plugin-config-head"><SlidersHorizontal size={15}/> 插件设置</div>
            <p class="settings-hint">普通字段离开输入框后保存；密钥需点“保存”，留空不变。</p>
            <fieldset class="config-fields" disabled={disabled || busyKey !== null || uninstallBusy}>
              <legend class="sr-only">{current.name}的设置</legend>
              {#each Object.entries(current.settings) as [key,value]}
                {@const secret = current.secret_settings?.includes(key) ?? false}
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
                      type={secret ? 'password' : 'text'} autocomplete="off" spellcheck={false}
                      placeholder={secret
                        ? (secretIsSet(key) ? '已设置；留空保持不变' : '未设置')
                        : ''}
                      oninput={e => editing[key] = e.currentTarget.value}
                      onblur={() => { if (!secret) void saveSetting(key); }} />
                      {#if secret}
                        <button class="small secondary" disabled={!String(editing[key] ?? '').length} onclick={() => void saveSetting(key)}>保存</button>
                      {/if}
                      {#if secret && secretIsSet(key)}
                        <button class="small secondary" title="移除已保存的值"
                          onclick={() => clearSetting(key)}>清除</button>
                      {/if}
                    </div>
                  {/if}
                </label>
              {/each}
            </fieldset>
          {:else}
            <p class="muted">此插件没有声明设置字段。</p>
          {/if}
          {#if panels.length || current.has_desktop_ui}
            <div class="plugin-config-head"><Puzzle size={15}/> 插件面板</div>
            <div class="panel-buttons">
              {#each panels as entry (entry.id)}
                <button class="secondary small" disabled={!!panelLoading || busyKey !== null} onclick={() => void openPanel(entry.id)}>
                  {panelLoading === entry.id ? '正在打开…' : entry.title || entry.id}
                </button>
              {/each}
              {#if current.has_desktop_ui && current.enabled && onOpenDesktopPanel}
                <button class="secondary small" onclick={() => onOpenDesktopPanel?.(current!.id)}>
                  打开桌面自定义界面
                </button>
              {/if}
            </div>
          {/if}
          {#if current.has_model || current.has_context}
            <p class="subtle-note">修改模型或上下文插件后需要重新连接模型才能生效。</p>
          {/if}
          {#if !current.project}
            <div class="plugin-danger-zone">
              <button class="danger secondary small" disabled={disabled || busyKey !== null || uninstallBusy}
                onclick={() => { actionError = ''; uninstallConfirm = true; }}><Trash2 size={14}/> 卸载</button>
              <small>仅移除插件，保留已保存的数据</small>
            </div>
          {/if}
        {:else}
          <div class="empty-inspector"><SlidersHorizontal size={25}/><p>选择插件以查看状态和设置</p></div>
        {/if}
      </div>
    </div>
  {/if}
  {#if uninstallConfirm && current}
    <dialog class="dialog-overlay" aria-labelledby="plugin-uninstall-heading"
      use:modal={{ onClose: () => uninstallConfirm = false, busy: uninstallBusy, initialFocus: '.dialog-secondary' }}>
      <div class="dialog-dim"></div>
      <div class="dialog-panel delete-confirm">
        <div class="dialog-heading"><span class="dialog-emblem danger"><Trash2 size={19}/></span>
          <div><h2 id="plugin-uninstall-heading">卸载这个插件？</h2><p>{current.name}</p></div>
        </div>
        <p class="dialog-description">插件会从列表中移除，已保存的插件数据仍然保留。项目内自带的插件不在此列。</p>
        {#if actionError}<p class="dialog-error" role="alert">{actionError}</p>{/if}
        <div class="dialog-actions">
          <button class="dialog-secondary" disabled={uninstallBusy} onclick={() => uninstallConfirm = false}>取消</button>
          <button class="dialog-danger" disabled={uninstallBusy} onclick={() => void confirmUninstall()}>
            {uninstallBusy ? '正在卸载…' : '卸载插件'}
          </button>
        </div>
      </div>
    </dialog>
  {/if}
  {#if panel}
    <dialog class="panel-overlay" aria-label={panel.title} use:modal={{ onClose: () => panel = null }}>
      <button class="panel-scrim" tabindex="-1" aria-hidden="true" onclick={() => panel = null}></button>
      <div class="panel-card" style={panelColors}>
        <div class="panel-head">
          <h3>{panel.title}</h3>
          <button class="icon-button" aria-label="关闭面板" onclick={() => panel = null}>×</button>
        </div>
        {#if panel.blocks}
          <PanelBlocksView blocks={panel.blocks} />
        {:else}
          <pre class="panel-body">{panel.body}</pre>
        {/if}
      </div>
    </dialog>
  {/if}
</section>

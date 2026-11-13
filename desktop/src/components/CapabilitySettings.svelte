<script lang="ts">
  import { onMount } from 'svelte';
  import { Search } from '@lucide/svelte';
  import { api, type CapabilityItem, type CapabilityKey, type CapabilitySettings } from '../lib/api';
  import SettingsDiagnostics from './SettingsDiagnostics.svelte';

  export let scope: 'defaults' | 'session' = 'defaults';
  export let category: CapabilityKey | 'all' = 'all';
  export let disabled = false;
  export let onSaving: (value: boolean) => void = () => {};
  export let onSaved: () => Promise<void> = async () => {};
  const entries: Array<{key: CapabilityKey; name: string; description: string; empty: string}> = [
    {key: 'tools', name: '工具', description: '逐个选择工具。关闭某个工具，不会连带关闭其他工具。', empty: '暂无可用工具。'},
    {key: 'mcp', name: 'MCP', description: '逐个选择外部工具服务，启用后直接加载，不需要批准。', empty: '暂无已配置的服务。配置沿用本机或工作区的 .kroma/mcp.json。'},
    {key: 'skills', name: 'Skills', description: '逐个选择提供给模型的技能说明。', empty: '暂无已安装的技能。技能从本机和工作区的 .agents/skills、.kroma/skills 读取。'},
    {key: 'plugins', name: '插件', description: '逐个选择插件，包括它提供的工具、行为扩展和自定义界面。关闭不代表卸载。', empty: '暂无已安装的插件。'},
  ];
  const sources: Record<string, string> = {builtin: '内置', project: '当前工作区', user: '本机', mcp: 'MCP', plugin: '插件', skills: 'Skill'};
  let data: CapabilitySettings | null = null;
  let saving = false;
  let error = '';
  let message = '';
  let search = '';
  $: selectedCategory = entries.find(entry => entry.key === category);
  $: warnings = data?.warning_groups && category !== 'all'
    ? data.warning_groups[category] ?? [] : data?.warnings ?? [];
  $: query = search.trim().toLowerCase();
  $: groups = entries.filter(entry => category === 'all' || entry.key === category).map(entry => {
    const all = data?.items[entry.key] ?? [];
    return {...entry, all, visible: all.filter(item => !query || `${item.name} ${item.id} ${item.description ?? ''}`.toLowerCase().includes(query))};
  });
  $: matches = groups.reduce((sum, group) => sum + group.visible.length, 0);

  async function load() {
    error = '';
    try {
      const next = await api.capabilitySettings();
      if (!next?.items || entries.some(entry => !Array.isArray(next.items[entry.key]))) {
        throw new Error('当前程序还不支持逐项设置，请关闭旧窗口后打开新版本。');
      }
      data = next;
    } catch (failure) { error = String(failure); }
  }
  onMount(() => { void load(); });

  async function save(group: CapabilityKey, items: Record<string, boolean | null>) {
    if (!data || saving || disabled || !Object.keys(items).length) return;
    saving = true;
    onSaving(true);
    error = '';
    message = '';
    let saved = false;
    try {
      data = await api.setCapabilityItems(scope, group, items);
      saved = true;
      message = '已保存';
      await onSaved();
    } catch (failure) { error = saved ? `设置已保存，但界面刷新未完成：${failure}` : String(failure); }
    finally { saving = false; onSaving(false); }
  }
  function selectValue(item: CapabilityItem) {
    return item.session_enabled === null ? 'default' : item.session_enabled ? 'on' : 'off';
  }
  function bulk(group: CapabilityKey, items: CapabilityItem[], value: boolean | null) {
    void save(group, Object.fromEntries(items.map(item => [item.id, value])));
  }
</script>

<section class="capability-settings" class:session-capabilities={scope === 'session'} aria-label={scope === 'session' ? '会话能力' : '默认能力'} aria-busy={saving}>
  {#if scope === 'defaults'}
    <div class="section-heading"><div><h2>{selectedCategory?.name ?? '能力设置'}</h2><p>{selectedCategory?.description ?? '管理默认提供给模型的工具与扩展。'}</p></div><span class="state-badge">默认设置</span></div>
  {/if}
  <p class="settings-hint">{scope === 'session'
    ? '逐项选择，只影响当前会话。每一项都可以选择“跟随默认”。'
    : '修改后自动保存。会话内的单独设置优先。'}</p>
  {#if disabled}<p class="settings-hint">任务执行期间不能切换能力，请先停止任务或等它完成。</p>{/if}
  {#if data}
    <SettingsDiagnostics messages={warnings} title={category === 'all' ? '能力检查提示' : `${selectedCategory?.name ?? '能力'}检查提示`}/>
    <div class="capability-search-wrap"><Search size={15}/><input class="capability-search" type="search" aria-label="搜索能力" placeholder="搜索名称、工具名或说明" bind:value={search}/></div>
    {#each groups as group (group.key)}
      {#if !query || group.visible.length}
        <section class="capability-group" aria-label={group.name}>
          <div class="capability-group-heading"><h3>{scope === 'defaults' && category !== 'all' ? '可用项目' : group.name}</h3><span>{group.all.filter(item => scope === 'session' ? item.effective_enabled : item.default_enabled).length} / {group.all.length} 已启用</span></div>
          {#if scope === 'session' || category === 'all'}<p class="settings-hint">{group.description}</p>{/if}
          {#if group.visible.length}
            <div class="capability-bulk">
              <button class="secondary small" disabled={disabled || saving} aria-label={`${group.name}${query ? '开启匹配项' : '全部开启'}`}
                onclick={() => bulk(group.key, group.visible, true)}>{query ? '开启匹配项' : '全部开启'}</button>
              <button class="secondary small" disabled={disabled || saving} aria-label={`${group.name}${query ? '关闭匹配项' : '全部关闭'}`}
                onclick={() => bulk(group.key, group.visible, false)}>{query ? '关闭匹配项' : '全部关闭'}</button>
              {#if scope === 'session'}<button class="secondary small" disabled={disabled || saving} aria-label={`${group.name}${query ? '匹配项跟随默认' : '全部跟随默认'}`}
                onclick={() => bulk(group.key, group.visible, null)}>{query ? '匹配项跟随默认' : '全部跟随默认'}</button>{/if}
            </div>
            <div class="capability-list">
              {#each group.visible as item, index (item.id)}
                <div class="capability-row" data-item-id={item.id}>
                  <div class="capability-item-text">
                    <div class="capability-item-heading"><label for={`${scope}-${group.key}-${index}`}>{item.name}</label>
                      {#if item.source}<small class="capability-source">{sources[item.source] ?? item.source}</small>{/if}
                    </div>
                    {#if item.id !== item.name}<code>{item.id}</code>{/if}
                    {#if item.description}<p>{item.description}</p>{/if}
                    {#if item.available === false}<small>当前不可用，已保留你的选择</small>{/if}
                  </div>
                  {#if scope === 'session'}
                    <select id={`${scope}-${group.key}-${index}`} value={selectValue(item)} disabled={disabled || saving}
                      onchange={event => {
                        const value = event.currentTarget.value;
                        event.currentTarget.value = selectValue(item);
                        void save(group.key, {[item.id]: value === 'default' ? null : value === 'on'});
                      }}>
                      <option value="default">跟随默认（{item.default_enabled ? '开启' : '关闭'}）</option>
                      <option value="on">开启</option><option value="off">关闭</option>
                    </select>
                  {:else}
                    <input id={`${scope}-${group.key}-${index}`} type="checkbox" checked={item.default_enabled} disabled={disabled || saving}
                      onchange={event => {
                        const value = event.currentTarget.checked;
                        event.currentTarget.checked = item.default_enabled;
                        void save(group.key, {[item.id]: value});
                      }}/>
                  {/if}
                </div>
              {/each}
            </div>
          {:else}<p class="settings-hint">{group.empty}</p>{/if}
        </section>
      {/if}
    {/each}
    {#if query && !matches}<p class="settings-hint">没有匹配的项目。</p>{/if}
  {:else if !error}<p role="status">正在读取设置…</p>{/if}
  {#if message}<p class="settings-hint" role="status">{message}</p>{/if}
  {#if error}<div class="settings-error" role="alert">{error}<button class="secondary small" disabled={saving} onclick={() => void load()}>重新读取</button></div>{/if}
</section>

<style>
  .capability-settings { max-width: 1000px; margin: 0 auto 28px; }
  .session-capabilities { max-height: 65vh; overflow: auto; padding-right: 6px; margin-bottom: 0; }
  .capability-search-wrap { position: relative; margin: 18px 0 4px; }
  .capability-search-wrap > :global(svg) { position: absolute; top: 50%; left: 12px; transform: translateY(-50%); pointer-events: none; color: var(--muted); }
  .capability-search { width: 100%; min-height: 40px; padding: 9px 12px 9px 36px; border: 1px solid var(--line); border-radius: 8px; background: var(--field-surface); color: var(--text); font-size: 12px; }
  .capability-search::placeholder { color: var(--muted); }
  .capability-group { margin-top: 20px; padding: 18px 20px 0; border: 1px solid var(--line); border-radius: var(--radius-lg); background: var(--panel-surface); }
  .capability-group-heading { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 6px 14px; margin-bottom: 8px; }
  .capability-group-heading h3 { margin: 0; font-size: 12px; font-weight: 600; }
  .capability-group-heading > span { color: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
  .capability-bulk { display: flex; flex-wrap: wrap; gap: 6px; margin: 12px 0; }
  .capability-bulk button { min-height: 28px; font-weight: 500; }
  .capability-list { border-top: 1px solid var(--hairline); }
  .capability-row { display: flex; align-items: center; justify-content: space-between; gap: 24px; min-height: 68px; padding: 16px 0; border-bottom: 1px solid var(--hairline); }
  .capability-row:last-child { border-bottom: 0; }
  .capability-item-text { min-width: 0; display: flex; flex-direction: column; align-items: flex-start; gap: 6px; overflow-wrap: anywhere; }
  .capability-item-heading { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
  .capability-row label { font-size: 12.5px; font-weight: 550; cursor: pointer; }
  .capability-row .capability-source { padding: 1px 6px; border: 1px solid var(--hairline); border-radius: 5px; font-size: 10px; line-height: 1.5; }
  .capability-row p { color: var(--muted); font-size: 12px; line-height: 1.7; margin: 0; }
  .capability-row code, .capability-row small { color: var(--muted); font-size: 11px; white-space: normal; }
  .capability-row code { font-family: var(--mono); }
  .capability-row select { flex-shrink: 0; width: 158px; max-width: 100%; min-height: 36px; padding: 6px 9px; border: 1px solid var(--line); border-radius: 8px; background: var(--field-surface); color: var(--text); font-size: 12px; }
  .capability-row input { flex-shrink: 0; width: 17px; height: 17px; margin: 6px; padding: 0; cursor: pointer; accent-color: var(--accent); }
  @media (max-width: 600px) {
    .capability-group { padding: 16px 14px 0; }
    .capability-row { gap: 12px; flex-wrap: wrap; }
  }
</style>

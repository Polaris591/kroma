<script module lang="ts">
  import { writable } from 'svelte/store';

  type Draft = {
    name: string;
    base_url: string;
    model: string;
    api_key: string;
    clear_key: boolean;
    protocol: string;
    reasoning: string;
    deepseek_thinking: string;
    context_window: string | number;
    max_output_tokens: string | number;
    native_web_search: boolean;
  };

  // Keep drafts only for this app runtime, including while the page is unmounted.
  const drafts = new Map<string, Draft>();
  const editor = writable({
    selected: '',
    draft: {
      name: '', base_url: 'https://api.openai.com/v1', model: '', api_key: '',
      clear_key: false, protocol: 'chat', reasoning: 'auto', deepseek_thinking: 'off',
      context_window: '', max_output_tokens: '', native_web_search: false,
    } as Draft,
    dirty: false,
    operation: '' as '' | 'save' | 'connect' | 'default',
    message: '',
    error: '',
  });
</script>

<script lang="ts">
  import type { Profiles, ProfileInput } from '../lib/api';
  import { PROFILE_NAME_PATTERN } from '../lib/profile-name';
  import { Save, PlugZap, KeyRound, Plus, Cpu, Check, Star, HardDrive } from '@lucide/svelte';
  export let data: Profiles;
  export let connected = '';
  export let onSave: (input: ProfileInput) => Promise<void>;
  export let onConnect: (name: string) => Promise<void>;
  export let onSetDefault: (name: string) => Promise<void>;
  export let onAdd: () => void;
  export let onOpenStorage: () => void = () => {};

  let form: HTMLFormElement;
  $: busy = $editor.operation !== '';

  function discardDraft() {
    if (busy) return;
    const id = $editor.selected;
    drafts.delete(id);
    $editor.selected = '';
    choose(id);
  }

  function choose(id: string) {
    if ($editor.operation || $editor.selected === id) return;
    const p = data.profiles.find(v => v.name === id);
    if (!p) return;
    if ($editor.dirty) drafts.set($editor.selected, {...$editor.draft});
    const draft = drafts.get(id);
    $editor = {
      selected: id,
      draft: draft ? {...draft} : {
        name: p.name, base_url: p.base_url, model: p.model, api_key: '', clear_key: false,
        protocol: p.protocol, reasoning: p.reasoning, deepseek_thinking: p.deepseek_thinking,
        context_window: p.context_window ?? '', max_output_tokens: p.max_output_tokens ?? '',
        native_web_search: p.native_web_search ?? false,
      },
      dirty: !!draft, operation: '', message: '', error: '',
    };
  }

  $: if (!$editor.selected && data.profiles.length > 0) {
    choose(data.profiles.find(p => p.name === data.active)?.name ?? data.profiles[0].name);
  }

  function markDirty() {
    $editor.dirty = true;
    $editor.message = '';
    $editor.error = '';
  }

  async function setDefault() {
    if ($editor.operation || $editor.dirty || !$editor.selected) return;
    $editor.operation = 'default';
    $editor.message = '';
    $editor.error = '';
    try { await onSetDefault($editor.selected); }
    catch { $editor.error = '设置默认模型未完成，请查看上方错误提示后重试。'; }
    finally { $editor.operation = ''; }
  }

  async function save(connectAfter: boolean) {
    if ($editor.operation || !form.reportValidity()) return;
    $editor.operation = 'save';
    $editor.message = '';
    $editor.error = '';
    let saved = false;
    try {
      if ($editor.dirty || !$editor.selected) {
        const draft = {...$editor.draft};
        await onSave({
          ...draft, api_key: draft.api_key || undefined,
          context_window: draft.context_window ? Number(draft.context_window) : null,
          max_output_tokens: draft.max_output_tokens ? Number(draft.max_output_tokens) : null,
        });
        drafts.delete($editor.selected);
        $editor.selected = draft.name;
        $editor.draft.api_key = '';
        $editor.draft.clear_key = false;
        $editor.dirty = false;
        saved = true;
        $editor.message = '配置已保存。';
      }
      if (connectAfter) {
        $editor.operation = 'connect';
        await onConnect($editor.selected);
      }
    } catch {
      $editor.error = saved
        ? '配置已保存，但连接未完成。请查看上方错误提示后重试连接。'
        : '操作未完成，输入已保留。请查看上方错误提示后重试。';
    } finally { $editor.operation = ''; }
  }
</script>

<section class="settings-page" aria-labelledby="settings-heading">
  <div class="section-heading">
    <div><h2 id="settings-heading">模型与连接</h2><p>管理接口、认证与模型参数。配置与 CLI / TUI 共享。</p></div>
    {#if data.profiles.length}<button class="primary small" disabled={busy} onclick={onAdd}><Plus size={15}/> 添加模型</button>{/if}
  </div>
  {#if data.profiles.length}
    <div class="model-settings-layout">
      <div class="model-profile-pane">
        <div class="settings-pane-heading"><span>模型配置</span><small>{data.profiles.length}</small></div>
        <div class="profile-switch" role="group" aria-label="选择模型配置">
          {#each data.profiles as p (p.name)}
            <button class:active={$editor.selected === p.name} disabled={busy}
              aria-pressed={$editor.selected === p.name} onclick={() => choose(p.name)} title={p.model}>
              <Cpu size={16}/>
              <span class="profile-option-label">
                <strong>{p.name}</strong><small>{p.model}</small>
                {#if data.active === p.name || connected === p.name}
                  <span class="profile-option-meta">{data.active === p.name ? '启动默认' : ''}{data.active === p.name && connected === p.name ? ' · ' : ''}{connected === p.name ? '已连接' : ''}</span>
                {/if}
                {#if drafts.has(p.name) || ($editor.selected === p.name && $editor.dirty)}<span class="profile-draft-label">未保存</span>{/if}
              </span>
              {#if $editor.selected===p.name}<Check size={14} class="profile-selected"/>{/if}
            </button>
          {/each}
        </div>
        <p class="profile-pane-note">选中配置只查看和编辑，<br/>点击“连接”才会切换模型。</p>
      </div>
      <form bind:this={form} onsubmit={(e) => {e.preventDefault(); void save(true);}} class="settings-form" aria-busy={busy}>
        <div class="settings-editor-heading">
          <div><h3>{$editor.selected}</h3><p>连接设置</p></div>
          {#if $editor.dirty}<span class="state-badge pending">未保存</span>
          {:else if connected === $editor.selected}<span class="state-badge connected"><span></span>当前连接</span>{/if}
        </div>
        <fieldset disabled={busy}>
          <legend class="sr-only">编辑模型配置</legend>
          <section class="settings-field-section" aria-label="基本信息">
            <h4>基本信息</h4>
            <div class="field-grid">
              <label>配置名称 <input required pattern={PROFILE_NAME_PATTERN} bind:value={$editor.draft.name} oninput={markDirty} placeholder="default"/></label>
              <label>模型 ID <input required spellcheck={false} bind:value={$editor.draft.model} oninput={markDirty} placeholder="model-name"/></label>
            </div>
            <div class="field-grid">
              <label>API Base URL <input required type="url" spellcheck={false} bind:value={$editor.draft.base_url} oninput={markDirty} placeholder="https://api.openai.com/v1"/></label>
              <label>API 协议
                <select bind:value={$editor.draft.protocol} onchange={markDirty}>
                  <option value="chat">OpenAI Chat</option><option value="responses">OpenAI Responses</option><option value="anthropic">Anthropic Messages</option>
                </select>
              </label>
            </div>
          </section>
          <section class="settings-field-section" aria-label="认证信息">
            <h4><KeyRound size={14}/><label for="settings-api-key">API Key</label></h4>
            <input id="settings-api-key" type="password" autocomplete="off" spellcheck={false}
              disabled={$editor.draft.clear_key} aria-describedby="settings-key-help"
              bind:value={$editor.draft.api_key} oninput={markDirty} placeholder="留空保留现有密钥或环境变量认证" />
            <p id="settings-key-help" class="field-caption">密钥不会回显。勾选清除时，优先删除已保存的认证。</p>
            <label class="inline-check"><input type="checkbox" bind:checked={$editor.draft.clear_key} onchange={markDirty}/>清除已保存的 API Key（含环境变量引用）</label>
          </section>
          <div class="settings-advanced-section">
            <details class="advanced-settings">
              <summary>高级参数 <span>思考、上下文、输出与联网</span></summary>
              <div class="field-grid">
                <label>默认思考强度
                  <select bind:value={$editor.draft.reasoning} onchange={markDirty}>
                    {#each ['auto', 'off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as level}<option value={level}>{level}</option>{/each}
                  </select>
                </label>
                <label>DeepSeek Thinking
                  <select bind:value={$editor.draft.deepseek_thinking} onchange={markDirty}>
                    {#each ['off', 'enabled', 'high', 'max'] as level}<option value={level}>{level}</option>{/each}
                  </select>
                </label>
                <label>上下文窗口（可选） <input type="number" min="1024" step="1" bind:value={$editor.draft.context_window} oninput={markDirty} placeholder="自动发现"/></label>
                <label>最大输出 Token（可选） <input type="number" min="1" step="1" bind:value={$editor.draft.max_output_tokens} oninput={markDirty} placeholder="自动发现"/></label>
              </div>
              <label class="inline-check"><input type="checkbox" bind:checked={$editor.draft.native_web_search} onchange={markDirty}/>启用模型原生联网搜索（需上游支持，可能单独计费）</label>
            </details>
          </div>
          <div class="settings-action-area">
            <div class="form-actions">
              <button type="submit" class="primary" disabled={busy || !$editor.draft.name || !$editor.draft.model}><PlugZap size={16}/> {$editor.operation === 'connect' ? '正在连接…' : $editor.operation === 'save' ? '正在保存…' : $editor.dirty || !$editor.selected ? '保存并连接' : '连接此模型'}</button>
              <button type="button" class="secondary" disabled={busy || !$editor.draft.name || !$editor.draft.model || !$editor.dirty}
                onclick={() => void save(false)}><Save size={15}/> 仅保存</button>
              {#if $editor.selected && $editor.selected !== data.active}
                <button type="button" class="secondary" disabled={busy || $editor.dirty} title="设为 CLI、TUI 与 Desktop 的启动默认"
                  onclick={() => void setDefault()}><Star size={14}/> {$editor.operation === 'default' ? '正在设置…' : '设为默认'}</button>
              {/if}
            </div>
            <p class="settings-hint">仅保存不切换模型，也不更改启动默认。</p>
            {#if $editor.dirty}<button type="button" class="quiet-action discard-profile-draft" onclick={discardDraft}>撤销未保存修改</button>{/if}
            <p class="settings-hint draft-retention-note">切换配置或页面会保留草稿，关闭应用后不保留。</p>
          </div>
        </fieldset>
        {#if busy || $editor.message || $editor.error}
          <div class="settings-feedback">
            <p class="settings-hint" role="status">{busy ? $editor.operation === 'connect' ? '正在连接模型…' : $editor.operation === 'default' ? '正在设置默认模型…' : '正在保存配置…' : $editor.message}</p>
            {#if $editor.error}<p class="settings-error" role="alert">{$editor.error}</p>{/if}
          </div>
        {/if}
      </form>
    </div>
  {:else}
    <div class="settings-empty-guide"><Cpu size={24}/><strong>连接你的第一个模型</strong><span>添加 API 地址与模型信息，即可开始在工作区中协作。</span><button class="primary" disabled={busy} onclick={onAdd}><Plus size={15}/> 添加模型</button></div>
  {/if}
  <div class="settings-foot">
    <button class="quiet-action" onclick={onOpenStorage}><HardDrive size={15}/> 本机数据与清理</button>
    <span>工作区占用、空会话清理与本机存储。</span>
  </div>
</section>

<style>
  fieldset { display: flex; flex-direction: column; min-width: 0; margin: 0; padding: 0; border: 0; }
  .settings-hint, .settings-error { margin: 0; font-size: 11.5px; line-height: 1.65; }
  .settings-hint { color: var(--muted); }
  .settings-error { color: var(--danger); }
</style>

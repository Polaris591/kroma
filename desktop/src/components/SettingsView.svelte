<script lang="ts">
  import type { Profiles, ProfileInput } from '../lib/api';
  import { Save, PlugZap, KeyRound, Plus, Cpu, Check, Star } from '@lucide/svelte';
  export let data: Profiles;
  export let onSave: (input: ProfileInput) => Promise<void>;
  export let onConnect: (name: string) => Promise<void>;
  export let onSetDefault: (name: string) => Promise<void>;
  export let onAdd: () => void;
  let selected = '';
  let name = '';
  let base_url = 'https://api.openai.com/v1';
  let model = '';
  let api_key = '';
  let clear_key = false;
  let protocol = 'chat';
  let reasoning = 'auto';
  let deepseek_thinking = 'off';
  let context_window = '';
  let max_output_tokens = '';
  let native_web_search = false;
  let busy = false;
  let dirty = false;

  function choose(id: string) {
    const p = data.profiles.find(v => v.name === id);
    if (!p) return;
    selected = id; name = p.name; base_url = p.base_url; model = p.model;
    api_key = ''; clear_key = false; protocol = p.protocol;
    reasoning = p.reasoning; deepseek_thinking = p.deepseek_thinking;
    context_window = String(p.context_window ?? '');
    max_output_tokens = String(p.max_output_tokens ?? '');
    native_web_search = p.native_web_search ?? false;
    dirty = false;
  }
  $: if (!dirty && !selected && data.profiles.length > 0) {
    choose(data.active ?? data.profiles[0].name);
  }
  async function setDefault() {
    if (busy || dirty || !selected) return;
    busy = true;
    try { await onSetDefault(selected); }
    finally { busy = false; }
  }

  async function save(connectAfter: boolean) {
    if (busy) return;
    busy = true;
    try {
      if (dirty || !selected) {
        await onSave({
          name, base_url, model, api_key: api_key || undefined, clear_key,
          protocol, reasoning, deepseek_thinking,
          context_window: context_window ? Number(context_window) : null,
          max_output_tokens: max_output_tokens ? Number(max_output_tokens) : null,
          native_web_search,
        });
        selected = name; api_key = ''; clear_key = false; dirty = false;
      }
      if (connectAfter) await onConnect(selected);
    } catch {
      // The host callback already shows the error in the main banner.
      // Keep the form intact so the user can correct it without retyping.
    } finally { busy = false; }
  }
</script>

<section class="settings-page">
  <div class="section-heading">
    <div><h2>模型与连接</h2><p>模型配置与 Kroma CLI / TUI 共享。</p></div>
    <button class="primary small" onclick={onAdd}><Plus size={15}/> 添加模型</button>
  </div>
  {#if data.profiles.length}
    <div class="profile-switch">
      {#each data.profiles as p}
        <button class:active={selected === p.name} onclick={() => choose(p.name)} title={p.model}>
          <Cpu size={17}/><span class="profile-option-label"><strong>{p.name}</strong><small>{p.model}</small></span>
          {#if data.active===p.name}<span class="profile-default-label">默认</span>{/if}
          {#if selected===p.name}<Check size={15} class="profile-selected"/>{/if}
        </button>
      {/each}
    </div>
  {:else}
    <div class="settings-empty-guide"><Cpu size={20}/><strong>还没有添加模型</strong><span>点击右上角添加模型，填写接口信息即可连接。</span></div>
  {/if}
  {#if data.profiles.length}
  <form onsubmit={(e) => {e.preventDefault(); void save(true);}} class="settings-form">
    <label>配置名称 <input required pattern={'[A-Za-z0-9][A-Za-z0-9_.-]{0,47}'} bind:value={name} oninput={() => dirty = true} placeholder="default"/></label>
    <label>API Base URL <input required bind:value={base_url} oninput={() => dirty = true} placeholder="https://api.openai.com/v1"/></label>
    <label>模型 ID <input required bind:value={model} oninput={() => dirty = true} placeholder="model-name"/></label>
    <label>API Key <span class="field-caption"><KeyRound size={13}/> 留空保留已保存的密钥</span>
      <input type="password" autocomplete="off" bind:value={api_key} oninput={() => dirty = true} placeholder="sk-..." />
    </label>
    <label class="inline-check"><input type="checkbox" bind:checked={clear_key} oninput={() => dirty = true}/>清除已保存的 API Key</label>
    <label>API 协议
      <select bind:value={protocol} onchange={() => dirty = true}>
        <option value="chat">OpenAI Chat</option><option value="responses">OpenAI Responses</option><option value="anthropic">Anthropic Messages</option>
      </select>
    </label>
    <details class="advanced-settings">
      <summary>高级参数 <span>思考、上下文、输出与原生联网</span></summary>
      <div class="field-grid">
        <label>默认思考强度
          <select bind:value={reasoning} onchange={() => dirty = true}>
            {#each ['auto', 'off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as level}
              <option value={level}>{level}</option>
            {/each}
          </select>
        </label>
        <label>DeepSeek Thinking
          <select bind:value={deepseek_thinking} onchange={() => dirty = true}>
            {#each ['off', 'enabled', 'high', 'max'] as level}<option value={level}>{level}</option>{/each}
          </select>
        </label>
        <label>上下文窗口（可选） <input type="number" min="1024" bind:value={context_window} oninput={() => dirty = true} placeholder="自动发现"/></label>
        <label>最大输出 Token（可选） <input type="number" min="1" bind:value={max_output_tokens} oninput={() => dirty = true} placeholder="自动发现"/></label>
      </div>
      <label class="inline-check"><input type="checkbox" bind:checked={native_web_search} onchange={() => dirty = true}/>启用模型原生联网搜索（需上游支持，可能单独计费）</label>
    </details>
    <div class="form-actions">
      <button type="submit" class="primary" disabled={busy || !name || !model}><PlugZap size={16}/> {busy ? '处理中…' : dirty || !selected ? '保存并连接' : '连接此模型'}</button>
      <button type="button" class="secondary" disabled={busy || !name || !model || !dirty} onclick={() => void save(false)}><Save size={16}/> 仅保存</button>
      {#if selected && selected !== data.active}
        <button type="button" class="secondary" disabled={busy || dirty} title="设为 CLI、TUI 与 Desktop 的启动默认"
          onclick={() => void setDefault()}><Star size={15}/> 设为默认</button>
      {/if}
    </div>
  </form>
  {/if}
</section>

<script lang="ts">
  import type { Profiles, ProfileInput } from '../lib/api';
  import { Save, PlugZap, KeyRound } from '@lucide/svelte';
  export let data: Profiles;
  export let onSave: (input: ProfileInput) => Promise<void>;
  export let onConnect: (name: string) => Promise<void>;
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
    dirty = false;
  }
  function blank() {
    selected = ''; name = ''; base_url = 'https://api.openai.com/v1';
    model = ''; api_key = ''; clear_key = false; protocol = 'chat';
    reasoning = 'auto'; deepseek_thinking = 'off'; context_window = ''; max_output_tokens = '';
    dirty = true;
  }
  $: if (!dirty && !selected && data.profiles.length > 0) {
    choose(data.active ?? data.profiles[0].name);
  }
  async function save() {
    busy = true;
    try {
      await onSave({
        name, base_url, model, api_key: api_key || undefined, clear_key,
        protocol, reasoning, deepseek_thinking,
        context_window: context_window ? Number(context_window) : null,
        max_output_tokens: max_output_tokens ? Number(max_output_tokens) : null,
      });
      selected = name; api_key = ''; clear_key = false; dirty = false;
    } finally { busy = false; }
  }
</script>

<section class="settings-page">
  <div class="section-heading">
    <div><h2>模型与连接</h2><p>使用与 Kroma CLI / TUI 相同的 Provider 配置。</p></div>
    <button class="secondary small" onclick={blank}>+ 新建配置</button>
  </div>
  {#if data.profiles.length}
    <div class="profile-switch">
      {#each data.profiles as p}
        <button class:active={selected === p.name} onclick={() => choose(p.name)}>{p.name}</button>
      {/each}
    </div>
  {/if}
  <form onsubmit={(e) => {e.preventDefault(); void save();}} class="settings-form">
    <label>配置名称 <input required pattern={'[A-Za-z0-9][A-Za-z0-9_.-]{0,47}'} bind:value={name} oninput={() => dirty = true} placeholder="default"/></label>
    <label>API Base URL <input required bind:value={base_url} oninput={() => dirty = true} placeholder="https://api.openai.com/v1"/></label>
    <label>模型 ID <input required bind:value={model} oninput={() => dirty = true} placeholder="model-name"/></label>
    <label>API Key <span class="field-caption"><KeyRound size={13}/> 留空保留已保存的密钥</span>
      <input type="password" autocomplete="off" bind:value={api_key} oninput={() => dirty = true} placeholder="sk-..." />
    </label>
    <label class="inline-check"><input type="checkbox" bind:checked={clear_key} oninput={() => dirty = true}/>清除已保存的 API Key</label>
    <div class="field-grid">
      <label>API 协议
        <select bind:value={protocol} onchange={() => dirty = true}>
          <option value="chat">OpenAI Chat</option><option value="responses">OpenAI Responses</option><option value="anthropic">Anthropic Messages</option>
        </select>
      </label>
      <label>思考强度
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
    <div class="form-actions">
      <button type="submit" class="primary" disabled={busy || !name || !model}><Save size={16}/> {busy ? '保存中…' : '保存配置'}</button>
      <button type="button" class="secondary" disabled={busy || !selected} onclick={() => onConnect(selected)}><PlugZap size={16}/> 连接此模型</button>
    </div>
  </form>
</section>

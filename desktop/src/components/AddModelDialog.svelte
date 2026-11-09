<script lang="ts">
  import { onMount } from 'svelte';
  import { X, Cpu, PlugZap } from '@lucide/svelte';
  import type { ProfileInput, ProfileSummary } from '../lib/api';
  export let profiles: ProfileSummary[] = [];
  export let onSaveConnect: (input: ProfileInput) => Promise<void>;
  export let onClose: () => void;
  let protocol: 'chat'|'responses'|'anthropic' = 'chat';
  let name = '';
  let model = '';
  let base_url = 'https://api.openai.com/v1';
  let api_key = '';
  let saving = false;
  let error = '';
  let modelInput: HTMLInputElement;
  onMount(() => modelInput?.focus());
  function setProtocol(value: typeof protocol) {
    protocol = value;
    if (value === 'anthropic' && base_url === 'https://api.openai.com/v1') base_url = 'https://api.anthropic.com/v1';
    if (value !== 'anthropic' && base_url === 'https://api.anthropic.com/v1') base_url = 'https://api.openai.com/v1';
  }
  function suggestedName() {
    const base = model.trim().toLowerCase().replace(/[^a-z0-9_.-]/g,'-').replace(/^[^a-z0-9]+/,'').slice(0,36) || 'model';
    let candidate = base;
    let suffix = 2;
    while (profiles.some(p=>p.name===candidate)) candidate = (base.slice(0, 35) + '-' + suffix++).slice(0,48);
    return candidate;
  }
  async function submit() {
    if (saving) return;
    const resolved = name.trim() || suggestedName();
    if (profiles.some(p=>p.name===resolved)) {error='配置名称已存在，请使用新名称';return;}
    if (!model.trim() || !base_url.trim()) {error='请填写模型 ID 和接口地址';return;}
    saving=true;error='';
    try {
      await onSaveConnect({
        name:resolved,model:model.trim(),base_url:base_url.trim(),api_key:api_key.trim() || undefined,
        protocol,reasoning:'auto',deepseek_thinking:'off'
      });
      onClose();
    } catch (e) {error=e instanceof Error?e.message:String(e);}
    finally {saving=false;}
  }
</script>
<svelte:window onkeydown={e=>{if(e.key==='Escape'&&!saving) onClose();}}/>
<div class="dialog-overlay" role="presentation">
  <div class="dialog-dim"></div>
  <div class="dialog-panel model-create" role="dialog" aria-modal="true" aria-label="添加模型">
    <header class="dialog-heading"><span class="dialog-emblem"><Cpu size={20}/></span>
      <div><h2>添加模型</h2><p>连接 API，立即开始对话。</p></div>
      <button class="dialog-close" title="关闭" onclick={onClose} disabled={saving}><X size={17}/></button>
    </header>
    <form onsubmit={e=>{e.preventDefault();void submit();}}>
      <span class="dialog-field-label">API 协议</span>
      <div class="protocol-pills">
        <button type="button" class:active={protocol==='chat'} onclick={()=>setProtocol('chat')}>OpenAI 兼容</button>
        <button type="button" class:active={protocol==='responses'} onclick={()=>setProtocol('responses')}>Responses</button>
        <button type="button" class:active={protocol==='anthropic'} onclick={()=>setProtocol('anthropic')}>Anthropic</button>
      </div>
      <label class="dialog-label">接口地址 <input required type="url" bind:value={base_url} placeholder="https://…/v1" autocomplete="url"/></label>
      <label class="dialog-label">模型 ID <input required bind:this={modelInput} bind:value={model} placeholder="例如 deepseek-v4.1-flash"/></label>
      <label class="dialog-label">API Key <input type="password" bind:value={api_key} placeholder="可留空，使用现有上游认证" autocomplete="off"/></label>
      <label class="dialog-label">配置名称 <span>可选，自动生成</span><input bind:value={name} placeholder={suggestedName()} pattern={'[A-Za-z0-9][A-Za-z0-9_.-]{0,47}'}/></label>
      {#if error}<p class="dialog-error" role="alert">{error}</p>{/if}
      <div class="dialog-actions">
        <button type="button" class="dialog-secondary" onclick={onClose} disabled={saving}>取消</button>
        <button type="submit" class="dialog-primary" disabled={saving||!model.trim()||!base_url.trim()}><PlugZap size={16}/>{saving?'正在连接…':'保存并连接'}</button>
      </div>
    </form>
  </div>
</div>
<script lang="ts">
  import type { Message } from '../lib/api';
  import Markdown from './Markdown.svelte';
  import { ChevronRight, Terminal, BrainCircuit, UserRound, Sparkles } from '@lucide/svelte';
  export let message: Message;
  $: role = message.role;
  $: isUser = role === 'user';
  $: isTool = role === 'tool';
  $: text = message.content ?? '';
  $: attachments = (message.provider_items ?? []).filter((p) => p.type === 'kroma_attachment');
  $: display = text.replace(/[\uE000-\uF8FF]/g, '');
</script>

{#if role !== 'system'}
<article class:user={isUser} class:tool={isTool} class="message">
  <div class="avatar" aria-hidden="true">
    {#if isUser}<UserRound size={16}/>
    {:else if isTool}<Terminal size={16}/>
    {:else}<Sparkles size={17}/>{/if}
  </div>
  <div class="message-body">
    <div class="message-author">{isUser ? '你' : isTool ? '工具结果' : 'Kroma'}</div>
    {#if message.reasoning_content}
      <details class="reasoning-block">
        <summary><BrainCircuit size={15}/> 思考过程 <ChevronRight size={14} class="disclosure"/></summary>
        <div class="reasoning-text">{message.reasoning_content}</div>
      </details>
    {/if}
    {#if isTool}
      <details class="tool-result" class:failed={message.provider_items?.some(p => p.type === 'kroma_tool_status' && p.is_error === true)}>
        <summary><Terminal size={15}/><span>展开工具输出</span><ChevronRight size={14} class="disclosure"/></summary>
        <pre>{text.slice(0, 30000)}{text.length > 30000 ? '\n\n[输出过长，其余内容保存在历史记录中]' : ''}</pre>
      </details>
    {:else}
      {#if display}<Markdown content={display}/>{/if}
      {#if message.tool_calls?.length}
        {#each message.tool_calls as call (call.id)}
          <details class="tool-result">
            <summary><Terminal size={15}/><span>{call.function?.name || '调用工具'}</span><ChevronRight size={14} class="disclosure"/></summary>
            <pre>{call.function?.arguments ?? '{}'}</pre>
          </details>
        {/each}
      {/if}
      {#if attachments.length}
        <div class="attachment-chips">
          {#each attachments as attachment}
            <span class="attachment-chip">{String(attachment.name ?? '附件')}</span>
          {/each}
        </div>
      {/if}
    {/if}
  </div>
</article>
{/if}

<script lang="ts">
  import type { Message } from '../lib/api';
  import Markdown from './Markdown.svelte';
  import { ChevronRight, Terminal, BrainCircuit } from '@lucide/svelte';

  export let message: Message;
  $: role = message.role;
  $: isUser = role === 'user';
  $: isTool = role === 'tool';
  $: text = message.content ?? '';
  $: attachments = (message.provider_items ?? []).filter((p) => p.type === 'kroma_attachment');
  $: display = text.replace(/[\uE000-\uF8FF]/g, '');
  $: toolFailed = isTool && !!message.provider_items?.some(
    (p) => p.type === 'kroma_tool_status' && p.is_error === true
  );
</script>

{#if role !== 'system'}
  <article class="message" class:user={isUser} class:tool={isTool} class:assistant={!isUser && !isTool}>
    {#if !isUser}
      <div class="avatar" class:tool-avatar={isTool} aria-hidden="true">
        {#if isTool}<Terminal size={16}/>{:else}K{/if}
      </div>
    {/if}
    <div class="message-body">
      {#if isUser}
        <div class="user-bubble">
          {#if display}<div class="user-text">{display}</div>{/if}
          {#if attachments.length}
            <div class="attachment-chips">
              {#each attachments as attachment}
                <span class="attachment-chip">{String(attachment.name ?? '附件')}</span>
              {/each}
            </div>
          {/if}
        </div>
      {:else}
        <div class="message-author">{isTool ? '工具结果' : 'Kroma'}</div>
        {#if message.reasoning_content}
          <details class="reasoning-block">
            <summary><BrainCircuit size={15}/> 思考过程 <ChevronRight size={14} class="disclosure"/></summary>
            <div class="reasoning-text">{message.reasoning_content}</div>
          </details>
        {/if}
        {#if isTool}
          <details class="tool-result" class:failed={toolFailed}>
            <summary><Terminal size={15}/><span>{toolFailed ? '工具执行失败' : '查看工具输出'}</span><span class="tool-state" class:error={toolFailed}>{toolFailed ? '失败' : '完成'}</span><ChevronRight size={14} class="disclosure"/></summary>
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
      {/if}
    </div>
  </article>
{/if}

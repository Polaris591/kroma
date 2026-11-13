<script lang="ts">
  import { ChevronRight, CircleHelp, Layers, Wrench } from '@lucide/svelte';
  import { compactCount } from '../lib/format';
  import { contextDisplay } from '../lib/context-display';
  import { popover } from '../lib/popover';
  import type { ContextReport } from '../lib/api';
  export let total: number | null = null;
  /// Measured composition of the next request. When present it replaces the
  /// old `window - available_input` ring, which only ever showed the output
  /// reserve and therefore never moved.
  export let report: ContextReport | null = null;
  export let usage: { input: number; output: number } | null = null;
  export let cache = '—';
  export let toolNames: string[] = [];
  export let skills: Array<{name: string; description?: string}> = [];
  export let onCompact: (() => Promise<void>) | null = null;
  export let onShowAbilities: (() => void) | null = null;
  export let compacting = false;
  export let compactDisabled = false;

  let showDetails = false;
  let trigger: HTMLButtonElement;
  const circumference = 2 * Math.PI * 12;

  const LABELS: Record<string, string> = {
    system: '系统提示词',
    guidance: '内置指引',
    instructions: '项目指令',
    extension: '扩展注入',
    tools: '工具定义',
    mcp: 'MCP 工具',
    messages: '对话消息',
  };

  $: ({capacity, segments: measuredSegments, used, reserve: outputReserve, free, fraction, percent} = contextDisplay(report, total));
  $: indicator = fraction >= .9 ? 'var(--danger)' : fraction >= .75 ? 'var(--warning)' : 'var(--accent)';
  $: progressLength = circumference * fraction;

  // The bar answers "what is inside my context", so it is normalised against
  // the used part: a 4.6k composition inside a 500k window would otherwise
  // collapse into invisible slivers. Window fullness stays in the ring.
  type Slice = { key: string; label: string; tokens: number; percent: number };
  $: slices = measuredSegments.map(segment => ({
    key: segment.key,
    label: LABELS[segment.key] ?? segment.key,
    tokens: segment.tokens,
    percent: used !== null && used > 0 ? segment.tokens / used * 100 : 0,
  }));
</script>

<div class="context-widget">
  <button class="context-trigger" aria-expanded={showDetails} aria-haspopup="dialog" bind:this={trigger}
    aria-label={percent === null ? '上下文容量未知，点击查看详情' : `上下文占用估算 ${percent}%，点击查看详情`}
    title="上下文与用量详情" onclick={() => showDetails = !showDetails}>
    <svg class="context-ring-svg" viewBox="0 0 32 32" width="29" height="29" aria-hidden="true">
      <circle cx="16" cy="16" r="12" fill="none" stroke="var(--line)" stroke-width="3"/>
      {#if percent !== null && progressLength > 0}
        <circle cx="16" cy="16" r="12" fill="none" stroke={indicator}
          stroke-width="3" stroke-linecap="round"
          stroke-dasharray={`${progressLength} ${circumference}`}
          transform="rotate(-90 16 16)"/>
      {/if}
    </svg>
    <span class="context-trigger-text">{percent === null ? '上下文 —' : `上下文 ${percent}%`}</span>
  </button>
  {#if showDetails}
    <div class="context-popover" role="dialog" aria-label="上下文使用详情" tabindex="-1"
      use:popover={{trigger, onClose: () => showDetails = false}}>
      <div class="context-popover-heading"><strong>上下文窗口</strong><CircleHelp size={15}/></div>
      <div class="context-big">{percent === null ? '—' : `${percent}%`} <span>{percent === null ? '尚无占用估算' : '估算占用'}</span></div>

      {#if slices.length}
        <div class="context-stack" aria-hidden="true">
          {#each slices as slice (slice.key)}
            <span class={`context-slice ${slice.key}`} style:width={`${slice.percent}%`}></span>
          {/each}
        </div>
        <div class="context-legend">
          {#each slices as slice (slice.key)}
            <div class="context-legend-row">
              <span class={`legend-dot ${slice.key}`}></span>
              <span class="legend-label">{slice.label}</span>
              <strong class="legend-value">{compactCount(slice.tokens)}</strong>
              <span class="legend-share">{(slice.percent).toFixed(slice.percent < 1 ? 1 : 0)}%</span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="context-progress"><span style:width={`${percent ?? 0}%`}></span></div>
      {/if}

      <div class="context-kv"><span>已占用合计</span><strong>{compactCount(used)} tokens</strong></div>
      <div class="context-kv"><span>输出预留</span><strong>{compactCount(outputReserve)} tokens</strong></div>
      <div class="context-kv"><span>当前可用输入空间</span><strong>{compactCount(free)} tokens</strong></div>
      <div class="context-kv"><span>上下文总容量</span><strong>{capacity ? compactCount(capacity) : '—'} tokens</strong></div>
      <p>{percent === null ? '尚未取得请求内容的占用估算；已知总容量不代表当前会话占用为零。' : '条状图展示已占用内容的构成。占用按下一次请求内容估算，不等同于计费 Token；可用输入空间同时受模型输入上限和输出预留约束。'}</p>
      {#if onCompact && percent !== null && percent >= 75}
        <button class="context-action" disabled={compacting || compactDisabled}
          title={compactDisabled ? '请在当前任务或操作结束后压缩' : '压缩下一次请求的上下文，保留原始记录'}
          onclick={() => void onCompact?.()}>
          <Layers size={14}/>
          <span class="context-action-text"><strong>{compacting ? '正在压缩…' : '压缩上下文'}</strong><small>只压缩下一次请求，原始历史保留</small></span>
        </button>
      {/if}
      {#if onShowAbilities}
        <button class="context-abilities" onclick={onShowAbilities}>
          <Wrench size={14}/>
          <span>已加载 {toolNames.length} 个工具{#if skills.length} · {skills.length} 个技能{/if}</span>
          <ChevronRight size={13}/>
        </button>
      {/if}
      <div class="context-section-label">本会话累计</div>
      <div class="context-kv"><span>输入</span><strong class="metric">{compactCount(usage?.input)}</strong></div>
      <div class="context-kv"><span>输出</span><strong class="metric">{compactCount(usage?.output)}</strong></div>
      <div class="context-kv"><span>缓存命中</span><strong class="metric">{cache}</strong></div>
    </div>
  {/if}
</div>

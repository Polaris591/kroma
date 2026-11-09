<script lang="ts">
  import { ChevronRight, Check, CircleAlert, Copy, FileDiff, GitFork, Pencil, Undo2 } from '@lucide/svelte';
  import type { TranscriptTurn } from '../lib/transcript';
  import type { TurnDiffSummary } from '../lib/api';
  import { summarizeTurn } from '../lib/transcript';
  import Markdown from './Markdown.svelte';
  import ToolDetail from './ToolDetail.svelte';

  export let turn: TranscriptTurn;
  export let showWorkspaceChanges = false;
  export let changeCount = 0;
  export let onViewChanges: () => void = () => {};
  /// Measured file changes for this turn, from the pre-task snapshot.
  export let changes: TurnDiffSummary | null = null;
  export let onViewTurnChanges: () => void = () => {};
  /// Rewind and fork stay hidden until hover: they are destructive or
  /// context-changing, so they must not compete with the answer itself.
  export let onRewind: (() => void) | null = null;
  export let onFork: (() => void) | null = null;
  export let onEdit: (() => void) | null = null;
  $: display = summarizeTurn(turn);
  $: answerReasoning = display.answer?.reasoning?.trim() ?? '';
  $: hasReasoning = !!answerReasoning
    || display.process.some(step => step.kind === 'assistant' && step.reasoning.trim().length > 0);
  $: hasProcess = display.process.length > 0 || !!answerReasoning;
  $: stepLabel = processLabel(display.toolCount, hasReasoning);
  let copied = false;
  let copyError = false;
  let processExpanded = false;

  // One quiet line describes the whole preamble (reasoning and tool calls),
  // so a finished answer starts with its content, not with two stacked rows.
  function processLabel(tools: number, reasoning: boolean): string {
    if (tools) return reasoning ? `思考 · ${tools} 次工具调用` : `${tools} 次工具调用`;
    return reasoning ? '思考过程' : '中间回复';
  }

  async function copyAnswer() {
    if (!display.answer) return;
    try {
      await navigator.clipboard.writeText(display.answer.text);
      copied = true;
      copyError = false;
    } catch {
      copyError = true;
      copied = false;
    }
  }
</script>

<div class="conversation-turn">
  {#if turn.user}
    <div class="message user">
      <div class="message-body">
        <div class="user-bubble">
          {#if turn.user.content}<span class="user-text">{turn.user.content.replace(/[\uE000-\uF8FF]/g, '')}</span>{/if}
          {#each (turn.user.provider_items ?? []).filter(p => p.type === 'kroma_attachment') as item}
            <span class="attachment-chip">{String(item.name ?? '附件')}</span>
          {/each}
        </div>
        {#if onRewind || onFork || onEdit}
          <div class="user-actions">
            {#if onEdit}
              <button class="turn-pill" onclick={onEdit} title="回到这条提问并把内容填回输入框">
                <Pencil size={13}/>编辑
              </button>
            {/if}
            {#if onRewind}
              <button class="turn-pill" onclick={onRewind} title="回到这条提问之前">
                <Undo2 size={13}/>回退
              </button>
            {/if}
            {#if onFork}
              <button class="turn-pill" onclick={onFork} title="从这条提问分叉出新会话">
                <GitFork size={13}/>分叉
              </button>
            {/if}
          </div>
        {/if}
      </div>
    </div>
  {/if}
  {#if turn.activities.length}
    <article class="message assistant turn-response">
      <div class="avatar agent-avatar" aria-hidden="true">K</div>
      <div class="message-body">
        <div class="message-author">
          <span>Kroma</span>
          {#if display.answer}
            <button class="reply-action" onclick={copyAnswer} title="复制本次回复的正文" aria-label="复制回复">
              {#if copied}<Check size={14}/>{:else}<Copy size={14}/>{/if}
              <span>{copied ? '已复制' : copyError ? '复制失败' : '复制'}</span>
            </button>
          {/if}
        </div>
        {#if hasProcess}
          <details class="execution-steps" open={!display.answer}
            ontoggle={event => processExpanded = event.currentTarget.open}>
            <summary>
              <ChevronRight size={14} class="disclosure"/>
              <span class="step-label">{stepLabel}</span>
              {#if display.failures}
                <span class="step-fail"><CircleAlert size={13}/>{display.failures} 次失败</span>
              {/if}
            </summary>
            {#if processExpanded || !display.answer}
            <div class="execution-content">
              {#each display.process as activity (activity.id + ':' + activity.kind)}
                {#if activity.kind === 'assistant'}
                  {#if activity.reasoning}
                    <div class="reasoning-text">{activity.reasoning}</div>
                  {/if}
                  {#if activity.text}
                    <div class="intermediate-text"><Markdown content={activity.text}/></div>
                  {/if}
                {:else}
                  <ToolDetail name={activity.name} args={activity.args}
                    output={activity.output} failed={activity.failed}/>
                {/if}
              {/each}
              {#if answerReasoning}
                <div class="reasoning-text">{answerReasoning}</div>
              {/if}
            </div>
            {/if}
          </details>
        {/if}
        {#if display.answer}
          <Markdown content={display.answer.text}/>
          {#if changes && changes.files}
            <div class="turn-changes">
              <button class="turn-changes-button" onclick={onViewTurnChanges}
                title="只读查看这一轮真正改了什么">
                <FileDiff size={14}/>
                本轮改动
                <span class="turn-changes-count">{changes.files} 个文件</span>
                <span class="turn-changes-stat"><b class="plus">+{changes.added}</b><b class="minus">−{changes.removed}</b></span>
              </button>
            </div>
          {:else if showWorkspaceChanges}
            <div class="turn-review-link">
              <button onclick={onViewChanges} title="只读查看当前 Git 工作区差异">
                <FileDiff size={15}/>
                查看工作区变更
                <span>{changeCount}{changeCount >= 200 ? '+' : ''}</span>
                <ChevronRight size={13}/>
              </button>
              <small>可能包含任务前已有修改</small>
            </div>
          {/if}
        {/if}
      </div>
    </article>
  {/if}
</div>

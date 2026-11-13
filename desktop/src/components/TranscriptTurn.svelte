<script lang="ts">
  import { Image, Check, ChevronRight, CircleAlert, Copy, Download, FileText, GitFork, Pencil, Undo2 } from '@lucide/svelte';
  import type { TranscriptTurn } from '../lib/transcript';
  import type { TurnDiffSummary } from '../lib/api';
  import { save } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { attachmentName, suggestedAttachmentName } from '../lib/attachments';
  import { desktopCommandMessage } from '../lib/native-error';
  import { summarizeTurn } from '../lib/transcript';
  import Markdown from './Markdown.svelte';
  import ToolDetail from './ToolDetail.svelte';
  import AttachmentPreviewDialog from './AttachmentPreviewDialog.svelte';

  export let turn: TranscriptTurn;
  export let showWorkspaceChanges = false;
  export let changeCount = 0;
  export let onViewChanges: () => void = () => {};
  /// Measured file changes for this turn, from the pre-task snapshot.
  export let changes: TurnDiffSummary | null = null;
  export let onViewTurnChanges: () => void = () => {};
  /// Editing, rewinding and forking act on the prompt, so they sit under it.
  /// Absolute index of the message the in-conversation search selected.
  export let searchHighlight: number | null = null;
  export let showError: (failure: unknown) => void = () => {};
  export let onRewind: (() => void) | null = null;
  export let onFork: (() => void) | null = null;
  export let onEdit: (() => void) | null = null;
  $: display = summarizeTurn(turn);
  $: answerReasoning = display.answer?.reasoning?.trim() ?? '';
  $: hasReasoning = !!answerReasoning
    || display.process.some(step => step.kind === 'assistant' && step.reasoning.trim().length > 0);
  $: hasProcess = display.process.length > 0 || !!answerReasoning;
  $: noteLabel = processLabel(display.toolCount, hasReasoning);
  $: attachments = (turn.user?.provider_items ?? []).filter(item => item.type === 'kroma_attachment');
  let copied = false;
  let copyError = false;
  let processExpanded = false;
  // History images load only when the person opens one, so a long conversation
  // never decodes hundreds of files at once.
  let previewAttachment: {path: string; name: string} | null = null;
  let savingAttachment = '';
  let saveNote = '';
  // A search hit can live inside a collapsed process panel; reveal it.
  $: if (searchHighlight !== null
    && display.process.some(step => step.id === searchHighlight)) processExpanded = true;

  /** Only an attachment the store can name is worth a preview or save entry; a
   * stale or legacy item without a path stays a plain label. */
  function attachmentPath(item: Record<string, unknown>): string {
    return typeof item.path === 'string' ? item.path : '';
  }

  function previewablePath(item: Record<string, unknown>): string {
    return item.kind === 'image' ? attachmentPath(item) : '';
  }

  function attachmentLabel(item: Record<string, unknown>): string {
    const name = typeof item.name === 'string' ? item.name : '';
    return name || attachmentName(previewablePath(item)) || '附件';
  }

  function processLabel(tools: number, reasoning: boolean): string {
    if (tools) return reasoning ? `思考 · ${tools} 次调用` : `${tools} 次调用`;
    return reasoning ? '思考过程' : '中间回复';
  }

  /** Save one stored attachment. Cancelling the native dialog is not an
   * error; a missing file is reported and the person can pick again. */
  async function saveAttachment(item: Record<string, unknown>) {
    const path = attachmentPath(item);
    if (!path || savingAttachment) return;
    savingAttachment = path;
    try {
      const destination = await save({
        title: '另存附件',
        defaultPath: suggestedAttachmentName(attachmentLabel(item), path),
      });
      if (!destination) return;
      await api.saveAttachmentAs(path, destination);
      saveNote = `已保存 ${attachmentLabel(item)}`;
      window.setTimeout(() => { saveNote = ''; }, 4000);
    } catch (failure) {
      showError(new Error(desktopCommandMessage('另存附件', failure)));
    } finally {
      savingAttachment = '';
    }
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
    <!-- The actions live in their own row under the bubble, so a short prompt
         keeps the width and height of its own text. -->
    <div class="turn-prompt">
      <div class="prompt" data-message-index={turn.user ? turn.id : undefined}
        class:search-hit={searchHighlight !== null && searchHighlight === turn.id}>
        {#if turn.user.content}
          <p class="prompt-text">{turn.user.content.replace(/[\uE000-\uF8FF]/g, '')}</p>
        {/if}
        {#each attachments as item}
          {@const stored = attachmentPath(item)}
          {@const label = attachmentLabel(item)}
          <span class="attachment-chip attachment-chip-stored">
            {#if previewablePath(item)}
              <button class="attachment-chip-button" title="查看图片"
                aria-label={'查看图片 ' + label}
                onclick={() => previewAttachment = {path: previewablePath(item), name: label}}>
                <Image size={12}/>{label}
              </button>
            {:else}
              <FileText size={12}/><span class="attachment-chip-name">{label}</span>
            {/if}
            {#if stored}
              <button class="attachment-save" title="另存为…" aria-label={'另存为 ' + label}
                disabled={savingAttachment === stored}
                onclick={() => void saveAttachment(item)}><Download size={12}/></button>
            {/if}
          </span>
        {/each}
        {#if saveNote}<small class="attachment-save-note" aria-live="polite">{saveNote}</small>{/if}
      </div>
      {#if onRewind || onFork || onEdit}
        <div class="prompt-actions">
          {#if onEdit}
            <button class="text-action" onclick={onEdit} title="回到这条提问并把内容填回输入框">
              <Pencil size={12}/>编辑
            </button>
          {/if}
          {#if onRewind}
            <button class="text-action" onclick={onRewind} title="回到这条提问之前">
              <Undo2 size={12}/>回退
            </button>
          {/if}
          {#if onFork}
            <button class="text-action" onclick={onFork} title="从这条提问分叉出新会话">
              <GitFork size={12}/>分叉
            </button>
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  {#if hasProcess}
    <aside class="turn-notes process-notes" aria-label="思考与工具过程">
      <button class="note note-toggle" aria-expanded={processExpanded || !display.answer}
        title="展开或收起这一步的过程记录"
        onclick={() => processExpanded = !processExpanded}>
        <ChevronRight size={12} class={`note-glyph${processExpanded || !display.answer ? ' rotated' : ''}`}/>
        <span>{noteLabel}</span>
      </button>
      {#if display.failures}
        <span class="note-fail"><CircleAlert size={12}/>{display.failures} 次失败</span>
      {/if}
    </aside>
  {/if}

  {#if hasProcess && (processExpanded || !display.answer)}
    <div class="process-panel">
      {#each display.process as activity (activity.id + ':' + activity.kind)}
        {#if activity.kind === 'assistant'}
          {#if activity.reasoning}
            <div class="reasoning-text">{activity.reasoning}</div>
          {/if}
          {#if activity.text}
            <div class="intermediate-text" data-message-index={activity.id}
              class:search-hit={searchHighlight !== null && searchHighlight === activity.id}>
              <Markdown content={activity.text}/>
            </div>
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

  {#if display.answer}
    <article class="answer" data-message-index={display.answer.id}
      class:search-hit={searchHighlight !== null && searchHighlight === display.answer.id}>
      <Markdown content={display.answer.text}/>
      <div class="answer-actions">
        <button class="text-action" onclick={copyAnswer} title="复制本次回复的正文" aria-label="复制回复">
          {#if copied}<Check size={13}/>{:else}<Copy size={13}/>{/if}
          <span aria-live="polite">{copied ? '已复制' : copyError ? '复制失败' : '复制'}</span>
        </button>
      </div>
    </article>
  {/if}

  {#if changes?.files || showWorkspaceChanges}
    <aside class="turn-notes" aria-label="本轮改动">
      {#if changes?.files}
        <button class="note" onclick={onViewTurnChanges} title="只读查看这一轮真正改了什么">
          本轮改动 {changes.files} 个<span class="note-stat"><b class="plus">+{changes.added}</b><b class="minus">−{changes.removed}</b></span>
        </button>
      {/if}
      {#if showWorkspaceChanges}
        <button class="note" onclick={onViewChanges} title="只读查看当前 Git 工作区差异">
          工作区变更 {changeCount}{changeCount >= 200 ? '+' : ''}
        </button>
      {/if}
    </aside>
  {/if}
</div>

{#if previewAttachment}
  <AttachmentPreviewDialog origin="history" path={previewAttachment.path} name={previewAttachment.name}
    onClose={() => previewAttachment = null}/>
{/if}

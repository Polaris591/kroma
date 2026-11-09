<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    ChevronDown, ChevronUp, FileText, ListTodo, Paperclip, Pause, Play,
    Send, Square, X,
  } from '@lucide/svelte';
  import { api, type SessionSnapshot } from '../lib/api';
  import { cachePercentage } from '../lib/format';
  import {
    canCompose, composerAction, composerPlaceholder, composerTip,
    COMPACTING_TIP, type ComposerGate,
  } from '../lib/composer';
  import { appDrafts, browserDraftStorage, persistDraftText, readStoredDraft } from '../lib/draft-cache';
  import { draftStorageKey } from '../lib/drafts';
  import { atTokenRange, insertFileReference } from '../lib/file-suggest';
  import type { FollowupTask } from '../lib/followups';
  import {
    goalActions, goalNextStep, goalReasonLabel, goalStatusLabel,
    type GoalActionId, type GoalState,
  } from '../lib/goal';
  import type { SteerOutcome } from '../lib/steering';
  import type { TaskResultStatus } from '../lib/ui-types';
  import ContextMeter from './ContextMeter.svelte';
  import {
    activityFrames, activityGlyph, composerCap, composerMaxRows, pluginColors, pluginStatuses, slotColor,
  } from '../lib/plugin-ui';

  export let project = '';
  export let sessionId = '';
  export let ready = false;
  export let running = false;
  export let busy = false;
  export let compacting = false;
  export let navigationBusy = false;
  export let snapshot: SessionSnapshot | null = null;
  export let goal: GoalState | null = null;
  export let goalActionBusy = false;
  export let followups: FollowupTask[] = [];
  export let followupPaused = true;
  export let followupExpanded = false;
  export let taskResult: TaskResultStatus = '';
  export let onSend: (text: string, paths: string[]) => void;
  export let onSteer: (instruction: string) => Promise<SteerOutcome | null>;
  export let onBlocked: () => void;
  export let onQueueFollowup: (text: string) => boolean;
  export let onGoalAction: (action: GoalActionId) => void;
  export let onCompact: () => Promise<void>;
  export let onShowAbilities: () => void;
  export let onCancelTask: () => void;
  export let onToggleFollowups: () => void;
  export let onPauseFollowups: () => void;
  export let onResumeFollowups: () => void;
  export let onRemoveFollowup: (id: string) => void;
  export let onReorderFollowup: (id: string, direction: -1 | 1) => void;
  export let onClearFollowups: () => void;
  export let showError: (failure: unknown) => void;
  export let clearError: () => void;
  export let toast: (message: string) => void;

  let prompt = '';
  let attached: string[] = [];
  // Blob URLs for pasted images only; paths picked from disk have none.
  let attachmentPreviews: Record<string, string> = {};
  // A preview that fails to load must degrade to the file icon, never to alt text.
  let brokenPreviews: Record<string, boolean> = {};
  let fileQuery: string | null = null;
  let fileResults: string[] = [];
  let fileIndex = 0;
  let fileTimer: ReturnType<typeof setTimeout> | null = null;
  let draftWriteTimer: ReturnType<typeof setTimeout> | null = null;
  let editorInput: HTMLTextAreaElement;
  let steeringBusy = false;

  $: gate = {
    ready, busy, running, compacting,
    hasContent: prompt.trim().length > 0 || attached.length > 0,
  } satisfies ComposerGate;
  $: canSend = canCompose(gate);
  $: composerStep = composerAction(gate);
  $: usage = snapshot?.usage?.coding;

  function persistentDraftKey(forProject = project, forSession = sessionId) {
    return draftStorageKey(forProject || '', forSession || 'new');
  }
  export function cancelDraftTimer() {
    if (draftWriteTimer !== null) {
      clearTimeout(draftWriteTimer);
      draftWriteTimer = null;
    }
  }

  export function saveDraft(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    appDrafts.remember(forProject, forSession, {text: prompt, paths: [...attached]});
    persistDraftText(browserDraftStorage(), persistentDraftKey(forProject, forSession), prompt);
  }

  export function restoreDraft(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    const saved = appDrafts.recall(forProject, forSession);
    prompt = saved?.text ?? readStoredDraft(browserDraftStorage(), persistentDraftKey(forProject, forSession));
    attached = [...(saved?.paths ?? [])];
    void tick().then(() => {
      if (editorInput) setEditorHeight(editorInput);
    });
  }

  export function clearForTask(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    appDrafts.forget(forProject, forSession);
    persistDraftText(browserDraftStorage(), persistentDraftKey(forProject, forSession), '');
    prompt = '';
    attached = [];
    if (editorInput) editorInput.style.height = 'auto';
  }

  export function isEmpty() {
    return !prompt.trim() && !attached.length;
  }

  export function restoreInput(text: string, paths: string[], forProject = project, forSession = sessionId) {
    prompt = text;
    attached = paths;
    saveDraft(forProject, forSession);
  }

  export function focus() {
    editorInput?.focus();
  }

  export function setPrompt(text: string) {
    prompt = text;
    editorInput?.focus();
  }

  export function setDraftText(text: string) {
    prompt = text;
    void tick().then(() => {
      editorInput?.focus();
      if (editorInput) setEditorHeight(editorInput);
    });
  }

  export function addPaths(paths: string[]) {
    if (running) { toast('当前任务执行中，文件请在任务结束后添加'); return; }
    attached = [...new Set([...attached, ...paths])].slice(0, 8);
  }

  export function releasePreviews(keep: string[] = []) {
    const retained: Record<string, string> = {};
    for (const [path, url] of Object.entries(attachmentPreviews)) {
      if (keep.includes(path)) retained[path] = url;
      else URL.revokeObjectURL(url);
    }
    attachmentPreviews = retained;
  }

  let frameTimer: ReturnType<typeof setInterval> | null = null;
  let frameTick = 0;
  // Cycle the plugin's declared indicator frames only while a task runs.
  $: animateFrameIndicator(running, $activityFrames.length);
  function animateFrameIndicator(active: boolean, frames: number) {
    if (active && frames) {
      if (frameTimer === null) frameTimer = setInterval(() => { frameTick += 1; }, 120);
    } else if (frameTimer !== null) {
      clearInterval(frameTimer);
      frameTimer = null;
    }
  }
  onDestroy(() => {
    cancelDraftTimer();
    if (fileTimer !== null) { clearTimeout(fileTimer); fileTimer = null; }
    if (frameTimer !== null) { clearInterval(frameTimer); frameTimer = null; }
  });

  let maxEditorHeight = 186;
  // A plugin may ask for a shorter composer; never taller than the built-in cap.
  $: maxEditorHeight = composerCap($composerMaxRows);
  $: statusAccent = slotColor("accent", $pluginColors);
  function setEditorHeight(input: HTMLTextAreaElement) {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, maxEditorHeight) + "px";
  }
  function resizeEditor(event: Event) {
    setEditorHeight(event.currentTarget as HTMLTextAreaElement);
  }
  function onEditorInput(event: Event) {
    resizeEditor(event);
    const input = event.currentTarget as HTMLTextAreaElement;
    syncFileSuggestions(input.value, input.selectionStart ?? input.value.length);
    const text = input.value;
    const key = persistentDraftKey();
    cancelDraftTimer();
    draftWriteTimer = setTimeout(() => {
      persistDraftText(browserDraftStorage(), key, text);
      draftWriteTimer = null;
    }, 350);
  }

  async function attachFile() {
    try {
      const selected = await open({multiple: true, title: '添加图片或文件'});
      if (typeof selected === 'string') addPaths([selected]);
      else if (Array.isArray(selected)) addPaths(selected);
    } catch (failure) { showError(failure); }
  }

  async function pasteImages(event: ClipboardEvent) {
    const items = [...(event.clipboardData?.items ?? [])]
      .filter(item => item.kind === 'file' && item.type.startsWith('image/'));
    if (!items.length) return;
    event.preventDefault();
    clearError();
    for (const item of items) {
      const file = item.getAsFile();
      if (!file) continue;
      try {
        const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
        const extension = file.type.split('/')[1] || 'png';
        const path = await api.savePastedImage(bytes, extension);
        attachmentPreviews = {...attachmentPreviews, [path]: URL.createObjectURL(file)};
        attached = [...attached, path].slice(0, 8);
      } catch (failure) {
        showError(failure);
      }
    }
  }

  function detachAt(index: number) {
    const path = attached[index];
    const preview = attachmentPreviews[path];
    if (preview) {
      URL.revokeObjectURL(preview);
      const next = {...attachmentPreviews};
      delete next[path];
      attachmentPreviews = next;
    }
    attached = attached.filter((_, position) => position !== index);
  }

  function closeFileSuggestions() {
    fileQuery = null;
    fileResults = [];
    fileIndex = 0;
    if (fileTimer !== null) { clearTimeout(fileTimer); fileTimer = null; }
  }

  function syncFileSuggestions(value: string, caret: number) {
    const token = atTokenRange(value, caret);
    if (!token) { closeFileSuggestions(); return; }
    fileQuery = token.query;
    if (fileTimer !== null) clearTimeout(fileTimer);
    const epoch = fileQuery;
    fileTimer = setTimeout(async () => {
      fileTimer = null;
      try {
        const paths = await api.projectFiles(epoch);
        if (fileQuery !== epoch) return;
        fileResults = paths;
        fileIndex = 0;
      } catch { fileResults = []; }
    }, 120);
  }

  function applyFileSuggestion(path: string) {
    const input = editorInput;
    if (!input) return;
    const caret = input.selectionStart ?? prompt.length;
    const inserted = insertFileReference(prompt, caret, path);
    if (!inserted) { closeFileSuggestions(); return; }
    prompt = inserted.value;
    closeFileSuggestions();
    void tick().then(() => {
      editorInput?.focus();
      editorInput?.setSelectionRange(inserted.caret, inserted.caret);
    });
  }

  function queueTypedFollowup() {
    const text = prompt.trim();
    if (!running || !text) return;
    if (!onQueueFollowup(text)) return;
    prompt = '';
    cancelDraftTimer();
    persistDraftText(browserDraftStorage(), persistentDraftKey(), '');
  }

  async function submit() {
    if (running) {
      const instruction = prompt.trim();
      if (!instruction || steeringBusy) return;
      steeringBusy = true;
      try {
        const outcome = await onSteer(instruction);
        // Do not erase edits typed while the IPC request was in flight.
        if (outcome !== null && prompt.trim() === instruction) {
          prompt = '';
          saveDraft();
        }
      } finally {
        steeringBusy = false;
      }
      return;
    }
    if (!canSend) { onBlocked(); return; }
    onSend(prompt.trim(), [...attached]);
  }

  function handleEditor(event: KeyboardEvent) {
    if (fileQuery !== null && fileResults.length) {
      if (event.key === 'ArrowDown') { event.preventDefault(); fileIndex = (fileIndex + 1) % fileResults.length; return; }
      if (event.key === 'ArrowUp') { event.preventDefault(); fileIndex = (fileIndex - 1 + fileResults.length) % fileResults.length; return; }
      if ((event.key === 'Enter' || event.key === 'Tab') && !event.isComposing) {
        event.preventDefault(); applyFileSuggestion(fileResults[fileIndex]); return;
      }
      if (event.key === 'Escape') { event.preventDefault(); closeFileSuggestions(); return; }
    }
    if (event.key === 'Enter' && event.ctrlKey && running && !event.isComposing) {
      event.preventDefault();
      queueTypedFollowup();
      return;
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault(); void submit();
    }
  }

  function cacheRate() { return cachePercentage(usage?.input, usage?.cached); }
</script>

<div class="composer-outer">
  {#if $pluginStatuses.length || (running && $activityFrames.length)}
    <div class="plugin-shelf" aria-label="插件状态">
      {#if running && $activityFrames.length}
        <span class="plugin-frame">{activityGlyph($activityFrames, frameTick)}</span>
      {/if}
      <span class="plugin-status" style:color={statusAccent ?? undefined}>{$pluginStatuses.join(' · ') || '运行中'}</span>
    </div>
  {/if}
  {#if goal && goal.status !== 'none'}
    <div class="goal-shelf" class:goal-running={goal.status === 'running'}
      class:goal-paused={goal.status === 'paused'} aria-label="当前目标状态">
      <div class="goal-heading">
        <span class="goal-state"><span class="goal-state-dot"></span>{goalStatusLabel(goal.status)}</span>
        <span class="goal-id" title={goal.owner ? '目标由插件 ' + goal.owner + ' 提供' : '目标状态由宿主提供'}>{goal.id || '未命名目标'}</span>
        <small class="goal-revision">修订 {goal.revision}</small>
      </div>
      {#if goal.summary}
        <p class="goal-summary" title={goal.summary}>{goal.summary}</p>
      {/if}
      {#if goalReasonLabel(goal.reason)}
        <p class="goal-reason">{goalReasonLabel(goal.reason)}</p>
      {/if}
      <p class="goal-next">{goalNextStep(goal.status)}</p>
      {#if goalActions(goal.status).length}
        <div class="goal-actions">
          {#each goalActions(goal.status) as action (action.id)}
            <button class="goal-action" class:goal-action-stop={action.id === 'cancel'}
              title={action.hint} disabled={goalActionBusy || busy || compacting || !ready}
              onclick={() => onGoalAction(action.id)}>
              {#if action.id === 'pause'}<Pause size={13}/>{:else if action.id === 'resume'}<Play size={13}/>{:else}<Square size={12}/>{/if}
              {action.label}
            </button>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
  {#if followups.length}
    <div class="followup-shelf">
      <div class="followup-heading">
        <button class="followup-toggle" aria-expanded={followupExpanded}
          onclick={onToggleFollowups}>
          <ListTodo size={15}/>
          <span>后续任务 <strong>{followups.length}</strong></span>
          <small>{followupPaused ? '已暂停' : running ? '当前任务结束后执行' : '等待执行'}</small>
          <ChevronDown size={14} class={followupExpanded ? 'rotated' : ''}/>
        </button>
        {#if followupPaused}
          <button class="followup-control" title="依次执行队列中的后续任务"
            disabled={busy || (!running && !ready)}
            onclick={onResumeFollowups}><Play size={14}/> 继续</button>
        {:else}
          <button class="followup-control" title="当前任务结束后暂停队列"
            onclick={onPauseFollowups}><Pause size={14}/> 暂停</button>
        {/if}
      </div>
      {#if followupExpanded}
        <div class="followup-list" aria-label="后续任务列表">
          {#each followups as queued,i (queued.id)}
            <div class="followup-item">
              <span class="followup-number">{i + 1}</span>
              <span class="followup-text" title={queued.text}>{queued.text}</span>
              <div class="followup-item-actions">
                <button title="上移" aria-label={'上移任务 '+(i+1)} disabled={i===0}
                  onclick={() => onReorderFollowup(queued.id,-1)}><ChevronUp size={14}/></button>
                <button title="下移" aria-label={'下移任务 '+(i+1)} disabled={i===followups.length-1}
                  onclick={() => onReorderFollowup(queued.id,1)}><ChevronDown size={14}/></button>
                <button title="移除" aria-label={'移除任务 '+(i+1)}
                  onclick={() => onRemoveFollowup(queued.id)}><X size={14}/></button>
              </div>
            </div>
          {/each}
          <div class="followup-footnote">
            <span>按顺序独立执行；失败或停止后自动暂停</span>
            <button onclick={onClearFollowups}>清空</button>
          </div>
        </div>
      {/if}
    </div>
  {/if}
  {#if fileQuery !== null && fileResults.length}
    <div class="file-suggest" role="listbox" aria-label="引用项目文件">
      <div class="file-suggest-head">引用文件 · {fileResults.length}</div>
      {#each fileResults as path, index (path)}
        <button class="file-suggest-row" class:active={index === fileIndex}
          role="option" aria-selected={index === fileIndex}
          onmouseenter={() => fileIndex = index}
          onclick={() => applyFileSuggestion(path)}>
          <FileText size={13}/>
          <span class="file-suggest-path">{path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : ''}<b>{path.split('/').pop()}</b></span>
        </button>
      {/each}
      <div class="file-suggest-foot">↑↓ 选择 · Enter 插入 · Esc 关闭</div>
    </div>
  {/if}
  <div class="composer">
    {#if attached.length}
      <div class="draft-files">
        {#each attached as file, i}
          <span class="file-chip" class:is-image={!!attachmentPreviews[file]}>
            {#if attachmentPreviews[file] && !brokenPreviews[file]}
              <img class="file-thumb" src={attachmentPreviews[file]} alt="" onerror={() => brokenPreviews = {...brokenPreviews, [file]: true}}/>
            {:else}
              <FileText size={14}/>
            {/if}
            <span class="file-chip-name">{file.slice(Math.max(file.lastIndexOf("/"), file.lastIndexOf("\\")) + 1)}</span>
            <button title="移除附件" onclick={() => detachAt(i)}><X size={14}/></button></span>
        {/each}
      </div>
    {/if}
    <textarea bind:this={editorInput} bind:value={prompt} onkeydown={handleEditor} oninput={onEditorInput} onpaste={pasteImages}
      disabled={navigationBusy}
      placeholder={composerPlaceholder(gate)}
      rows="1" aria-label="输入任务"></textarea>
    <div class="composer-actions">
      <div class="composer-left">
        <button class="icon-button" title="添加图片或文件" disabled={running || !ready} onclick={attachFile}><Paperclip size={17}/></button>
        <span class="composer-tip">{composerTip(composerStep, running, fileQuery !== null && fileResults.length > 0)}</span>
      </div>
      <div class="composer-right">
        {#if running}
          <button class="stop-button" title="取消当前任务" onclick={onCancelTask}><Square size={14} fill="currentColor"/> 停止</button>
          {#if prompt.trim()}
            <button class="followup-add-button" title="完成当前任务后再执行这一条"
              onclick={queueTypedFollowup}><ListTodo size={15}/> 排队</button>
            <button class="send-button" title={steeringBusy ? '正在插入指令…' : '插入当前任务'}
              disabled={steeringBusy} onclick={submit}><Send size={16}/></button>
          {/if}
        {:else}
          <button class="send-button" title="发送" disabled={!canSend} onclick={submit}><Send size={17}/></button>
        {/if}
      </div>
    </div>
  </div>
  <div class="footer-metrics">
    <ContextMeter total={snapshot?.context_window ?? null} available={snapshot?.available_input ?? null}
      report={snapshot?.context ?? null} usage={usage ?? null} cache={cacheRate()}
      toolNames={snapshot?.tool_names ?? []} skills={snapshot?.skills ?? []}
      compacting={compacting} onCompact={onCompact} onShowAbilities={onShowAbilities}/>
    <div class="footer-usage">
      {#if running}<span class="footer-working"><span class="live-dot"></span> 正在运行</span>
      {:else if taskResult === 'completed'}<span class="footer-finished">本轮已结束</span>
      {:else if taskResult === 'failed'}<span class="footer-stopped">本轮执行失败</span>
      {:else if taskResult === 'cancelled'}<span class="footer-stopped">本轮已停止</span>{/if}
    </div>
  </div>
</div>

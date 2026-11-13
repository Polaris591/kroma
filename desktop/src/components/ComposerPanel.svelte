<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    ArrowUp, ChevronDown, ChevronUp, CornerUpLeft, FileText, ListTodo, Paperclip, Pause, Play,
    Square, X,
  } from '@lucide/svelte';
  import { api, type SessionSnapshot } from '../lib/api';
  import { attachmentName, attachmentStateMessage, looksLikeImage } from '../lib/attachments';
  import { desktopCommandMessage } from '../lib/native-error';
  import AttachmentImage from './AttachmentImage.svelte';
  import AttachmentPreviewDialog from './AttachmentPreviewDialog.svelte';
  import { cachePercentage } from '../lib/format';
  import {
    canCompose, composerPlaceholder, type ComposerGate,
  } from '../lib/composer';
  import { appDrafts, browserDraftStorage, persistDraft, readStoredDraft } from '../lib/draft-cache';
  import { draftStorageKey, MAX_DRAFT_LENGTH, type DraftWrite } from '../lib/drafts';
  import { atTokenRange, insertFileReference } from '../lib/file-suggest';
  import { followupLabel, type FollowupTask } from '../lib/followups';
  import type { SteerOutcome } from '../lib/steering';
  import type { TaskResultStatus } from '../lib/ui-types';
  import ContextMeter from './ContextMeter.svelte';
  import AskBar from './AskBar.svelte';
  import type { AskAnswer, AskQuestion } from '../lib/ask-question';
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
  export let followups: FollowupTask[] = [];
  export let followupPaused = true;
  export let followupExpanded = false;
  export let taskResult: TaskResultStatus = '';
  // An open plugin question owns the composer slot until it is answered.
  export let ask: AskQuestion | null = null;
  export let onAnswer: (answer: AskAnswer) => void;
  export let onSend: (text: string, paths: string[]) => void;
  export let onSteer: (instruction: string) => Promise<SteerOutcome | null>;
  export let onBlocked: () => void;
  export let onQueueFollowup: (text: string, paths: string[]) => Promise<boolean>;
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
  // Blob URLs for pasted images only; paths picked from disk are read through
  // the backend preview command, bounded and verified there.
  let attachmentPreviews: Record<string, string> = {};
  // A preview that fails to load must degrade to the file icon, never to alt text.
  let brokenPreviews: Record<string, boolean> = {};
  // The image the person asked to see in full; nothing is loaded until then.
  let previewOpen: {path: string; name: string} | null = null;
  const maxAttachments = 8;
  // Pasting and file selection finish asynchronously. Restoring, clearing or
  // replacing the draft retires everything still in flight, so a late answer
  // can neither revive the first draft of A -> B -> A nor report its failure
  // against the new one. Typing never changes the draft on purpose.
  let attachEpoch = 0;
  let fileQuery: string | null = null;
  let fileResults: string[] = [];
  let fileIndex = 0;
  let fileTimer: ReturnType<typeof setTimeout> | null = null;
  let draftWriteTimer: ReturnType<typeof setTimeout> | null = null;
  let editorInput: HTMLTextAreaElement;
  let steeringBusy = false;
  // Restored draft attachments are checked once, off the input path. A failed
  // file keeps its label with a visible reason and can be removed or replaced.
  let invalidAttachments: Record<string, string> = {};
  let validationEpoch = 0;
  let queueing = false;
  let storageWarned = false;
  let truncationWarned = false;

  $: gate = {
    ready, busy, running, compacting,
    hasContent: prompt.trim().length > 0 || attached.length > 0,
  } satisfies ComposerGate;
  $: canSend = canCompose(gate);
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

  /** Report a refused or shortened storage write without interrupting typing. */
  function reportDraftWrite(outcome: DraftWrite) {
    if (outcome === 'unavailable' && !storageWarned) {
      storageWarned = true;
      toast('本机存储不可用，草稿只在本次运行中保留');
    }
    if (outcome === 'truncated' && !truncationWarned) {
      truncationWarned = true;
      toast(`草稿超过 ${MAX_DRAFT_LENGTH} 字：重启后只能恢复前面的 ${MAX_DRAFT_LENGTH} 字`);
    }
  }

  function storeDraft(forProject = project, forSession = sessionId) {
    return persistDraft(browserDraftStorage(), persistentDraftKey(forProject, forSession),
      {text: prompt, paths: attached});
  }

  export function saveDraft(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    appDrafts.remember(forProject, forSession, {text: prompt, paths: [...attached]});
    reportDraftWrite(storeDraft(forProject, forSession));
  }

  /** Re-check restored files once. The epoch retires answers that belong to a
   * draft the person has already left. */
  function validateAttachments() {
    const epoch = ++validationEpoch;
    if (!attached.length) { invalidAttachments = {}; return; }
    void api.inspectAttachments([...attached]).then(result => {
      if (epoch !== validationEpoch) return;
      const next: Record<string, string> = {};
      for (const entry of result.entries) {
        if (entry.state !== 'ok') next[entry.path] = attachmentStateMessage(entry.state);
      }
      invalidAttachments = next;
    }).catch(failure => {
      if (epoch === validationEpoch) toast(desktopCommandMessage('附件检查', failure));
    });
  }

  export function restoreDraft(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    closeFileSuggestions();
    attachEpoch += 1;
    const saved = appDrafts.recall(forProject, forSession);
    const stored = saved
      ? {text: saved.text, paths: saved.paths, truncated: false}
      : readStoredDraft(browserDraftStorage(), persistentDraftKey(forProject, forSession));
    prompt = stored.text;
    attached = [...stored.paths];
    invalidAttachments = {};
    if (stored.truncated && !truncationWarned) {
      truncationWarned = true;
      toast(`草稿过长：只恢复了前面的 ${MAX_DRAFT_LENGTH} 字`);
    }
    validateAttachments();
    void tick().then(() => {
      if (editorInput) setEditorHeight(editorInput);
    });
  }

  export function clearForTask(forProject = project, forSession = sessionId) {
    cancelDraftTimer();
    closeFileSuggestions();
    attachEpoch += 1;
    validationEpoch += 1;
    appDrafts.forget(forProject, forSession);
    prompt = '';
    attached = [];
    invalidAttachments = {};
    persistDraft(browserDraftStorage(), persistentDraftKey(forProject, forSession), {text: '', paths: []});
    if (editorInput) editorInput.style.height = 'auto';
  }

  export function isEmpty() {
    return !prompt.trim() && !attached.length;
  }

  export function restoreInput(text: string, paths: string[], forProject = project, forSession = sessionId) {
    attachEpoch += 1;
    prompt = text;
    attached = paths;
    invalidAttachments = {};
    closeFileSuggestions();
    saveDraft(forProject, forSession);
  }

  export function focus() {
    editorInput?.focus();
  }

  export function setPrompt(text: string) {
    prompt = text;
    closeFileSuggestions();
    editorInput?.focus();
  }

  export function setDraftText(text: string, mode = 'replace') {
    prompt = mode === 'append' ? (prompt ? `${prompt}${text}` : text) : text;
    closeFileSuggestions();
    void tick().then(() => {
      editorInput?.focus();
      if (editorInput) setEditorHeight(editorInput);
    });
  }

  // Only the paths this draft actually takes may produce a preview URL, and
  // pasted images are only written to disk while there is still room.
  function acceptPaths(paths: string[]) {
    const candidates = [...new Set(paths)].filter(path => !attached.includes(path));
    const accepted = candidates.slice(0, Math.max(0, maxAttachments - attached.length));
    if (accepted.length) attached = [...attached, ...accepted];
    if (accepted.length < candidates.length) toast(`最多添加 ${maxAttachments} 个附件，超出的文件未添加`);
    return accepted;
  }

  export function addPaths(paths: string[]) {
    acceptPaths(paths);
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
    attachEpoch += 1;
    cancelDraftTimer();
    closeFileSuggestions();
    if (frameTimer !== null) { clearInterval(frameTimer); frameTimer = null; }
  });

  let maxEditorHeight = 186;
  let viewportHeight = 850;
  // Keep room for the conversation when a queue and a multiline draft coexist.
  $: maxEditorHeight = Math.min(composerCap($composerMaxRows), Math.max(44, Math.floor(viewportHeight * .22)));
  $: resizeForPrompt(editorInput, prompt, maxEditorHeight);
  function resizeForPrompt(input: HTMLTextAreaElement | undefined, _text: string, _cap: number) {
    void tick().then(() => { if (input?.isConnected) setEditorHeight(input); });
  }
  $: statusAccent = slotColor("accent", $pluginColors);
  function setEditorHeight(input: HTMLTextAreaElement) {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, maxEditorHeight) + "px";
  }
  function onEditorInput(event: Event) {
    const input = event.currentTarget as HTMLTextAreaElement;
    syncFileSuggestions(input.value, input.selectionStart ?? input.value.length);
    const text = input.value;
    const key = persistentDraftKey();
    cancelDraftTimer();
    draftWriteTimer = setTimeout(() => {
      draftWriteTimer = null;
      reportDraftWrite(persistDraft(browserDraftStorage(), key, {text, paths: attached}));
    }, 350);
  }

  // Attachments may be chosen while a task runs: they belong to the queued
  // task. Each answer still belongs to one draft only, and the epoch retires
  // work that was in flight when the draft changed.
  function acceptingAttachments(scope: number) {
    return scope === attachEpoch;
  }

  async function attachFile() {
    const scope = attachEpoch;
    try {
      const selected = await open({multiple: true, title: '添加图片或文件'});
      // The dialog stays open across a session switch or task start.
      if (scope !== attachEpoch) return;
      if (typeof selected === 'string') addPaths([selected]);
      else if (Array.isArray(selected)) addPaths(selected);
    } catch (failure) {
      if (scope !== attachEpoch) return;
      showError(failure);
    }
  }

  async function pasteImages(event: ClipboardEvent) {
    const items = [...(event.clipboardData?.items ?? [])]
      .filter(item => item.kind === 'file' && item.type.startsWith('image/'));
    if (!items.length) return;
    event.preventDefault();
    clearError();
    const scope = attachEpoch;
    for (const item of items) {
      const file = item.getAsFile();
      if (!file) continue;
      try {
        const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
        // A full draft writes no file and creates no preview URL.
        if (!acceptingAttachments(scope)) return;
        if (attached.length >= maxAttachments) { toast(`最多添加 ${maxAttachments} 个附件，请先移除一个再粘贴`); return; }
        const extension = file.type.split('/')[1] || 'png';
        const path = await api.savePastedImage(bytes, extension);
        if (!acceptingAttachments(scope)) return;
        if (!acceptPaths([path]).length) return;
        attachmentPreviews = {...attachmentPreviews, [path]: URL.createObjectURL(file)};
      } catch (failure) {
        if (!acceptingAttachments(scope)) return;
        showError(failure);
      }
    }
  }

  function openPreview(path: string) {
    previewOpen = {path, name: attachmentName(path)};
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

  // Only the newest request may install candidates: a newer query, a
  // programmatic prompt replacement, a session change or unmount supersedes
  // everything still in flight. Comparing query strings alone would revive an
  // older request when the same query comes back (a -> b -> a).
  let fileRequest = 0;

  function closeFileSuggestions() {
    fileRequest += 1;
    fileQuery = null;
    fileResults = [];
    fileIndex = 0;
    if (fileTimer !== null) { clearTimeout(fileTimer); fileTimer = null; }
  }

  function syncFileSuggestions(value: string, caret: number) {
    const token = atTokenRange(value, caret);
    if (!token) { closeFileSuggestions(); return; }
    if (fileTimer !== null) clearTimeout(fileTimer);
    // Candidates for the previous query must not stay insertable while the
    // replaced query is still being resolved.
    if (token.query !== fileQuery) { fileResults = []; fileIndex = 0; }
    fileQuery = token.query;
    const request = ++fileRequest;
    fileTimer = setTimeout(async () => {
      fileTimer = null;
      try {
        const paths = await api.projectFiles(token.query);
        if (request !== fileRequest) return;
        fileResults = paths;
        fileIndex = 0;
      } catch {
        // A failure a newer query already replaced leaves that query's results alone.
        if (request === fileRequest) fileResults = [];
      }
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

  /** Queue the exact text and attachment references. The answer is awaited
   * because the native side first copies the files; only this draft is cleared,
   * and only while it is still the draft that was queued. */
  async function queueTypedFollowup() {
    const text = prompt.trim();
    const paths = [...attached];
    if (!running || (!text && !paths.length) || queueing) return;
    const scope = attachEpoch;
    queueing = true;
    let queued = false;
    try {
      queued = await onQueueFollowup(text, paths);
    } finally {
      queueing = false;
    }
    if (!queued || scope !== attachEpoch) return;
    // The person may have kept typing or changed attachments while the copy
    // ran; that newer draft belongs to them, not to the queued task.
    if (prompt.trim() !== text || attached.join('\u0000') !== paths.join('\u0000')) return;
    prompt = '';
    attached = [];
    invalidAttachments = {};
    closeFileSuggestions();
    cancelDraftTimer();
    persistDraft(browserDraftStorage(), persistentDraftKey(), {text: '', paths: []});
  }

  async function submit() {
    if (running) {
      // Attachments have no steering meaning; they are queued with the next
      // task instead of being silently left behind in the draft.
      if (attached.length) {
        toast('附件会随排队任务一起发送，请使用“排队”');
        return;
      }
      const instruction = prompt.trim();
      if (!instruction || steeringBusy) return;
      steeringBusy = true;
      try {
        const outcome = await onSteer(instruction);
        // Do not erase edits typed while the IPC request was in flight.
        if (outcome !== null && prompt.trim() === instruction) {
          prompt = '';
          closeFileSuggestions();
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
    if (event.isComposing) return;
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
      void queueTypedFollowup();
      return;
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault(); void submit();
    }
  }

  function cacheRate() { return cachePercentage(usage?.input, usage?.cached); }
</script>

<svelte:window bind:innerHeight={viewportHeight}/>
<div class="composer-outer">
  {#if $pluginStatuses.length || (running && $activityFrames.length)}
    <div class="plugin-shelf" aria-label="插件状态">
      {#if running && $activityFrames.length}
        <span class="plugin-frame">{activityGlyph($activityFrames, frameTick)}</span>
      {/if}
      <span class="plugin-status" style:color={statusAccent ?? undefined}>{$pluginStatuses.join(' · ') || '运行中'}</span>
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
            {@const label = followupLabel(queued)}
            <div class="followup-item">
              <span class="followup-number">{i + 1}</span>
              <span class="followup-text" title={label}>{label}</span>
              {#if queued.paths.length}
                <span class="followup-attachments" title="入队时已复制进会话附件目录">
                  <Paperclip size={11}/>{queued.paths.length}</span>
              {/if}
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
            <button title="移除全部待执行任务，不影响当前任务" onclick={onClearFollowups}>清空</button>
          </div>
        </div>
      {/if}
    </div>
  {/if}
  {#if ask}
    <!-- The question takes the composer slot as a bar; the normal message box
         comes back as soon as it is answered or closed. -->
    {#key ask.id}
      <AskBar {ask} {onAnswer}/>
    {/key}
  {/if}
  <!-- Kept mounted but hidden, so the draft, attachments and loaded history
       survive the question exactly as they survive a tab switch. -->
  <div class="composer" hidden={!!ask} style:--composer-height-cap={`${maxEditorHeight}px`}>
    {#if !ask && fileQuery !== null && fileResults.length}
      <div class="file-suggest" id="composer-file-suggestions" role="listbox" aria-label="引用项目文件">
        <div class="file-suggest-head">引用文件 · {fileResults.length}</div>
        {#each fileResults as path, index (path)}
          <button class="file-suggest-row" class:active={index === fileIndex}
            id={`composer-file-${index}`} tabindex="-1" role="option" aria-selected={index === fileIndex}
            onmouseenter={() => fileIndex = index}
            onclick={() => applyFileSuggestion(path)}>
            <FileText size={13}/>
            <span class="file-suggest-path">{path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : ''}<b>{path.split('/').pop()}</b></span>
          </button>
        {/each}
        <div class="file-suggest-foot">↑↓ 选择 · Enter 插入 · Esc 关闭</div>
      </div>
    {/if}
    {#if attached.length}
      <div class="draft-files">
        {#each attached as file, i}
          {@const pasted = attachmentPreviews[file]}
          {@const previewable = !!pasted || looksLikeImage(file)}
          {@const invalid = invalidAttachments[file]}
          <span class="file-chip" class:is-image={previewable} class:invalid={!!invalid}
            title={invalid ? `附件已失效：${invalid}，请移除或重新选择` : file}>
            {#if previewable}
              <button class="file-thumb-button" title="查看图片"
                aria-label={'查看图片 ' + attachmentName(file)} onclick={() => openPreview(file)}>
                {#if pasted && !brokenPreviews[file]}
                  <img class="file-thumb" src={pasted} alt=""
                    onerror={() => brokenPreviews = {...brokenPreviews, [file]: true}}/>
                {:else}
                  <AttachmentImage origin="draft" path={file} className="file-thumb"
                    alt={attachmentName(file)}>
                    <FileText size={14}/>
                  </AttachmentImage>
                {/if}
              </button>
            {:else}
              <FileText size={14}/>
            {/if}
            <span class="file-chip-name">{attachmentName(file)}</span>
            {#if invalid}<span class="file-chip-invalid" role="status">已失效</span>{/if}
            <button title={invalid ? `移除失效附件（${invalid}）` : '移除附件'}
              aria-label="移除附件" onclick={() => detachAt(i)}><X size={14}/></button></span>
        {/each}
      </div>
    {/if}
    <textarea bind:this={editorInput} bind:value={prompt} onkeydown={handleEditor} oninput={onEditorInput} onpaste={pasteImages}
      style:padding-block={maxEditorHeight < 44 ? '0px' : undefined}
      disabled={navigationBusy}
      placeholder={composerPlaceholder(gate)}
      rows="1" aria-label="输入任务" aria-describedby="composer-help"
      aria-autocomplete="list" aria-controls={fileResults.length ? 'composer-file-suggestions' : undefined}
      aria-activedescendant={fileResults.length ? `composer-file-${fileIndex}` : undefined}></textarea>
    <div class="composer-actions">
      <div class="composer-left">
        <button class="icon-button" title={attached.length >= maxAttachments ? '已达到 8 个附件上限，移除后可继续添加' : !ready ? '连接模型后可添加附件' : '添加图片或文件'} aria-label="添加图片或文件" disabled={!ready || navigationBusy || attached.length >= maxAttachments} onclick={attachFile}><Paperclip size={17}/></button>
        <ContextMeter total={snapshot?.context_window ?? null}
          report={snapshot?.context ?? null} usage={usage ?? null} cache={cacheRate()}
          toolNames={snapshot?.tool_names ?? []} skills={snapshot?.skills ?? []}
          {compacting} compactDisabled={running || busy || navigationBusy} onCompact={onCompact} onShowAbilities={onShowAbilities}/>
      </div>
      <div class="composer-right">
        {#if !running && taskResult === 'failed'}<span class="footer-stopped failed">本轮执行失败</span>
        {:else if !running && taskResult === 'cancelled'}<span class="footer-stopped">本轮已停止</span>{/if}
        {#if running}
          <button class="stop-button" title="取消当前任务" onclick={onCancelTask}><Square size={14} fill="currentColor"/> 停止</button>
          {#if prompt.trim() || attached.length}
            <button class="followup-add-button" title="完成当前任务后再执行这一条（含附件）"
              disabled={queueing} onclick={() => void queueTypedFollowup()}><ListTodo size={15}/> 排队</button>
          {/if}
          {#if prompt.trim() && !attached.length}
            <button class="send-button" title={steeringBusy ? '正在插入指令…' : '插入当前任务'} aria-label="插入当前任务"
              disabled={steeringBusy} onclick={submit}><CornerUpLeft size={17}/></button>
          {/if}
        {:else}
          <button class="send-button" title="发送 · Enter" aria-label="发送任务" disabled={!canSend} onclick={submit}><ArrowUp size={18}/></button>
        {/if}
      </div>
    </div>
  </div>
  {#if !ask}<p class="composer-hint" id="composer-help">
    <span><kbd>Enter</kbd> {running ? '插话' : '发送'}</span>
    {#if running}<span><kbd>Ctrl+Enter</kbd> 排队</span>{/if}
    <span><kbd>Shift+Enter</kbd> 换行</span>
    {#if !running}<span><kbd>@</kbd> 引用文件</span>{/if}
  </p>{/if}
</div>
{#if previewOpen}
  <AttachmentPreviewDialog origin="draft" path={previewOpen.path} name={previewOpen.name}
    onClose={() => previewOpen = null}/>
{/if}

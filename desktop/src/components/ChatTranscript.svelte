<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { ArrowDown, ChevronDown, ChevronRight, ChevronUp, FolderClosed, GitPullRequest, LoaderCircle, ScanSearch, Search, Settings2, Wrench, X } from '@lucide/svelte';
  import { api, type Bootstrap, type CheckpointTurn, type Message, type SessionSnapshot, type TurnDiffSummary, type WorkspaceReview } from '../lib/api';
  import { canActOnTurn, turnDiffFor, turnNumberByUserIndex } from '../lib/checkpoints';
  import { scrollTopAfterPrepend, workspaceDisplayName } from '../lib/chat-ui';
  import { carryOlderHistory, sessionPreview } from '../lib/snapshot';
  import { createRequestTokens } from '../lib/latest';
  import { historyPageBelongsTo } from '../lib/snapshot';
  import { groupTranscript, visibleTranscriptStart, type LiveActivity } from '../lib/transcript';
  import {
    findCountLabel, findInMessages, findScopeNote, stepHit, type FindHit,
  } from '../lib/search';
  import type { Tab } from '../lib/ui-types';
  import LiveTimeline from './LiveTimeline.svelte';
  import TranscriptTurn from './TranscriptTurn.svelte';
  import TurnChanges from './TurnChanges.svelte';
  import WorkspaceChanges from './WorkspaceChanges.svelte';
  import PluginWidgets from './PluginWidgets.svelte';

  export let data: Bootstrap | null = null;
  export let snapshot: SessionSnapshot | null = null;
  export let switchingTo = '';
  export let previewSnapshot: SessionSnapshot | null = null;
  export let running = false;
  export let busy = false;
  export let ready = false;
  export let draftPrompt = '';
  export let liveActivities: LiveActivity[] = [];
  export let activeElapsed = 0;
  export let taskSnapshotReceived = false;
  /** Fresh values for imperative calls that follow a synchronous state change. */
  export let getData: () => Bootstrap | null;
  export let getTaskGeneration: () => number;
  export let onRewind: (messageIndex: number, snapshotTurn: number | null, diff: TurnDiffSummary | null) => void;
  export let onFork: (messageIndex: number) => void;
  export let onEdit: (messageIndex: number, text: string) => void;
  export let onUsePrompt: (text: string) => void;
  export let onSetTab: (tab: Tab) => void;
  export let showError: (failure: unknown) => void;
  export let widgetActionBusy = false;
  export let onWidgetAction: (pluginId: string, widgetId: string, actionId: string) => void;

  let olderMessages: Message[] = [];
  let historyHasMore = false;
  let historyBusy = false;
  let historyCount = 72;
  let scroller: HTMLDivElement;
  let atBottom = true;
  // Bounded, in-memory reading windows. Leaving a running conversation must
  // not discard pages the person already opened or collapse it back to 72.
  const historyViews = new Map<string, {
    snapshot: SessionSnapshot; older: Message[]; count: number; top: number; bottom: boolean;
  }>();
  const historyKey = (project: string, session: string) => JSON.stringify([project, session]);
  let checkpointTurns: CheckpointTurn[] = [];
  let turnDiffs = new Map<number, TurnDiffSummary>();
  let conversationKey = '';
  // A slow checkpoint response or file list must never paint another
  // conversation's turns or file changes.
  const checkpointRequests = createRequestTokens();
  const turnFilesRequests = createRequestTokens();
  let turnChangesView: {turn: number; summary: TurnDiffSummary; files: Array<{path:string;status:string;added:number;removed:number}>} | null = null;
  let reviewOpen = false;
  let reviewBusy = false;
  let reviewError = '';
  let workspaceReview: WorkspaceReview | null = null;
  let reviewRequest = 0;
  // In-conversation search. It reads the messages already in memory only: no
  // disk scan per keypress and no model or tool call.
  let findOpen = false;
  let findQuery = '';
  let findHits: FindHit[] = [];
  let findCurrent = -1;
  let findNote = '';
  let findInput: HTMLInputElement;
  // The selection is kept in absolute conversation coordinates; the rendered
  // transcript and its turns use local indexes into the loaded window.
  let highlightAbsolute: number | null = null;
  let absoluteBase = 0;
  let findSignature = '';
  let revealRequest = 0;

  $: history = [...olderMessages, ...(snapshot?.messages ?? [])];
  // The loaded messages are one contiguous suffix of the conversation, so a
  // native absolute message index is localIndex + absoluteBase.
  $: absoluteBase = Math.max(0, (snapshot?.total_messages ?? history.length) - history.length);
  $: highlightIndex = highlightAbsolute === null ? null : highlightAbsolute - absoluteBase;
  $: historyStart = visibleTranscriptStart(history, historyCount);
  $: visibleTurns = groupTranscript(history.slice(historyStart), historyStart);
  $: turnNumbers = turnNumberByUserIndex(checkpointTurns, history);
  $: hasWorkspaceChanges = !!workspaceReview?.is_repo && workspaceReview.entries.length > 0;
  // Recompute only when the scope, query or loaded window actually changes.
  // The selected hit is preserved by absolute message index across a reload.
  $: {
    const signature = `${data?.project ?? ''}|${snapshot?.session_id ?? ''}`
      + `|${findOpen}|${findQuery}|${history.length}|${olderMessages.length}|${historyHasMore}`;
    if (signature !== findSignature) {
      findSignature = signature;
      refreshFind();
    }
  }
  $: {
    // The project belongs to the scope: two projects can hold a session with
    // the same id and message count. Opening a session is its own scope as
    // well, so a preview and the snapshot it is replaced by never share the
    // answers of the conversation that was shown before.
    const key = `${data?.project ?? ''}|${snapshot?.session_id ?? ''}|${snapshot?.messages.length ?? 0}|${switchingTo ? 'opening' : 'shown'}`;
    if (key !== conversationKey) {
      conversationKey = key;
      // A reveal or find answer that describes the previous project/session
      // must not paint or scroll the new one. Same ID in two projects is a
      // different conversation.
      revealRequest += 1;
      highlightAbsolute = null;
      findHits = [];
      findCurrent = -1;
      findNote = '';
      checkpointTurns = [];
      turnDiffs = new Map();
      turnChangesView = null;
      // Answers still on their way describe the previous scope; success and
      // failure alike must be dropped instead of reviving on this one.
      checkpointRequests.begin();
      turnFilesRequests.begin();
      // While a preview is shown the Agent still owns the previous session, so
      // a lookup here would attach another conversation's turns to the preview.
      if (!switchingTo && snapshot?.session_id && snapshot.session_id !== 'unknown') {
        void refreshCheckpoints();
      }
    }
  }

  onDestroy(() => {
    // A component that is gone can neither show an answer nor report its error.
    checkpointRequests.begin();
    turnFilesRequests.begin();
    revealRequest += 1;
    highlightAbsolute = null;
    historyViews.clear();
  });

  export function rememberHistory() {
    const current = getData();
    const full = current?.snapshot;
    if (!current || !full?.messages || !full.session_id || full.session_id === 'unknown') return;
    const key = historyKey(current.project, full.session_id);
    const recent = sessionPreview(full, 120);
    const older = carryOlderHistory(olderMessages, full, recent);
    historyViews.delete(key);
    historyViews.set(key, {snapshot: recent, older,
      count: Math.min(historyCount, recent.messages.length + older.length),
      top: scroller?.scrollTop ?? 0, bottom: atBottom});
    if (historyViews.size > 5) historyViews.delete(historyViews.keys().next().value!);
  }

  export function syncHistory(restore = false): boolean {
    const current = getData();
    const next = current?.snapshot;
    const key = current && next ? historyKey(current.project, next.session_id) : '';
    const cached = restore ? historyViews.get(key) : undefined;
    olderMessages = cached && next ? carryOlderHistory(cached.older, cached.snapshot, next) : [];
    const start = next ? Math.max(0, (next.total_messages ?? next.messages.length) - next.messages.length) : 0;
    historyHasMore = start > olderMessages.length || (!next?.total_messages && !!next?.history_truncated);
    const advanced = cached && next ? Math.max(0,
      (next.total_messages ?? next.messages.length) - (cached.snapshot.total_messages ?? cached.snapshot.messages.length)) : 0;
    historyCount = cached ? Math.max(72, cached.count + advanced) : 72;
    if (!restore) historyViews.delete(key);
    if (cached) {
      const generation = getTaskGeneration();
      atBottom = cached.bottom;
      void tick().then(() => {
        const shown = getData();
        if (scroller && shown?.snapshot && generation === getTaskGeneration()
          && historyKey(shown.project, shown.snapshot.session_id) === key) {
          scroller.scrollTop = cached.bottom ? scroller.scrollHeight : cached.top;
        }
      });
    }
    return !!cached;
  }

  /** Navigation starts from a fresh window before the preview is shown. */
  export function resetWindow() {
    olderMessages = [];
    historyCount = 72;
  }

  /** Keep older pages when a task snapshot extends the same conversation. */
  export function carrySnapshot(previous: SessionSnapshot | null, next: SessionSnapshot) {
    if (!previous || previous.session_id !== next.session_id) return;
    olderMessages = carryOlderHistory(olderMessages, previous, next);
    if (historyCount > 72) {
      const advanced = Math.max(0, (next.total_messages ?? next.messages.length)
        - (previous.total_messages ?? previous.messages.length));
      historyCount = Math.min(historyCount + advanced, olderMessages.length + next.messages.length);
    }
    const nextStart = Math.max(0, (next.total_messages ?? next.messages.length) - next.messages.length);
    historyHasMore = nextStart > olderMessages.length;
  }

  export async function refreshCheckpoints() {
    const token = checkpointRequests.begin();
    const session = snapshot?.session_id ?? '';
    try {
      const result = await api.checkpointTurns();
      if (!checkpointRequests.isCurrent(token) || (snapshot?.session_id ?? '') !== session) return;
      checkpointTurns = result.turns ?? [];
      turnDiffs = new Map((result.diffs ?? []).map(diff => [diff.turn, diff]));
    } catch {
      if (!checkpointRequests.isCurrent(token) || (snapshot?.session_id ?? '') !== session) return;
      checkpointTurns = [];
      turnDiffs = new Map();
    }
  }

  export async function scrollDown() {
    await tick();
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
    atBottom = true;
  }

  export async function scrollIfBottom() {
    if (atBottom) await scrollDown();
  }

  /** A project switch drops the previous workspace report entirely. */
  export function clearReview() {
    workspaceReview = null;
    reviewOpen = false;
  }

  export function closeReview(): boolean {
    if (!reviewOpen) return false;
    reviewOpen = false;
    return true;
  }

  export function isReviewOpen(): boolean {
    return reviewOpen;
  }

  export async function refreshReview() {
    const id = ++reviewRequest;
    const current = getData();
    const project = current?.project;
    if (!project || project === current?.default_project) {
      workspaceReview = null;
      reviewBusy = false;
      return;
    }
    reviewError = '';
    reviewBusy = true;
    try {
      const result = await api.workspaceChanges();
      if (id === reviewRequest && getData()?.project === project) workspaceReview = result;
    } catch (failure) {
      if (id === reviewRequest && getData()?.project === project) {
        workspaceReview = null;
        reviewError = failure instanceof Error ? failure.message : String(failure);
      }
    } finally {
      if (id === reviewRequest) reviewBusy = false;
    }
  }

  function openWorkspaceReview() {
    reviewOpen = true;
    void refreshReview();
  }

  async function openTurnChanges(turn: number) {
    // During a preview or another pending navigation the Agent still answers
    // for the previous conversation; ask it nothing about this transcript.
    if (switchingTo || busy) return;
    const token = turnFilesRequests.begin();
    const session = snapshot?.session_id ?? '';
    const generation = getTaskGeneration();
    // A->B->A must not let the first A answer reopen a panel on the second A.
    const current = () => turnFilesRequests.isCurrent(token)
      && getTaskGeneration() === generation
      && (snapshot?.session_id ?? '') === session;
    try {
      const files = await api.turnFiles(turn);
      // A turn without files is a real answer, not a panel to open.
      if (!current() || !files.length) return;
      const summary = turnDiffs.get(turn)
        ?? {turn, added: 0, removed: 0, files: files.length, truncated: false};
      turnChangesView = {turn, summary, files};
    } catch (failure) {
      if (current()) showError(failure);
    }
  }

  async function loadOlderHistory() {
    const current = getData()?.snapshot;
    if (!current || historyBusy) return;
    const generation = getTaskGeneration();
    const session = current.session_id;
    historyBusy = true;
    try {
      const loaded = olderMessages.length + current.messages.length;
      const before = Math.max(0, (current.total_messages ?? loaded) - loaded);
      const page = await api.historyPage(before, 200);
      // A page can arrive after the user switched sessions. Never splice
      // another conversation's messages into the visible transcript.
      if (!historyPageBelongsTo(page.session_id, getData()?.snapshot?.session_id, generation, getTaskGeneration())
          || page.session_id !== session) return;
      const oldHeight = scroller?.scrollHeight ?? 0;
      const oldTop = scroller?.scrollTop ?? 0;
      olderMessages = [...page.messages, ...olderMessages];
      // Expand the visible window and preserve the reader's position when
      // older messages arrive above the current viewport.
      historyCount += page.messages.length;
      historyHasMore = page.next_before != null && page.next_before > 0;
      await tick();
      if (scroller) scroller.scrollTop = scrollTopAfterPrepend(oldTop, oldHeight, scroller.scrollHeight);
    } catch (failure) { showError(failure); }
    finally { historyBusy = false; }
  }

  /** Search the messages the transcript already holds. */
  function refreshFind() {
    if (!findOpen || !findQuery.trim()) {
      findHits = [];
      findCurrent = -1;
      findNote = '';
      if (findOpen) highlightAbsolute = null;
      return;
    }
    const previous = findHits[findCurrent]?.messageIndex ?? -1;
    // Hits carry absolute indexes; the DOM and turns keep local indexes.
    findHits = findInMessages(history, findQuery, absoluteBase);
    const restored = findHits.findIndex(hit => hit.messageIndex === previous);
    findCurrent = restored >= 0 ? restored : (findHits.length ? 0 : -1);
    findNote = findScopeNote(history.length, historyHasMore, running);
    if (findCurrent >= 0) {
      highlightAbsolute = findHits[findCurrent].messageIndex;
      unfoldSelection();
      void scrollToHighlight();
    } else {
      highlightAbsolute = null;
    }
  }

  /** A hit in already loaded older messages may still be folded away. */
  function unfoldSelection() {
    if (highlightAbsolute === null) return;
    const local = highlightAbsolute - absoluteBase;
    if (local >= 0 && local < historyStart) {
      historyCount = Math.max(historyCount, history.length - local);
    }
  }

  function stepFind(direction: -1 | 1) {
    if (!findHits.length) return;
    findCurrent = stepHit(findHits, findCurrent, direction);
    highlightAbsolute = findHits[findCurrent].messageIndex;
    unfoldSelection();
    void scrollToHighlight();
  }

  async function scrollToHighlight() {
    await tick();
    const absolute = highlightAbsolute;
    if (absolute === null || !scroller) return;
    const local = absolute - absoluteBase;
    if (local < 0 || local >= history.length) return;
    const target = scroller.querySelector(`[data-message-index="${local}"]`);
    target?.scrollIntoView({block:'center'});
  }

  export function openFind() {
    findOpen = true;
    void tick().then(() => {
      findInput?.focus();
      findInput?.select();
    });
  }

  export function closeFind(): boolean {
    if (!findOpen) return false;
    findOpen = false;
    findQuery = '';
    findHits = [];
    findCurrent = -1;
    findNote = '';
    highlightAbsolute = null;
    return true;
  }

  function onFindKey(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      closeFind();
      return;
    }
    if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault();
      stepFind(event.shiftKey ? -1 : 1);
    }
  }

  /** Open a conversation hit: page older history, bounded, until the absolute
   * target is inside the loaded window, then highlight the exact message. */
  export async function revealMessage(index: number): Promise<void> {
    if (!Number.isInteger(index) || index < 0) return;
    // A session switch installs its snapshot immediately before this call; wait
    // for the reactive window to describe that conversation before measuring it.
    await tick();
    const request = ++revealRequest;
    const generation = getTaskGeneration();
    const project = getData()?.project ?? '';
    const session = getData()?.snapshot?.session_id ?? '';
    const stillCurrent = () => (getData()?.project ?? '') === project
      && getData()?.snapshot?.session_id === session;
    let pages = 0;
    while (pages < 12 && absoluteBase > index && historyHasMore && !historyBusy) {
      if (!stillCurrent()) return;
      const current = getData()?.snapshot;
      const loaded = olderMessages.length + (current?.messages.length ?? 0);
      const before = Math.max(0, (current?.total_messages ?? loaded) - loaded);
      if (before <= 0) break;
      let page;
      try {
        page = await api.historyPage(before, 500);
      } catch (failure) {
        if (request === revealRequest) showError(failure);
        return;
      }
      if (request !== revealRequest || !stillCurrent()
          || !historyPageBelongsTo(page.session_id, getData()?.snapshot?.session_id,
            generation, getTaskGeneration())
          || page.session_id !== session) return;
      if (!page.messages.length) break;
      olderMessages = [...page.messages, ...olderMessages];
      historyCount += page.messages.length;
      historyHasMore = page.next_before != null && page.next_before > 0;
      pages += 1;
      await tick();
    }
    if (request !== revealRequest || !stillCurrent()) return;
    // A task that started during paging owns newer history; do not paint a
    // selection that belongs to the previous generation.
    if (getTaskGeneration() !== generation) return;
    const local = index - absoluteBase;
    if (local < 0 || local >= history.length) {
      showError(new Error('匹配的消息在更早的历史中；请点“加载更早的完整历史”继续查找'));
      return;
    }
    highlightAbsolute = index;
    unfoldSelection();
    findOpen = false;
    await scrollToHighlight();
  }

  function trackScroll() {
    if (!scroller) return;
    atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 115;
  }
</script>

  <div class="transcript-region">
{#if findOpen}
  <div class="find-bar" role="search" aria-label="在当前会话中查找">
    <Search size={14}/>
    <input bind:this={findInput} bind:value={findQuery} onkeydown={onFindKey}
      aria-label="在当前会话中查找" placeholder="在当前会话中查找文字" />
    <span class="find-count" aria-live="polite">{findCountLabel(findHits, findCurrent)}</span>
    <button class="icon-button" title="上一个匹配（Shift+Enter）" aria-label="上一个匹配"
      disabled={!findHits.length} onclick={() => stepFind(-1)}><ChevronUp size={14}/></button>
    <button class="icon-button" title="下一个匹配（Enter）" aria-label="下一个匹配"
      disabled={!findHits.length} onclick={() => stepFind(1)}><ChevronDown size={14}/></button>
    <button class="icon-button" title="关闭查找（Esc）" aria-label="关闭查找" onclick={closeFind}><X size={14}/></button>
    {#if findNote}<span class="find-note">{findNote}</span>{/if}
  </div>
{/if}
<div class="transcript" class:at-bottom={atBottom} bind:this={scroller} onscroll={trackScroll} aria-busy={!!switchingTo}>
  {#if switchingTo && (!previewSnapshot || history.length === 0)}
    <div class="session-loading" class:session-preview-note={!!previewSnapshot} role="status">
      <LoaderCircle size={18} class="spin"/>
      <span>{previewSnapshot ? '正在同步最新记录…' : '正在打开会话…'}</span>
    </div>
  {:else if history.length === 0 && !running}
    <div class="welcome">
      <div class="welcome-mark" aria-hidden="true">
        <svg width="28" height="28" viewBox="0 0 24 24" fill="currentColor"><path d="M4 3h4v18H4zM10 12l7-9h5l-7 9 7 9h-5z"/></svg>
      </div>
      <div class="welcome-location" title={data?.project}><FolderClosed size={13}/><span>{data?.project === data?.default_project ? '个人对话' : workspaceDisplayName(data?.project ?? '', data?.user_home)}</span></div>
      <h1>从这里，开始下一步。</h1>
      <p>{ready ? '描述你想完成的事，或选一个起点。' : '先连接一个模型，再把你的想法带进工作区。'}</p>
      {#if !ready}
        <button class="primary" onclick={() => onSetTab('settings')}><Settings2 size={16}/> 连接模型，开始使用</button>
      {:else}
        <div class="welcome-prompts" aria-label="示例任务">
          <button onclick={() => onUsePrompt('检查当前项目，找出值得优先修复的问题')}><ScanSearch size={18}/><strong>检查项目</strong><small>找出值得优先修复的问题</small><ChevronRight size={14}/></button>
          <button onclick={() => onUsePrompt('运行测试并修复发现的错误')}><Wrench size={18}/><strong>修复错误</strong><small>从测试失败开始排查</small><ChevronRight size={14}/></button>
          <button onclick={() => onUsePrompt('帮我实现一个新功能')}><GitPullRequest size={18}/><strong>实现功能</strong><small>把一个想法落到代码里</small><ChevronRight size={14}/></button>
        </div>
        <small class="welcome-footnote">示例只填入草稿，不会自动执行。</small>
      {/if}
    </div>
  {:else}
    <div class="transcript-inner">
      {#if switchingTo}
        <div class="session-preview-note" role="status">
          <LoaderCircle size={13} class="spin"/> 正在同步最新记录…
        </div>
      {/if}
      {#if historyHasMore}
        <button class="load-earlier" onclick={loadOlderHistory} disabled={historyBusy || busy}>
          {historyBusy ? '加载中…' : '加载更早的完整历史'}
        </button>
      {/if}
      {#if history.length > historyCount}
        <button class="load-earlier" disabled={busy} onclick={() => historyCount += 100}>显示更早的 {Math.min(100, history.length - historyCount)} 条消息</button>
      {/if}
      {#each visibleTurns as turn,i (turn.id)}
        {@const checkpointTurn = turnNumbers.get(turn.id)}
        {@const agentIndex = turn.id - olderMessages.length}
        {@const agentTurn = canActOnTurn(snapshot?.messages ?? [], agentIndex) ? agentIndex : -1}
        <TranscriptTurn {turn} searchHighlight={highlightIndex} {showError}
          onRewind={agentTurn >= 0 ? () => onRewind(agentTurn, checkpointTurn ?? null, turnDiffFor(turnDiffs, checkpointTurn ?? null)) : null}
          onFork={agentTurn >= 0 ? () => onFork(agentTurn) : null}
          onEdit={agentTurn >= 0 ? () => onEdit(agentTurn, (turn.user?.content ?? '').replace(/[\uE000-\uF8FF]/g, '')) : null}
          changes={checkpointTurn == null ? null : turnDiffs.get(checkpointTurn) ?? null}
          onViewTurnChanges={() => checkpointTurn != null && void openTurnChanges(checkpointTurn)}
          showWorkspaceChanges={i === visibleTurns.length - 1 && hasWorkspaceChanges}
          changeCount={workspaceReview?.entries.length ?? 0}
          onViewChanges={openWorkspaceReview}/>
      {/each}
      {#if running && !taskSnapshotReceived}
        <div class="conversation-turn">
          {#if draftPrompt}
            <div class="prompt draft">
              <p class="prompt-text">{draftPrompt}</p>
            </div>
          {/if}
          <article class="answer live-message">
            <LiveTimeline activities={liveActivities} elapsed={activeElapsed}/>
          </article>
        </div>
      {/if}
    </div>
  {/if}
  <div class="transcript-widgets">
    <PluginWidgets placement="transcript" busy={widgetActionBusy} onAction={onWidgetAction}/>
  </div>
</div>
{#if !atBottom}
  <button class="scroll-latest" title="回到最新消息" aria-label="回到最新消息" onclick={scrollDown}><ArrowDown size={17}/></button>
{/if}
</div>

{#if turnChangesView}
  <TurnChanges turn={turnChangesView.turn} summary={turnChangesView.summary}
    files={turnChangesView.files} onClose={() => turnChangesView = null}/>
{/if}
{#if reviewOpen && data}
  <WorkspaceChanges project={data.project} review={workspaceReview}
    busy={reviewBusy} error={reviewError}
    onRefresh={() => void refreshReview()}
    onClose={() => reviewOpen = false}/>
{/if}

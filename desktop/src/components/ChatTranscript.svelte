<script lang="ts">
  import { tick } from 'svelte';
  import { ArrowDown, ChevronRight, LoaderCircle, Settings2 } from '@lucide/svelte';
  import { api, type Bootstrap, type CheckpointTurn, type Message, type SessionSnapshot, type TurnDiffSummary, type WorkspaceReview } from '../lib/api';
  import { canActOnTurn, turnDiffFor, turnNumberByUserIndex } from '../lib/checkpoints';
  import { scrollTopAfterPrepend } from '../lib/chat-ui';
  import { carryOlderHistory } from '../lib/snapshot';
  import { createRequestTokens } from '../lib/latest';
  import { historyPageBelongsTo } from '../lib/snapshot';
  import { groupTranscript, visibleTranscriptStart, type LiveActivity } from '../lib/transcript';
  import type { Tab } from '../lib/ui-types';
  import LiveTimeline from './LiveTimeline.svelte';
  import TranscriptTurn from './TranscriptTurn.svelte';
  import TurnChanges from './TurnChanges.svelte';
  import WorkspaceChanges from './WorkspaceChanges.svelte';

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

  let olderMessages: Message[] = [];
  let historyHasMore = false;
  let historyBusy = false;
  let historyCount = 72;
  let scroller: HTMLDivElement;
  let atBottom = true;
  let checkpointTurns: CheckpointTurn[] = [];
  let turnDiffs = new Map<number, TurnDiffSummary>();
  let checkpointKey = '';
  // A slow checkpoint response must never paint another conversation's turns.
  const checkpointRequests = createRequestTokens();
  let turnChangesView: {turn: number; summary: TurnDiffSummary; files: Array<{path:string;status:string;added:number;removed:number}>} | null = null;
  let reviewOpen = false;
  let reviewBusy = false;
  let reviewError = '';
  let workspaceReview: WorkspaceReview | null = null;
  let reviewRequest = 0;

  $: history = [...olderMessages, ...(snapshot?.messages ?? [])];
  $: historyStart = visibleTranscriptStart(history, historyCount);
  $: visibleTurns = groupTranscript(history.slice(historyStart), historyStart);
  $: turnNumbers = turnNumberByUserIndex(checkpointTurns, history);
  $: hasWorkspaceChanges = !!workspaceReview?.is_repo && workspaceReview.entries.length > 0;
  $: {
    const key = `${snapshot?.session_id ?? ''}|${snapshot?.messages.length ?? 0}`;
    if (key !== checkpointKey) {
      checkpointKey = key;
      checkpointTurns = [];
      turnDiffs = new Map();
      // While a preview is shown the Agent still owns the previous session, so
      // a lookup here would attach another conversation's turns to the preview.
      if (!switchingTo && snapshot?.session_id && snapshot.session_id !== 'unknown') {
        void refreshCheckpoints();
      }
    }
  }

  export function syncHistory() {
    olderMessages = [];
    historyHasMore = getData()?.snapshot?.history_truncated ?? false;
    historyCount = 72;
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
    const nextStart = Math.max(0, (next.total_messages ?? next.messages.length) - next.messages.length);
    historyHasMore = nextStart > olderMessages.length;
  }

  export async function refreshCheckpoints() {
    const token = checkpointRequests.begin();
    try {
      const result = await api.checkpointTurns();
      if (!checkpointRequests.isCurrent(token)) return;
      checkpointTurns = result.turns ?? [];
      turnDiffs = new Map((result.diffs ?? []).map(diff => [diff.turn, diff]));
    } catch {
      if (!checkpointRequests.isCurrent(token)) return;
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
    try {
      const files = await api.turnFiles(turn);
      if (!files.length) return;
      const summary = turnDiffs.get(turn)
        ?? {turn, added: 0, removed: 0, files: files.length, truncated: false};
      turnChangesView = {turn, summary, files};
    } catch (failure) { showError(failure); }
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

  function trackScroll() {
    if (!scroller) return;
    atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 115;
  }
</script>

<div class="transcript" class:at-bottom={atBottom} bind:this={scroller} onscroll={trackScroll} aria-busy={!!switchingTo}>
  {#if switchingTo && !previewSnapshot}
    <div class="session-loading" role="status">
      <LoaderCircle size={18} class="spin"/>
      <span>正在打开会话…</span>
    </div>
  {:else if history.length === 0 && !running}
    <div class="welcome">
      <div class="welcome-mark">K</div>
      <h1>有什么想做的？</h1>
      <p>直接描述目标，Kroma 会在当前工作目录中帮你完成。</p>
      {#if !ready}
        <button class="primary" onclick={() => onSetTab('settings')}><Settings2 size={16}/> 连接模型，开始使用</button>
      {:else}
        <div class="welcome-prompts" aria-label="示例任务">
          <button onclick={() => onUsePrompt('检查当前项目，找出值得优先修复的问题')}>检查项目 <ChevronRight size={13}/></button>
          <button onclick={() => onUsePrompt('运行测试并修复发现的错误')}>修复错误 <ChevronRight size={13}/></button>
          <button onclick={() => onUsePrompt('帮我实现一个新功能')}>实现功能 <ChevronRight size={13}/></button>
        </div>
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
        <TranscriptTurn {turn}
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
          <article class="message user draft">
            <div class="message-body"><div class="user-bubble">{draftPrompt}</div></div>
          </article>
          <article class="message assistant live-message">
            <div class="avatar agent-avatar">K</div>
            <div class="message-body">
              <div class="message-author">Kroma <span class="streaming-status"><span class="live-dot"></span> 正在工作</span></div>
              <LiveTimeline activities={liveActivities} elapsed={activeElapsed}/>
            </div>
          </article>
        </div>
      {/if}
    </div>
  {/if}
</div>
{#if !atBottom}
  <button class="scroll-latest" title="回到最新消息" aria-label="回到最新消息" onclick={scrollDown}><ArrowDown size={17}/></button>
{/if}

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

<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { AlertCircle, LoaderCircle, Paperclip, X } from '@lucide/svelte';
  import {
    api, type Bootstrap, type DesktopSurface, type PluginUiEvent,
    type SessionSnapshot, type DesktopEvent, type StreamEnvelope,
  } from './lib/api';
  import { createLatestNavigation, sessionLabel } from './lib/chat-ui';
  import {
    applySnapshot, applySessionSwitch, createOperationGuard, isActiveTask, sessionPreview,
  } from './lib/snapshot';
  import { steerNotice, type SteerOutcome } from './lib/steering';
  import {
    enqueueFollowup, hasPersistedSessionId, loadFollowups, moveFollowup,
    saveFollowups, shouldStartNextFollowup, MAX_FOLLOWUPS, type FollowupTask,
  } from './lib/followups';
  import {
    mergeMigrated, queuedForSession, sessionFollowupKey,
  } from './lib/followup-session';
  import { appDrafts, browserDraftStorage } from './lib/draft-cache';
  import { announceTaskResult, ensureNotificationPermission } from './lib/task-notify';
  import { createSessionCommands } from './lib/session-commands';
  import { createTurnActions } from './lib/turn-actions';
  import type { DesktopHost, RewindTarget } from './lib/desktop-host';
  import type { SessionDestination, SessionTarget, Tab, TaskResultStatus } from './lib/ui-types';
  import type { LiveActivity } from './lib/transcript';
  import { foldLiveEvents } from './lib/live-events';
  import {
    applyPluginUiEvent, applyPresentation, applySnapshotPluginUi, desktopSurfaces, livePluginPanel,
    loadPresentation, pluginColors, pluginShortcuts, pluginThemeStyle, pluginWidgets, shortcutFor,
  } from './lib/plugin-ui';
  import {
    dismissAnswer, nextAsk, readAsk, type AskAnswer, type AskQuestion,
  } from './lib/ask-question';
  import SessionSidebar from './components/SessionSidebar.svelte';
  import Topbar from './components/Topbar.svelte';
  import ChatTranscript from './components/ChatTranscript.svelte';
  import ComposerPanel from './components/ComposerPanel.svelte';
  import AppBanner from './components/AppBanner.svelte';
  import SettingsView from './components/SettingsView.svelte';
  import PluginsView from './components/PluginsView.svelte';
  import PanelBlocksView from './components/PanelBlocksView.svelte';
  import PluginWidgets from './components/PluginWidgets.svelte';
  import PluginDesktopFrame from './components/PluginDesktopFrame.svelte';
  import AddModelDialog from './components/AddModelDialog.svelte';
  import AbilitiesDialog from './components/AbilitiesDialog.svelte';
  import RewindDialog from './components/RewindDialog.svelte';

  let data: Bootstrap | null = null;
  let tab: Tab = 'chat';
  let error = '';
  let notice = '';
  let busy = false;
  let running = false;
  let initialized = false;
  let mcpDismissed = false;
  let rename = false;
  let newTitle = '';
  let sidebarVisible = true;
  let narrowLayout = false;
  let draggingFiles = false;
  let showAddModel = false;
  let abilitiesOpen = false;
  let compacting = false;
  let rewinding = false;
  let recentlyDeleted: SessionTarget[] = [];
  let rewindTarget: RewindTarget | null = null;
  let liveActivities: LiveActivity[] = [];
  let liveSequence = 0;
  let modelStartIndex = 0;
  let draftPrompt = '';
  let taskStartedAt = 0;
  let activeElapsed = 0;
  let taskResult: TaskResultStatus = '';
  let cancellationRequested = false;
  let taskSnapshotReceived = false;
  // The plugin question that currently owns the composer slot.
  let pendingAsk: AskQuestion | null = null;
  let followups: FollowupTask[] = [];
  let followupScope = '';
  let followupPaused = true;
  let followupExpanded = false;
  let followupSequence = 0;
  let taskGeneration = 0;
  let switchingTo = '';
  let navigationBusy = false;
  let navigationProject = '';
  let previewSnapshot: SessionSnapshot | null = null;
  let lastSnapshotRef: SessionSnapshot | null = null;
  let widgetActionBusy = false;
  let openDesktopModal: DesktopSurface | null = null;
  const previewCache = new Map<string, SessionSnapshot>();
  let sidebar: SessionSidebar;
  let chat: ChatTranscript;
  let composer: ComposerPanel;
  // Token owner for operations that install a whole bootstrap. A late response
  // from a superseded operation must not overwrite the project or session the
  // user is looking at now.
  const bootstrapGuard = createOperationGuard();

  $: snapshot = switchingTo ? previewSnapshot : (data?.snapshot ?? null);
  $: if (data?.snapshot !== lastSnapshotRef) {
    lastSnapshotRef = data?.snapshot ?? null;
    applySnapshotPluginUi(lastSnapshotRef);
  }
  $: if (openDesktopModal && !$desktopSurfaces.some(s => s.plugin_id === openDesktopModal?.plugin_id)) {
    openDesktopModal = null;
  }
  $: if (tab.startsWith('plugin:') && !$desktopSurfaces.some(s => s.plugin_id === tab.slice('plugin:'.length))) {
    tab = 'chat';
  }
  $: sidebarWidgets = $pluginWidgets.filter(w => w.placement === 'sidebar');
  $: sidebarDesktopSurfaces = $desktopSurfaces.filter(s => s.slot === 'sidebar');
  $: composerDesktopSurfaces = $desktopSurfaces.filter(s => s.slot === 'above_composer');
  $: activePluginTabSurface = tab.startsWith('plugin:')
    ? $desktopSurfaces.find(s => s.plugin_id === tab.slice('plugin:'.length)) ?? null
    : null;
  $: hasRightDock = sidebarWidgets.length > 0 || sidebarDesktopSurfaces.length > 0;
  $: appThemeStyle = pluginThemeStyle($pluginColors);
  $: modelName = data?.connected_model || '未连接模型';
  // The taskbar is the cheapest "still working" signal on a desktop.
  $: windowTitle = `${running ? '● ' : ''}${currentSessionName || 'Kroma'} — Kroma`;
  $: if (initialized) void getCurrentWindow().setTitle(windowTitle).catch(() => {});
  $: connectedProfile = data?.connected_profile || '';
  $: reasoningLevel = data?.reasoning_level || 'auto';
  $: reasoningLevels = Array.from(new Set(['auto', ...(data?.thinking_levels?.length ? data.thinking_levels : ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']), reasoningLevel]));
  $: activeSessionId = switchingTo || snapshot?.session_id;
  $: selectedProject = navigationBusy && navigationProject ? navigationProject : data?.project;
  // Look up the highlighted conversation in its own project group. During
  // cross-project navigation, the old project's flat session list is stale.
  $: currentSessionEntry = (data?.session_groups ?? []).find(group => group.project === selectedProject)
    ?.sessions.find(session => session.id === activeSessionId)
    ?? data?.sessions.find(session => session.id === activeSessionId);
  $: currentSessionName = currentSessionEntry ? sessionLabel(currentSessionEntry) : '新会话';
  $: hasSession = !!snapshot?.session_id && snapshot.session_id !== 'unknown';
  // Mirrors the original render branches: the chat pane owns the fallback case.
  $: chatVisible = initialized && !(tab === 'settings' && data) && !(tab === 'plugins' && data) && !tab.startsWith('plugin:');

  function showError(failure: unknown) { error = failure instanceof Error ? failure.message : String(failure); }
  function toast(message: string) { notice = message; }
  function sessionProject(): string { return data?.project || ''; }
  function sessionId(): string | undefined { return data?.snapshot?.session_id; }
  function saveDraft() { composer?.saveDraft(sessionProject(), sessionId()); }
  function restoreDraft() { composer?.restoreDraft(sessionProject(), sessionId()); }

  const host: DesktopHost = {
    get data() { return data; },
    set data(value) { data = value; },
    get snapshot() { return snapshot; },
    get tab() { return tab; },
    set tab(value) { tab = value; },
    get busy() { return busy; },
    set busy(value) { busy = value; },
    get running() { return running; },
    get initialized() { return initialized; },
    set initialized(value) { initialized = value; },
    get compacting() { return compacting; },
    set compacting(value) { compacting = value; },
    get rewinding() { return rewinding; },
    set rewinding(value) { rewinding = value; },
    get rewindTarget() { return rewindTarget; },
    set rewindTarget(value) { rewindTarget = value; },
    get connectedProfile() { return connectedProfile; },
    get reasoningLevel() { return reasoningLevel; },
    get sessionName() { return currentSessionName; },
    get taskGeneration() { return taskGeneration; },
    set taskGeneration(value) { taskGeneration = value; },
    get mcpDismissed() { return mcpDismissed; },
    set mcpDismissed(value) { mcpDismissed = value; },
    guard: bootstrapGuard,
    clearError() { error = ''; },
    clearNotice() { notice = ''; },
    setRunning(value) { running = value; },
    resetTaskResult() { taskResult = ''; },
    clearLiveActivities() { liveActivities = []; },
    saveDraft,
    resetHistoryWindow() { chat?.resetWindow(); },
    collapseSidebarOnNarrow() { if (narrowLayout) sidebarVisible = false; },
    endRename() { rename = false; },
    clearDeletedSessions() { recentlyDeleted = []; },
    showError,
    toast,
    syncHistory() { chat?.syncHistory(); },
    restoreDraft,
    restoreFollowups,
    refreshReview() { void chat?.refreshReview(); },
    async scrollDown() { await chat?.scrollDown(); },
    async scrollIfBottom() { await chat?.scrollIfBottom(); },
    focusComposer() { composer?.focus(); },
    setComposerDraft(text) { composer?.setDraftText(text); },
    clearWorkspaceReview() { chat?.clearReview(); },
    expandProject(project) { sidebar?.expandProject(project); },
  };
  const commands = createSessionCommands(host);
  const turns = createTurnActions(host);

  const navigation = createLatestNavigation<SessionDestination>(
    performNavigation,
    finishNavigation,
    failure => {
      // A failed intermediate destination must not leave an error banner
      // covering the final conversation when a newer click is waiting.
      if (!navigation.hasPending) showError(failure);
    },
  );

  function previewKey(project: string, id: string) {
    return project + '::' + id;
  }
  function rememberPreview(project: string, snapshot: SessionSnapshot) {
    if (!snapshot.session_id || snapshot.session_id === 'unknown') return;
    const key = previewKey(project, snapshot.session_id);
    previewCache.delete(key);
    previewCache.set(key, sessionPreview(snapshot));
    // Only a handful of previously visited sessions. Never cache full WALs.
    if (previewCache.size > 5) {
      const oldest = previewCache.keys().next().value;
      if (oldest !== undefined) previewCache.delete(oldest);
    }
  }

  function openGroupSession(project: string, id: string) {
    if (!data || running || compacting || (busy && !navigationBusy)) return;
    if (!navigationBusy && project === data.project && id === data.snapshot?.session_id) {
      tab = 'chat';
      return;
    }
    if (!navigationBusy) {
      saveDraft();
      if (data.snapshot) rememberPreview(data.project, data.snapshot);
      taskGeneration += 1; // reject late history pages from the previous chat
      bootstrapGuard.begin();
      chat?.resetWindow();
      navigationBusy = true;
      busy = true;
      error = '';
      taskResult = '';
    }

    // The latest click is visible immediately, but only the one active Agent
    // may change actual sessions. Intermediate requests are coalesced.
    navigationProject = project;
    sidebar?.expandProject(project);
    switchingTo = id;
    previewSnapshot = previewCache.get(previewKey(project, id)) ?? null;
    tab = 'chat';
    if (narrowLayout) sidebarVisible = false;
    void chat?.scrollDown();
    navigation.request({project, id});
  }

  async function performNavigation({project, id}: SessionDestination) {
    if (!data) return;
    try {
      if (project !== data.project) {
        data = await api.openProject(project);
        chat?.clearReview();
        liveActivities = [];
        applyPresentation(null);
        openDesktopModal = null;
      }
      // A previous project change may have released the Runtime, even if a
      // newer click now targets that same project. Reconnect before resuming.
      if (navigation.hasPending) return;
      if (!data.ready) {
        if (data.profiles.profiles.length || data.plugins.some(plugin => plugin.enabled && plugin.has_model)) {
          data = await api.connect();
          void loadPresentation();
        } else {
          tab = 'settings';
          throw new Error('请先连接模型，然后再打开该工作区的会话');
        }
      }
      if (navigation.hasPending) return;
      if (data.snapshot?.session_id !== id) {
        const result = await api.switchSession(id);
        livePluginPanel.set(null);
        openDesktopModal = null;
        data = {
          ...applySessionSwitch(data, result.snapshot, result.title),
          connected_model: result.connected_model,
          reasoning_level: result.reasoning_level,
          thinking_levels: result.thinking_levels,
        };
        rememberPreview(project, result.snapshot);
      }
    } catch (failure) {
      // An interrupted openProject/switchSession can leave the backend in a
      // newer state even when the IPC reports failure. Never assume that the
      // visible stale snapshot is authoritative after an error.
      try { data = await api.bootstrap(); }
      catch { /* Preserve the original navigation error. */ }
      throw failure;
    }
  }

  function finishNavigation() {
    navigationBusy = false;
    busy = false;
    rename = false;
    switchingTo = '';
    navigationProject = '';
    previewSnapshot = null;
    chat?.syncHistory();
    restoreDraft();
    restoreFollowups();
    // The preview suppressed the checkpoint lookup; the real snapshot is
    // visible now, so load the turns that belong to it.
    void chat?.refreshCheckpoints();
    if (data?.ready) tab = 'chat';
    void chat?.scrollDown();
    if (tab === 'chat') composer?.focus();
    void chat?.refreshReview();
  }

  async function newChatInGroup(project: string) {
    if (running || busy) return;
    if (project !== data?.project) {
      await commands.chooseProject(project);
      if (data?.project !== project || !data?.ready) return;
    }
    await commands.newSession();
  }

  onMount(() => {
    void ensureNotificationPermission();
    const resize = () => {
      const nextNarrow = window.innerWidth < 880;
      if (nextNarrow !== narrowLayout) {
        narrowLayout = nextNarrow;
        sidebarVisible = !nextNarrow;
      }
    };
    narrowLayout = window.innerWidth < 880;
    sidebarVisible = !narrowLayout;
    window.addEventListener('resize', resize);
    // A message may still be queued for task steering while the Agent runs.
    const saveOnExit = () => { if (data) saveDraft(); };
    window.addEventListener('beforeunload', saveOnExit);
    const interval = window.setInterval(() => {
      if (running) activeElapsed = Math.floor((Date.now() - taskStartedAt) / 1000);
    }, 1000);
    void commands.initialize();
    let stop: (() => void) | undefined;
    let stopPluginUi: (() => void) | undefined;
    let mounted = true;
    void listen<PluginUiEvent>('plugin_ui', event => {
      if (!mounted || !event.payload) return;
      applyPluginUiEvent(event.payload, {
        onNotify: (message, level) => {
          if (level === 'error') error = message;
          else toast(message);
        },
        onComposerSet: (text, mode) => {
          composer?.setDraftText(text, mode);
        },
      });
    }).then(fn => { if (mounted) stopPluginUi = fn; else fn(); }).catch(() => {});
    void getCurrentWebview().onDragDropEvent(event => {
      if (event.payload.type === 'enter') draggingFiles = tab === 'chat';
      else if (event.payload.type === 'leave') draggingFiles = false;
      else if (event.payload.type === 'drop') {
        draggingFiles = false;
        if (tab === 'chat') composer?.addPaths(event.payload.paths);
      }
    }).then(fn => { if (mounted) stop = fn; else fn(); }).catch(() => {});
    return () => {
      mounted = false; stop?.(); stopPluginUi?.();
      saveOnExit();
      composer?.cancelDraftTimer();
      window.removeEventListener('beforeunload', saveOnExit);
      window.removeEventListener('resize',resize);
      window.clearInterval(interval);
      composer?.releasePreviews();
    };
  });

  function installTaskSnapshot(next: SessionSnapshot, sessions?: Bootstrap['sessions'], groups?: Bootstrap['session_groups']) {
    if (!data) return;
    const previous = data.snapshot;
    const sameSession = previous?.session_id === next.session_id;
    if (previous && sameSession) chat?.carrySnapshot(previous, next);
    data = applySnapshot(data, next);
    if (sessions) data = {...data, sessions};
    if (groups) data = {...data, session_groups:groups};
    if (!sameSession) {
      chat?.syncHistory();
      if (running) migrateFollowups(previous?.session_id, next.session_id);
      else restoreFollowups();
    }
  }

  // Fold every native event batch before committing one UI update. A tool
  // stays at its original position even when text arrives while it is running.
  function applyEvents(events: DesktopEvent[]) {
    const folded = foldLiveEvents({
      activities: liveActivities, sequence: liveSequence, modelStartIndex
    }, events);
    liveActivities = folded.state.activities;
    liveSequence = folded.state.sequence;
    modelStartIndex = folded.state.modelStartIndex;
    for (const warning of folded.warnings) toast(warning);
    // Dropped live events leave a gap that only the durable history can fill.
    if (folded.resync) void commands.refresh();
    void chat?.scrollIfBottom();
  }

  // One question at a time: a newer one supersedes the open bar, and the
  // superseded question is answered `dismissed` so no plugin waits forever.
  function receiveAsk(raw: unknown) {
    const incoming = readAsk(raw);
    if (!incoming) return;
    const {pending, dismiss} = nextAsk(pendingAsk, incoming);
    pendingAsk = pending;
    if (dismiss !== null) void answerQuestion(dismissAnswer(dismiss));
  }

  // Closing the bar is never approval: only a real pick or a typed answer
  // leaves `dismissed` false. The bar disappears first, so the composer is
  // usable again even if the host rejects an answer for a finished question.
  async function answerQuestion(answer: AskAnswer) {
    if (pendingAsk?.id === answer.id) pendingAsk = null;
    try { await api.answerQuestion(answer); }
    catch (failure) { showError(failure); }
    // The composer is back in its slot; the person keeps typing where they left off.
    void tick().then(() => composer?.focus());
  }

  function onStream(event: StreamEnvelope, generation: number) {
    if (!isActiveTask(generation, taskGeneration)) return;
    if (event.kind === 'batch') applyEvents(event.events);
    if (event.kind === 'ask') receiveAsk(event.ask);
    if (event.kind === 'snapshot' && data) {
      taskSnapshotReceived = true;
      installTaskSnapshot(event.snapshot, event.sessions, event.session_groups);
      void chat?.scrollIfBottom();
    }
    if (event.kind === 'done') {
      taskResult = event.result.status as TaskResultStatus;

      if (event.result.error) error = event.result.error;
      else if (event.result.status === 'cancelled') toast('任务已停止；此前执行的操作不会自动撤销');
    }
  }

  async function executeTask(text: string, paths: string[], fromQueue: boolean) {
    if (running || busy || compacting || !data?.ready || (!text.trim() && !paths.length)) return;
    const generation = ++taskGeneration;
    let sawSnapshot = false;
    let appliedSnapshot = false;
    let completed = false;
    if (!fromQueue) composer?.clearForTask(sessionProject(), sessionId());
    draftPrompt = text || ('已添加 ' + paths.length + ' 个文件');
    running = true;
    liveActivities = [];
    liveSequence = 0;
    modelStartIndex = 0;
    activeElapsed = 0;
    taskStartedAt = Date.now();
    taskResult = '';
    cancellationRequested = false;
    taskSnapshotReceived = false;
    pendingAsk = null;
    error = '';
    await chat?.scrollDown();
    try {
      const result = await api.send(text, paths, event => {
        if (event.kind === 'snapshot' && isActiveTask(generation, taskGeneration)) {
          if (appliedSnapshot) return;
          sawSnapshot = true;
        }
        onStream(event, generation);
      });
      composer?.releasePreviews(paths);
      if (result.result.error) error = result.result.error;
      if (!sawSnapshot && isActiveTask(generation, taskGeneration) && data) {
        installTaskSnapshot(result.snapshot, result.sessions, result.session_groups);
        appliedSnapshot = true;
      }
      taskResult = result.result.status as TaskResultStatus;
      // A slash command replies with text instead of a model answer; the user
      // must see it, or the command would look like it did nothing.
      if (result.result.text) toast(result.result.text);
      // A plugin may have been installed or edited outside this window.
      void loadPresentation();

      completed = result.result.status === 'completed';
      void announceTaskResult({
        status: taskResult, sessionName: currentSessionName, messages: result.snapshot.messages,
      });
      if (result.result.status === 'cancelled') {
        toast('任务已停止；后续任务已暂停，之前执行的操作不会自动撤销');
      }
    } catch (failure) {
      showError(failure);
      taskResult = 'failed';
      // Ambiguous IPC failures are never automatically retried. Keep the
      // instruction available for a deliberate retry without overwriting
      // anything the user typed while the task was in flight.
      if (!sawSnapshot && !appliedSnapshot && composer?.isEmpty()) {
        composer?.restoreInput(text, paths, sessionProject(), sessionId());
      }
    } finally {
      if (isActiveTask(generation, taskGeneration)) {
        running = false;
        draftPrompt = '';
        liveActivities = [];
        // A finished task cannot still be waiting for an answer.
        pendingAsk = null;
        if (!completed && followups.length) {
          followupPaused = true;
          followupExpanded = true;
        }
        if (!sawSnapshot && !appliedSnapshot) {
          try { await commands.refresh(); } catch (failure) { showError(failure); }
        }
        await chat?.scrollIfBottom();
        void chat?.refreshReview();
        if (shouldStartNextFollowup(completed ? 'completed' : 'failed', followupPaused, followups.length)) {
          await runNextFollowup();
        }
      }
    }
  }

  function sendTask(text: string, paths: string[]) {
    void executeTask(text, paths, false);
  }
  function blockedSend() {
    if (!data?.ready) tab = 'settings';
  }
  async function steerTask(instruction: string): Promise<SteerOutcome | null> {
    try {
      const outcome = await api.steer(instruction);
      toast(steerNotice(outcome));
      return outcome ?? {};
    } catch (failure) {
      showError(failure); // Keep the text intact for a deliberate retry.
      return null;
    }
  }

  function queueFollowup(text: string): boolean {
    if (!running || !text.trim()) return false;
    if (!followupScope) restoreFollowups();
    const queued = enqueueFollowup(
      followups, text, `followup-${Date.now()}-${++followupSequence}`
    );
    if (!queued) {
      toast(`最多排队 ${MAX_FOLLOWUPS} 项，每项不超过 12000 字`);
      return false;
    }
    const firstPending = followups.length === 0;
    followups = queued;
    if (firstPending && !cancellationRequested) followupPaused = false;
    followupExpanded = true;
    persistFollowups();
    toast('已加入后续任务队列');
    return true;
  }

  async function runNextFollowup() {
    if (followupPaused || running || busy || compacting || !data?.ready || !followups.length) return;
    const next = followups[0];
    // Remove before sending. A crash/timeout must never silently rerun a
    // potentially destructive task. A failed submission is restored as draft.
    followups = followups.slice(1);
    persistFollowups();
    await executeTask(next.text, [], true);
  }

  async function cancelTask() {
    // Stopping the current task must never start its queued successors,
    // even if the cancellation races a normally completed model response.
    cancellationRequested = true;
    followupPaused = true;
    try { await api.cancel(); } catch (failure) { showError(failure); }
  }

  /** Follow-ups belong to the actual project + conversation, not a global
   * Desktop queue. Saved entries never auto-run after a restart. */
  function restoreFollowups() {
    if (!data) return;
    const key = sessionFollowupKey(data.project, data.snapshot?.session_id);
    if (key === followupScope) return;
    followupScope = key;
    // 'unknown' can be reused by many different unsent conversations. Never
    // restore a queued task under that placeholder into a different session.
    followups = queuedForSession(browserDraftStorage(), data.project, data.snapshot?.session_id);
    followupPaused = true;
    followupExpanded = followups.length > 0;
  }
  function persistFollowups() {
    if (!data || !hasPersistedSessionId(data.snapshot?.session_id)) return;
    if (followupScope) saveFollowups(browserDraftStorage(), followupScope, followups);
  }
  function migrateFollowups(previousId: string | undefined, nextId: string) {
    if (!data || !nextId || previousId === nextId) return;
    const from = sessionFollowupKey(data.project, previousId || 'unknown');
    if (followupScope !== from) return;
    const target = sessionFollowupKey(data.project, nextId);
    const saved = loadFollowups(browserDraftStorage(), target);
    // A new conversation receives its real session ID during its first run.
    // Never replace a pre-existing queue belonging to the new ID.
    followups = mergeMigrated(saved, followups);
    saveFollowups(browserDraftStorage(), from, []);
    followupScope = target;
    persistFollowups();
  }
  function removeFollowup(id: string) {
    followups = followups.filter(item => item.id !== id);
    persistFollowups();
  }
  function reorderFollowup(id: string, direction: -1 | 1) {
    followups = moveFollowup(followups, id, direction);
    persistFollowups();
  }
  function clearFollowups() {
    followups = [];
    followupPaused = true;
    persistFollowups();
  }
  function resumeFollowups() {
    if (!followups.length || !data?.ready) return;
    followupPaused = false;
    if (!running && !busy) void runNextFollowup();
  }
  function pauseFollowups() { followupPaused = true; }
  function toggleFollowups() { followupExpanded = !followupExpanded; }

  async function afterSingleDelete(target: SessionTarget, deletedCurrent: boolean) {
    if (!deletedCurrent) return;
    appDrafts.forget(target.project, target.id);
    chat?.syncHistory();
    restoreDraft();
    restoreFollowups();
    await chat?.scrollDown();
  }
  async function afterBulkDelete() {
    chat?.syncHistory();
    restoreDraft();
    await chat?.scrollDown();
  }
  function setData(next: Bootstrap) { data = next; }
  async function reloadData() { data = await api.bootstrap(); }
  function setBusy(value: boolean) { busy = value; }
  function clearError() { error = ''; }

  async function handleWidgetAction(pluginId: string, widgetId: string, actionId: string) {
    if (widgetActionBusy) return;
    widgetActionBusy = true;
    try {
      const reply = await api.pluginUiAction(pluginId, widgetId, actionId);
      if (reply.content?.trim()) toast(reply.content);
      if (reply.task?.trim()) {
        tab = 'chat';
        if (running) queueFollowup(reply.task);
        else void executeTask(reply.task, [], true);
      }
    } catch (failure) {
      showError(failure);
    } finally {
      widgetActionBusy = false;
    }
  }

  function openDesktopPanel(pluginId: string) {
    const surface = $desktopSurfaces.find(s => s.plugin_id === pluginId);
    if (!surface) return;
    if (surface.slot === 'tab') {
      tab = `plugin:${surface.plugin_id}`;
    } else {
      openDesktopModal = surface;
    }
  }

  function handleGlobalKey(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      if (openDesktopModal) { openDesktopModal = null; event.preventDefault(); }
      else if ($livePluginPanel) { livePluginPanel.set(null); event.preventDefault(); }
      else if (showAddModel) { showAddModel = false; event.preventDefault(); }
      else if (sidebar?.closeDeleteDialog()) { event.preventDefault(); }
      else if (chat?.closeReview()) { event.preventDefault(); }
      else if (sidebar?.closeSearch()) { event.preventDefault(); }
      else if (rename) { rename = false; event.preventDefault(); }
      else if (narrowLayout && sidebarVisible) { sidebarVisible = false; event.preventDefault(); }
      return;
    }
    // Modal dialogs own their keyboard interaction; global shortcuts must
    // never unexpectedly switch tabs or sessions behind an open dialog.
    if (openDesktopModal || $livePluginPanel || showAddModel || sidebar?.hasBlockingDialog() || chat?.isReviewOpen()) return;
    // A plugin's declared Alt+letter shortcut runs its command as if typed.
    const shortcut = shortcutFor(event, $pluginShortcuts);
    if (shortcut) {
      event.preventDefault();
      void executeTask(shortcut.command, [], true);
      return;
    }
    if (event.defaultPrevented || event.isComposing || event.altKey || !(event.ctrlKey || event.metaKey)) return;
    const key = event.key.toLowerCase();
    if (key === 'k') {
      event.preventDefault();
      tab = 'chat';
      sidebarVisible = true;
      sidebar?.openSearch();
    } else if (key === 'n' && data?.ready && !running && !busy && !compacting) {
      event.preventDefault();
      void commands.newSession().then(() => composer?.focus());
    } else if (key === 'b') {
      event.preventDefault();
      sidebarVisible = !sidebarVisible;
    } else if (key === 'o') {
      event.preventDefault();
      void commands.chooseProject();
    } else if (event.key === ',') {
      event.preventDefault();
      tab = 'settings';
    }
  }
</script>

<svelte:head><title>Kroma Desktop</title></svelte:head>
<svelte:window onkeydown={handleGlobalKey} />
<div class="app-shell" class:sidebar-visible={sidebarVisible} class:narrow-layout={narrowLayout} style={appThemeStyle}>
  {#if narrowLayout && sidebarVisible}
    <button class="sidebar-backdrop" aria-label="关闭侧边栏" onclick={() => sidebarVisible = false}></button>
  {/if}
  <SessionSidebar bind:this={sidebar}
    {data} {tab} {selectedProject} {activeSessionId} {running} {busy} {compacting} {navigationBusy}
    guard={bootstrapGuard} getSnapshot={() => snapshot}
    onNewSession={() => void commands.newSession()}
    onChooseProject={(path) => void commands.chooseProject(path)}
    onOpenSession={openGroupSession}
    onNewChatInProject={(project) => void newChatInGroup(project)}
    onSetTab={(next) => tab = next}
    setData={setData} reloadData={reloadData}
    afterSingleDelete={afterSingleDelete} afterBulkDelete={afterBulkDelete}
    setBusy={setBusy} clearError={clearError} showError={showError} toast={toast}
    onDeleted={(targets) => recentlyDeleted = targets}/>
  <main class="main-area">
    <Topbar {tab} {currentSessionName} {hasSession} {rename} {newTitle} {sidebarVisible} {busy} {running}
      ready={!!data?.ready} profiles={data?.profiles.profiles ?? []} {connectedProfile} {modelName}
      {reasoningLevel} {reasoningLevels} thinkingEnabled={!!data?.reasoning_level}
      onToggleSidebar={() => sidebarVisible = !sidebarVisible}
      onStartRename={() => { newTitle = currentSessionEntry?.name || ''; rename = true; }}
      onNewTitleChange={(value) => newTitle = value}
      onSaveTitle={() => void commands.saveTitle(newTitle)}
      onExport={() => void commands.exportCurrentSession()}
      onSelectModel={commands.changeModel} onAddModel={() => showAddModel = true}
      onManageModels={() => tab = 'settings'} onSelectReasoning={commands.changeReasoning}/>
    <PluginWidgets placement="header" busy={widgetActionBusy} onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
    <AppBanner error={error} notice={notice} undoAvailable={recentlyDeleted.length > 0} {busy} {running}
      onDismissError={clearError}
      onDismissNotice={() => { notice = ''; recentlyDeleted = []; }}
      onUndo={() => void commands.undoDelete(recentlyDeleted)}/>
    {#if !initialized}
      <div class="loading-screen"><LoaderCircle size={28} class="spin"/><span>正在载入 Kroma…</span></div>
    {:else if tab === 'settings' && data}
      <div class="page-scroll"><SettingsView data={data.profiles} onSave={commands.saveProfile}
        onConnect={commands.doConnect} onSetDefault={commands.setDefaultProfile}
        onAdd={() => showAddModel=true}/></div>
    {:else if tab === 'plugins' && data}
      <div class="page-scroll"><PluginsView plugins={data.plugins} onAction={commands.pluginAction}
        onInstall={commands.installPlugin} onOpenDesktopPanel={openDesktopPanel}/></div>
    {:else if activePluginTabSurface && data}
      <div class="plugin-tab-page">
        <PluginDesktopFrame
          surface={activePluginTabSurface}
          sessionId={snapshot?.session_id ?? ''}
          project={data.project}
          fullHeight={true}
          onRunTask={(text) => { tab = 'chat'; void executeTask(text, [], true); }}
          onSetComposer={(text, mode) => composer?.setDraftText(text, mode)}
          onNotify={(msg, level) => level === 'error' ? (error = msg) : toast(msg)}
          onError={showError}/>
      </div>
    {/if}
    <!-- The chat pane stays mounted while another tab is active so the draft,
         attachments and loaded history survive a tab switch, exactly as they
         did when this state lived in the shell. -->
    <div class="chat-screen" hidden={!chatVisible}>
        {#if draggingFiles && !running}
          <div class="drop-overlay" role="status"><Paperclip size={22}/> 松开鼠标，将文件添加到任务中</div>
        {/if}
        {#if data?.mcp_project.configured && !data.mcp_project.trusted && !mcpDismissed}
          <div class="banner notice-banner" role="alert">
            <AlertCircle size={16}/>
            <span>此项目带有 <code>.kroma/mcp.json</code>，其中命令尚未获授权。批准前不会执行任何 MCP 进程。</span>
            <button class="primary small" onclick={commands.approveProjectMcp} disabled={busy || running}>批准并重连</button>
            <button title="稍后" onclick={() => mcpDismissed = true}><X size={15}/></button>
          </div>
        {/if}
        <div class="chat-body-row">
          <div class="chat-main-column">
            <ChatTranscript bind:this={chat} {data} {snapshot} {switchingTo} {previewSnapshot} {running} {busy}
              ready={!!data?.ready} {draftPrompt} {liveActivities} {activeElapsed} {taskSnapshotReceived}
              getData={() => data} getTaskGeneration={() => taskGeneration}
              onRewind={turns.askRewind} onFork={(index) => void turns.forkFromTurn(index)}
              onEdit={(index, text) => void turns.editFromTurn(index, text)}
              onUsePrompt={(text) => composer?.setPrompt(text)}
              onSetTab={(next) => tab = next} showError={showError}/>
            <!-- A transcript block belongs to the conversation itself, so it
                 sits at the end of the transcript instead of a fixed slot. -->
            <PluginWidgets placement="transcript" busy={widgetActionBusy}
              onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
            <PluginWidgets placement="above_composer" busy={widgetActionBusy}
              onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
            {#each composerDesktopSurfaces as surface (surface.plugin_id)}
              <div class="composer-desktop-slot">
                <PluginDesktopFrame
                  {surface}
                  sessionId={snapshot?.session_id ?? ''}
                  project={data?.project ?? ''}
                  onRunTask={(text) => void executeTask(text, [], true)}
                  onSetComposer={(text, mode) => composer?.setDraftText(text, mode)}
                  onNotify={(msg, level) => level === 'error' ? (error = msg) : toast(msg)}
                  onError={showError}/>
              </div>
            {/each}
            <ComposerPanel bind:this={composer}
              project={data?.project ?? ''} sessionId={snapshot?.session_id ?? ''}
              ready={!!data?.ready} {running} {busy} {compacting} {navigationBusy} {snapshot}
              {followups} {followupPaused} {followupExpanded} {taskResult}
              ask={pendingAsk} onAnswer={(answer) => void answerQuestion(answer)}
              onSend={sendTask} onSteer={steerTask} onBlocked={blockedSend}
              onQueueFollowup={queueFollowup}
              onCompact={turns.compactNow}
              onShowAbilities={() => abilitiesOpen = true}
              onCancelTask={() => void cancelTask()}
              onToggleFollowups={toggleFollowups}
              onPauseFollowups={pauseFollowups}
              onResumeFollowups={resumeFollowups}
              onRemoveFollowup={removeFollowup}
              onReorderFollowup={reorderFollowup}
              onClearFollowups={clearFollowups}
              {showError} {clearError} {toast}/>
            <PluginWidgets placement="below_composer" busy={widgetActionBusy}
              onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
            <PluginWidgets placement="footer" busy={widgetActionBusy}
              onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
          </div>
          {#if hasRightDock}
            <aside class="plugin-right-dock" aria-label="插件侧边区">
              <PluginWidgets placement="sidebar" busy={widgetActionBusy}
                onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
              {#each sidebarDesktopSurfaces as surface (surface.plugin_id)}
                <section class="plugin-widget-card plugin-dock-frame">
                  <header class="plugin-widget-head">
                    <strong>{surface.title}</strong>
                    <span>{surface.plugin_id}</span>
                  </header>
                  <PluginDesktopFrame
                    {surface}
                    sessionId={snapshot?.session_id ?? ''}
                    project={data?.project ?? ''}
                    onRunTask={(text) => void executeTask(text, [], true)}
                    onSetComposer={(text, mode) => composer?.setDraftText(text, mode)}
                    onNotify={(msg, level) => level === 'error' ? (error = msg) : toast(msg)}
                    onError={showError}/>
                </section>
              {/each}
            </aside>
          {/if}
        </div>
      </div>
  </main>
  {#if showAddModel}
    <AddModelDialog profiles={data?.profiles.profiles ?? []}
      onSaveConnect={commands.addAndConnectModel} onClose={() => showAddModel=false}/>
  {/if}
  {#if abilitiesOpen}
    <AbilitiesDialog toolNames={snapshot?.tool_names ?? []} skills={snapshot?.skills ?? []}
      onClose={() => abilitiesOpen = false}/>
  {/if}
  {#if rewindTarget}
    <RewindDialog files={rewindTarget.files} added={rewindTarget.added} removed={rewindTarget.removed}
      busy={rewinding} onCancel={() => rewindTarget = null}
      onConfirm={(restore) => void turns.confirmRewind(restore)}/>
  {/if}
  {#if $livePluginPanel}
    <div class="panel-overlay">
      <button class="panel-scrim" aria-label="关闭面板" onclick={() => livePluginPanel.set(null)}></button>
      <div class="panel-card" role="dialog" aria-modal="true" aria-label={$livePluginPanel.title}>
        <div class="panel-head">
          <h3>{$livePluginPanel.title}</h3>
          <button class="icon-button" aria-label="关闭面板" onclick={() => livePluginPanel.set(null)}>×</button>
        </div>
        {#if $livePluginPanel.blocks.length}
          <PanelBlocksView blocks={$livePluginPanel.blocks} actions={$livePluginPanel.actions}
            busy={widgetActionBusy}
            onAction={(actionId) => void handleWidgetAction($livePluginPanel!.plugin_id, 'panel', actionId)} />
        {:else}
          <pre class="panel-body">{$livePluginPanel.text}</pre>
        {/if}
      </div>
    </div>
  {/if}
  {#if openDesktopModal}
    <div class="panel-overlay">
      <button class="panel-scrim" aria-label="关闭自定义界面" onclick={() => openDesktopModal = null}></button>
      <div class="panel-card panel-card-wide" role="dialog" aria-modal="true" aria-label={openDesktopModal.title}>
        <div class="panel-head">
          <h3>{openDesktopModal.title}</h3>
          <button class="icon-button" aria-label="关闭自定义界面" onclick={() => openDesktopModal = null}>×</button>
        </div>
        <PluginDesktopFrame
          surface={openDesktopModal}
          sessionId={snapshot?.session_id ?? ''}
          project={data?.project ?? ''}
          fullHeight={true}
          onRunTask={(text) => { openDesktopModal = null; tab = 'chat'; void executeTask(text, [], true); }}
          onSetComposer={(text, mode) => composer?.setDraftText(text, mode)}
          onNotify={(msg, level) => level === 'error' ? (error = msg) : toast(msg)}
          onError={showError}/>
      </div>
    </div>
  {/if}
</div>

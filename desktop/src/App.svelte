<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { LoaderCircle, Paperclip, Puzzle, X } from '@lucide/svelte';
  import {
    api, type Bootstrap, type DesktopSurface, type PluginUiEvent,
    type SessionSnapshot, type DesktopEvent, type StreamEnvelope,
  } from './lib/api';
  import { modal } from './lib/modal';
  import { createLatestNavigation, sessionLabel, sidebarIsVisible } from './lib/chat-ui';
  import {
    applySnapshot, applySessionSwitch, createOperationGuard, sessionPreview,
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
  import { draftStorageKey, persistDraft, readStoredDraft } from './lib/drafts';
  import { announceTaskResult, ensureNotificationPermission } from './lib/task-notify';
  import { createSessionCommands } from './lib/session-commands';
  import { createTurnActions } from './lib/turn-actions';
  import type { DesktopHost, RewindTarget } from './lib/desktop-host';
  import type { SessionDestination, SessionTarget, Tab, TaskResultStatus } from './lib/ui-types';
  import type { LiveActivity } from './lib/transcript';
  import { foldLiveEvents } from './lib/live-events';
  import {
    applyPluginUiEvent, applyPresentation, applySnapshotPluginUi, desktopSurfaces, layoutStyle,
    livePluginPanel, loadPresentation, pluginColors, pluginLayout, pluginShortcuts, pluginThemeStyle,
    pluginWidgets, shortcutFor,
  } from './lib/plugin-ui';
  import {
    dismissAnswer, nextAsk, readAsk, type AskAnswer, type AskQuestion,
  } from './lib/ask-question';
  import SessionSidebar from './components/SessionSidebar.svelte';
  import Topbar from './components/Topbar.svelte';
  import ChatTranscript from './components/ChatTranscript.svelte';
  import ComposerPanel from './components/ComposerPanel.svelte';
  import AppBanner from './components/AppBanner.svelte';
  import UnifiedSettings from './components/UnifiedSettings.svelte';
  import CapabilitySettings from './components/CapabilitySettings.svelte';
  import PanelBlocksView from './components/PanelBlocksView.svelte';
  import PluginWidgets from './components/PluginWidgets.svelte';
  import PluginDesktopFrame from './components/PluginDesktopFrame.svelte';
  import AddModelDialog from './components/AddModelDialog.svelte';
  import AbilitiesDialog from './components/AbilitiesDialog.svelte';
  import RewindDialog from './components/RewindDialog.svelte';
  import KeyboardShortcutsDialog from './components/KeyboardShortcutsDialog.svelte';
  import CloseConfirmDialog from './components/CloseConfirmDialog.svelte';
  import BackupImportDialog from './components/BackupImportDialog.svelte';
  import BackupExportDialog from './components/BackupExportDialog.svelte';

  let data: Bootstrap | null = null;
  let tab: Tab = 'chat';
  let error = '';
  let notice = '';
  let busy = false;
  let running = false;
  let initialized = false;
  let sessionSettingsOpen = false;
  let settingsSection: 'models' | import('./lib/api').CapabilityKey = 'models';
  let rename = false;
  let newTitle = '';
  let sidebarChoice: boolean | null = null;
  let sidebarDefault: 'visible' | 'hidden' | null = null;
  let narrowLayout = false;
  let drawerWasOpen = false;
  $: syncNavigationFocus(narrowLayout && sidebarVisible);

  function syncNavigationFocus(open: boolean) {
    if (open === drawerWasOpen) return;
    drawerWasOpen = open;
    const restore = document.querySelector('.sidebar')?.contains(document.activeElement);
    void tick().then(() => {
      if (open && narrowLayout && sidebarVisible) sidebar?.focusNavigation();
      else if (!open && restore) document.querySelector<HTMLButtonElement>('.sidebar-toggle')?.focus({preventScroll: true});
    });
  }

  function setTab(next: Tab) {
    tab = next;
    if (narrowLayout) sidebarChoice = false;
  }
  $: sidebarVisible = sidebarIsVisible(narrowLayout, sidebarChoice, sidebarDefault);
  let chatWidth = 0;
  let dockOpen = false;
  $: dockOverlay = chatWidth < 840;
  let workspaceDetached = false;
  let draggingFiles = false;
  let showAddModel = false;
  let abilitiesOpen = false;
  let shortcutsOpen = false;
  let compacting = false;
  let rewinding = false;
  let recentlyDeleted: SessionTarget[] = [];
  let rewindTarget: RewindTarget | null = null;
  // Per-conversation task state. Navigation only changes which task the view
  // projects; a task that belongs to another conversation keeps receiving its
  // own stream and can finish without touching this window's visible state.
  interface SessionTask {
    key: string;
    project: string;
    session: string;
    runId: number;
    running: boolean;
    activities: LiveActivity[];
    sequence: number;
    modelStartIndex: number;
    draftPrompt: string;
    startedAt: number;
    result: TaskResultStatus;
    ask: AskQuestion | null;
    cancellationRequested: boolean;
    snapshotReceived: boolean;
    // The queue belongs to this conversation. A background completion advances
    // only its own queue and never the visible conversation's composer.
    followups: FollowupTask[];
    followupPaused: boolean;
    followupExpanded: boolean;
    followupScope: string;
    followupSequence: number;
    queueing: boolean;
    draining: boolean;
  }
  let sessionTasks: Record<string, SessionTask> = {};
  let taskRunSequence = 0;
  let clockNow = Date.now();
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
  let taskGeneration = 0;
  let viewKey = '';
  let switchingTo = '';
  let navigationBusy = false;
  let navigationProject = '';
  let previewSnapshot: SessionSnapshot | null = null;
  let lastSnapshotRef: SessionSnapshot | null = null;
  let widgetActionBusy = false;
  let openDesktopModal: DesktopSurface | null = null;
  // A cross-session search hit that must be located once its conversation is
  // really shown; the navigation generation keeps a late answer out.
  let locateTarget: {project: string; id: string; index: number} | null = null;
  let importBackupOpen = false;
  let backupOpen = false;
  // Closing the window during a task is a deliberate choice. The backend calls
  // window.destroy() itself once a close request is not prevented, so
  // `closeApproved` is the only thing that lets a close through while running.
  let closeConfirmOpen = false;
  let closeApproved = false;
  let closingWindow = false;
  const previewCache = new Map<string, SessionSnapshot>();
  let sidebar: SessionSidebar;
  let chat: ChatTranscript;
  let composer: ComposerPanel;
  // Token owner for operations that install a whole bootstrap. A late response
  // from a superseded operation must not overwrite the project or session the
  // user is looking at now.
  const bootstrapGuard = createOperationGuard();

  $: snapshot = switchingTo ? previewSnapshot : (data?.snapshot ?? null);
  // The conversation the window projects. A task key is project + session, so
  // the same id in two workspaces never shares live state.
  function taskKey(project: string, session: string | null | undefined): string {
    return `${project || ''}::${session || 'unknown'}`;
  }
  $: viewKey = data ? taskKey(data.project, snapshot?.session_id) : '';
  $: viewTask = viewKey ? sessionTasks[viewKey] : undefined;
  $: running = !!viewTask?.running;
  $: cancellationRequested = !!viewTask?.cancellationRequested;
  $: taskSnapshotReceived = !!viewTask?.snapshotReceived;
  $: liveActivities = viewTask?.activities ?? [];
  $: liveSequence = viewTask?.sequence ?? 0;
  $: modelStartIndex = viewTask?.modelStartIndex ?? 0;
  $: draftPrompt = viewTask?.draftPrompt ?? '';
  $: taskStartedAt = viewTask?.startedAt ?? 0;
  $: taskResult = viewTask?.result ?? '';
  $: pendingAsk = viewTask?.ask ?? null;
  $: followups = viewTask?.followups ?? [];
  $: followupPaused = viewTask?.followupPaused ?? true;
  $: followupExpanded = viewTask?.followupExpanded ?? false;
  $: followupScope = viewTask?.followupScope ?? '';
  $: activeElapsed = running && taskStartedAt
    ? Math.max(0, Math.floor((clockNow - taskStartedAt) / 1000))
    : 0;
  $: anyRunning = Object.values(sessionTasks).some(task => task.running);
  // A view generation only invalidates transcript lookups; it never gates a
  // background task's stream callback.
  function taskFor(project: string, session: string | null | undefined): SessionTask | undefined {
    return sessionTasks[taskKey(project, session)];
  }
  function updateTask(key: string, change: (task: SessionTask) => SessionTask) {
    const current = sessionTasks[key];
    if (!current) return;
    sessionTasks = {...sessionTasks, [key]: change(current)};
  }
  function renameTask(oldKey: string, nextProject: string, nextSession: string): string {
    const current = sessionTasks[oldKey];
    const nextKey = taskKey(nextProject, nextSession);
    if (!current || oldKey === nextKey) return nextKey;
    const next = {...sessionTasks};
    delete next[oldKey];
    next[nextKey] = {...current, key: nextKey, project: nextProject, session: nextSession};
    sessionTasks = next;
    return nextKey;
  }
  $: if (data?.snapshot !== lastSnapshotRef) {
    lastSnapshotRef = data?.snapshot ?? null;
    applySnapshotPluginUi(lastSnapshotRef);
  }
  // Passed to the sidebar as a plain reactive map: a task starting or finishing
  // in another conversation updates exactly its own row. The block names its
  // inputs directly so Svelte re-runs it on every task change.
  $: taskStatuses = buildTaskStatuses(sessionTasks, data);
  function buildTaskStatuses(
    tasks: Record<string, SessionTask>, bootstrap: Bootstrap | null,
  ): Record<string, string> {
    const statuses: Record<string, string> = {};
    for (const group of bootstrap?.session_groups ?? []) {
      for (const session of group.sessions) {
        const key = taskKey(group.project, session.id);
        const task = tasks[key];
        let status: string = task ? (task.running ? (task.ask ? 'waiting' : 'running') : '') : '';
        if (!status) {
          const state = (bootstrap?.session_states ?? []).find(entry =>
            entry.project === group.project && entry.session === session.id);
          if (state?.waiting) status = 'waiting';
          else if (state?.running || state?.busy) status = 'running';
        }
        if (status) statuses[key] = status;
      }
    }
    return statuses;
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
  // A plugin may own the whole main area. It never traps the person: the chat
  // stays one click away and keeps running underneath.
  $: workspaceSurface = $desktopSurfaces.find(s => s.slot === 'workspace') ?? null;
  $: workspaceActive = !!workspaceSurface && !workspaceDetached && tab === 'chat';
  $: appThemeStyle = [pluginThemeStyle($pluginColors), layoutStyle($pluginLayout)].filter(Boolean).join(';');
  $: compactDensity = $pluginLayout.density === 'compact';
  // A declared default sidebar state is applied once, so the person can still
  // open or close it afterwards without the plugin fighting them.
  let layoutDefaultsApplied = false;
  $: if (!layoutDefaultsApplied && $pluginLayout.sidebar) {
    layoutDefaultsApplied = true;
    sidebarDefault = $pluginLayout.sidebar;
  }
  $: modelName = data?.connected_model || '未连接模型';
  // The taskbar is the cheapest "still working" signal on a desktop.
  $: windowTitle = `${anyRunning ? '● ' : ''}${currentSessionName || 'Kroma'} — Kroma`;
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
  $: chatVisible = initialized && !workspaceActive && !(tab === 'settings' && data) && !tab.startsWith('plugin:');

  function showError(failure: unknown) { error = failure instanceof Error ? failure.message : String(failure); }
  function toast(message: string) { notice = message; }
  function sessionProject(): string { return data?.project || ''; }
  function sessionId(): string | undefined { return data?.snapshot?.session_id; }
  function saveDraft() { composer?.saveDraft(sessionProject(), sessionId()); }
  function restoreDraft() { composer?.restoreDraft(sessionProject(), sessionId()); }
  /** Keep a background task's rejected input in its OWN conversation draft.
   * An existing draft is never overwritten, and the visible composer is not
   * touched. */
  function saveOwnerDraft(project: string, session: string, text: string, paths: string[]) {
    const stored = appDrafts.recall(project, session)
      ?? readStoredDraft(browserDraftStorage(), draftStorageKey(project, session || 'new'));
    if (stored.text.trim() || stored.paths.length) return;
    appDrafts.remember(project, session, {text, paths: [...paths]});
    persistDraft(browserDraftStorage(), draftStorageKey(project, session || 'new'), {text, paths});
  }

  const host: DesktopHost = {
    get data() { return data; },
    set data(value) { data = value; },
    get snapshot() { return snapshot; },
    get tab() { return tab; },
    set tab(value) { if (value === 'settings') openModelSettings(); else tab = value; },
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
    guard: bootstrapGuard,
    clearError() { error = ''; },
    clearNotice() { notice = ''; },
    setRunning(value) {
      if (!viewKey) return;
      updateTask(viewKey, task => ({...task, running: value}));
    },
    resetTaskResult() {
      if (!viewKey) return;
      updateTask(viewKey, task => ({...task, result: ''}));
    },
    clearLiveActivities() {
      if (!viewKey) return;
      updateTask(viewKey, task => ({
        ...task, activities: [], sequence: 0, modelStartIndex: 0, draftPrompt: ''
      }));
    },
    saveDraft,
    resetHistoryWindow() { chat?.resetWindow(); },
    collapseSidebarOnNarrow() { if (narrowLayout) sidebarChoice = false; },
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
      if (!navigation.hasPending) { locateTarget = null; showError(failure); }
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
    if (!data || compacting || (busy && !navigationBusy)) return;
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
    }

    // The latest click is visible immediately, but only the one active Agent
    // may change actual sessions. Intermediate requests are coalesced.
    navigationProject = project;
    sidebar?.expandProject(project);
    switchingTo = id;
    previewSnapshot = previewCache.get(previewKey(project, id)) ?? null;
    tab = 'chat';
    if (narrowLayout) sidebarChoice = false;
    void chat?.scrollDown();
    navigation.request({project, id});
  }

  /** Open a cross-session search hit. The conversation is opened normally;
   * the target message is located only after navigation installs its
   * snapshot, or immediately when it is already the visible conversation. */
  function openSessionMessage(project: string, id: string, index: number) {
    if (!data || busy) return;
    if (project === data.project && id === data.snapshot?.session_id) {
      tab = 'chat';
      void chat?.revealMessage(index);
      return;
    }
    locateTarget = {project, id, index};
    openGroupSession(project, id);
  }

  async function performNavigation({project, id}: SessionDestination) {
    if (!data) return;
    try {
      if (project !== data.project) {
        data = await api.openProject(project);
        chat?.clearReview();
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
          openModelSettings();
          throw new Error('请先连接模型，然后再打开该工作区的会话');
        }
      }
      if (navigation.hasPending) return;
      if (data.snapshot?.session_id !== id) {
        const result = await api.switchSession(project, id);
        livePluginPanel.set(null);
        openDesktopModal = null;
        data = {
          ...applySessionSwitch(data, result.snapshot, result.title),
          connected_model: result.connected_model ?? data.connected_model,
          reasoning_level: result.reasoning_level ?? data.reasoning_level,
          thinking_levels: result.thinking_levels ?? data.thinking_levels,
        };
        rememberPreview(project, result.snapshot);
        applyPresentation(null);
        void loadPresentation();
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
    // A cross-session search hit is located only after the real snapshot is
    // installed, so the transcript window belongs to the requested session.
    const target = locateTarget;
    locateTarget = null;
    if (target && data?.project === target.project && data?.snapshot?.session_id === target.id) {
      void chat?.revealMessage(target.index);
    }
    // The preview suppressed the checkpoint lookup; the real snapshot is
    // visible now, so load the turns that belong to it.
    void chat?.refreshCheckpoints();
    if (data?.ready) tab = 'chat';
    void chat?.scrollDown();
    if (tab === 'chat') composer?.focus();
    void chat?.refreshReview();
  }

  async function newChatInGroup(project: string) {
    if (busy) return;
    if (project !== data?.project) {
      await commands.chooseProject(project);
      if (data?.project !== project || !data?.ready) return;
    }
    await commands.newSession();
  }

  onMount(() => {
    void ensureNotificationPermission();
    const resize = () => { narrowLayout = window.innerWidth < 880; };
    resize();
    window.addEventListener('resize', resize);
    // A message may still be queued for task steering while the Agent runs.
    const saveOnExit = () => { if (data) saveDraft(); };
    window.addEventListener('beforeunload', saveOnExit);
    const interval = window.setInterval(() => {
      // One clock drives every task; the view derives its own elapsed time.
      clockNow = Date.now();
    }, 1000);
    void commands.initialize();
    let stop: (() => void) | undefined;
    let stopPluginUi: (() => void) | undefined;
    let mounted = true;
    void listen<{project: string; session: string; event: PluginUiEvent}>('plugin_ui', event => {
      if (!mounted || !event.payload?.event) return;
      const envelope = event.payload;
      // A background conversation's plugin event must never rewrite the visible
      // composer, theme or widgets; switching back restores it from its
      // snapshot/presentation instead.
      if (envelope.project !== data?.project
        || envelope.session !== (data?.snapshot?.session_id ?? 'unknown')) return;
      if (data?.snapshot?.capabilities?.plugins === false) return;
      applyPluginUiEvent(envelope.event, {
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
    // The title bar X and Alt+F4 both arrive as a close request.
    let stopClose: (() => void) | undefined;
    void getCurrentWindow().onCloseRequested(event => {
      if (closeApproved || !anyRunning) return;
      event.preventDefault();
      closeConfirmOpen = true;
    }).then(fn => { if (mounted) stopClose = fn; else fn(); }).catch(failure => {
      // Never pretend the window is protected when the listener is missing.
      if (mounted) showError(`关窗确认暂不可用：${failure instanceof Error ? failure.message : String(failure)}`);
    });
    return () => {
      mounted = false; stop?.(); stopPluginUi?.(); stopClose?.();
      saveOnExit();
      composer?.cancelDraftTimer();
      window.removeEventListener('beforeunload', saveOnExit);
      window.removeEventListener('resize',resize);
      window.clearInterval(interval);
      composer?.releasePreviews();
    };
  });

  /** True when the window currently projects this exact conversation. */
  function isViewing(project: string, session: string | null | undefined): boolean {
    return !!data && data.project === project
      && (data.snapshot?.session_id ?? 'unknown') === (session || 'unknown');
  }

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
      restoreFollowups();
    }
  }

  // Fold every native event batch into the task that produced it. The visible
  // conversation only reads its own task; another session's batch never touches
  // this transcript.
  function applyEvents(key: string, events: DesktopEvent[]) {
    const task = sessionTasks[key];
    if (!task) return;
    const folded = foldLiveEvents({
      activities: task.activities, sequence: task.sequence, modelStartIndex: task.modelStartIndex
    }, events);
    sessionTasks = {...sessionTasks, [key]: {
      ...task,
      activities: folded.state.activities,
      sequence: folded.state.sequence,
      modelStartIndex: folded.state.modelStartIndex,
    }};
    for (const warning of folded.warnings) toast(warning);
    // Dropped live events leave a gap that only the durable history can fill.
    if (folded.resync && key === viewKey) void commands.refresh();
    if (key === viewKey) void chat?.scrollIfBottom();
  }

  // One question at a time per conversation: a newer one supersedes that
  // conversation's open bar, and the superseded question is answered
  // `dismissed` so no plugin waits forever.
  function receiveAsk(key: string, raw: unknown) {
    const incoming = readAsk(raw);
    const task = sessionTasks[key];
    if (!incoming || !task) return;
    const {pending, dismiss} = nextAsk(task.ask, incoming);
    sessionTasks = {...sessionTasks, [key]: {...task, ask: pending}};
    if (dismiss !== null) {
      void api.answerQuestion(dismissAnswer(dismiss), task.project, task.session).catch(showError);
    }
  }

  // Closing the bar is never approval: only a real pick or a typed answer
  // leaves `dismissed` false. The bar disappears first, so the composer is
  // usable again even if the host rejects an answer for a finished question.
  async function answerQuestion(answer: AskAnswer) {
    const task = viewTask;
    if (task?.ask?.id === answer.id) updateTask(task.key, current => ({...current, ask: null}));
    try {
      await api.answerQuestion(answer, task?.project ?? sessionProject(), task?.session ?? sessionId() ?? 'unknown');
    } catch (failure) { showError(failure); }
    // The composer is back in its slot; the person keeps typing where they left off.
    void tick().then(() => composer?.focus());
  }

  function onStream(event: StreamEnvelope, key: string, runId: number) {
    const task = sessionTasks[key];
    if (!task || task.runId !== runId) return;
    const isView = key === viewKey;
    if (event.kind === 'batch') applyEvents(key, event.events);
    if (event.kind === 'ask' && (!event.session || event.session === task.session)) {
      receiveAsk(key, event.ask);
    }
    if (event.kind === 'snapshot') {
      updateTask(key, current => ({...current, snapshotReceived: true}));
      if (isView && data) {
        installTaskSnapshot(event.snapshot, event.sessions, event.session_groups);
        void chat?.scrollIfBottom();
      } else {
        // A background conversation only updates its own cache and the shared
        // navigation metadata; it never installs the visible snapshot.
        rememberPreview(task.project, event.snapshot);
        if (data && data.project === task.project) {
          data = {...data,
            sessions: event.sessions ?? data.sessions,
            session_groups: event.session_groups ?? data.session_groups};
        }
      }
    }
    if (event.kind === 'done') {
      updateTask(key, current => ({...current, result: event.result.status as TaskResultStatus}));
      if (isView) {
        if (event.result.error) error = event.result.error;
        else if (event.result.status === 'cancelled') toast('任务已停止；此前执行的操作不会自动撤销');
      }
    }
  }

  async function executeTask(
    text: string,
    paths: string[],
    fromQueue: boolean,
    target?: {project: string; session: string},
  ): Promise<boolean> {
    // A queued task names its own conversation; a manual send targets the view.
    const taskProject = target?.project ?? sessionProject();
    const taskSession = target?.session ?? (sessionId() || 'unknown');
    const taskIsVisible = () => isViewing(taskProject, sessionTasks[firstKey]?.session ?? taskSession);
    if ((!fromQueue && busy) || (!fromQueue && (compacting || !data?.ready)) || (!text.trim() && !paths.length)) return false;
    const firstKey = taskKey(taskProject, taskSession);
    // One task per conversation; other conversations may keep running.
    if (sessionTasks[firstKey]?.running) return false;
    const runId = ++taskRunSequence;
    if (taskIsVisible()) taskGeneration += 1;
    if (!fromQueue && taskIsVisible()) composer?.clearForTask(taskProject, taskSession);
    const now = Date.now();
    const previous = sessionTasks[firstKey];
    const task: SessionTask = {
      key: firstKey, project: taskProject, session: taskSession, runId,
      running: true, activities: [], sequence: 0, modelStartIndex: 0,
      draftPrompt: text || ('已添加 ' + paths.length + ' 个文件'),
      startedAt: now, result: '', ask: null, cancellationRequested: false,
      snapshotReceived: false,
      followups: previous?.followups ?? [],
      followupPaused: previous?.followupPaused ?? true,
      followupExpanded: previous?.followupExpanded ?? false,
      followupScope: previous?.followupScope ?? sessionFollowupKey(taskProject, taskSession),
      followupSequence: previous?.followupSequence ?? 0,
      queueing: false,
      draining: previous?.draining ?? false,
    };
    sessionTasks = {...sessionTasks, [firstKey]: task};
    clockNow = now;
    if (taskIsVisible()) error = '';
    let key = firstKey;
    let sawSnapshot = false;
    let appliedSnapshot = false;
    let completed = false;
    if (taskIsVisible()) await chat?.scrollDown();
    try {
      const result = await api.send(text, paths, {project:taskProject, session:taskSession}, event => {
        if (event.kind === 'session') {
          // The backend allocated the real id before the first turn. Move this
          // task, and the visible address, without creating a second task.
          const previousSession = sessionTasks[key]?.session;
          key = renameTask(key, event.project, event.session);
          if (data?.snapshot && isViewing(event.project, sessionId() || 'unknown')) {
            data = {...data, snapshot: {...data.snapshot, session_id: event.session}};
            migrateTaskQueue(key, previousSession, event.session);
          }
          return;
        }
        if (event.kind === 'snapshot') {
          if (appliedSnapshot) return;
          sawSnapshot = true;
          appliedSnapshot = true;
        }
        onStream(event, key, runId);
      });
      composer?.releasePreviews(paths);
      key = renameTask(key, result.project || taskProject, result.session || taskSession);
      const isView = isViewing(result.project || taskProject, result.session || taskSession);
      if (result.result.error) {
        if (isView) error = result.result.error;
        else toast(`另一会话的任务结束：${result.result.error}`);
      }
      if (!sawSnapshot) {
        if (isView && data) {
          installTaskSnapshot(result.snapshot, result.sessions, result.session_groups);
          appliedSnapshot = true;
        } else {
          rememberPreview(result.project || taskProject, result.snapshot);
          if (data && data.project === (result.project || taskProject)) {
            data = {...data,
              sessions: result.sessions ?? data.sessions,
              session_groups: result.session_groups ?? data.session_groups};
          }
        }
      }
      updateTask(key, current => ({...current, result: result.result.status as TaskResultStatus}));
      // The backend rejected this input before writing any user turn: an
      // attachment could not be imported. Only this named signal restores the
      // draft, because a model, network or IPC failure may already be saved and
      // must never look unsubmitted. Another conversation's completion never
      // touches this composer.
      if (result.result.input_rejected && isView && composer?.isEmpty()) {
        composer?.restoreInput(text, paths, result.project || taskProject, result.session || taskSession);
        toast('这次的输入没有提交，已放回输入框');
      } else if (result.result.input_rejected) {
        // The owner is not visible: preserve its exact input in its own draft
        // instead of dropping the attachments or touching another composer.
        saveOwnerDraft(result.project || taskProject, result.session || taskSession, text, paths);
      }
      // A slash command replies with text instead of a model answer; the user
      // must see it, or the command would look like it did nothing.
      if (result.result.text && isView) toast(result.result.text);
      // Start from a fresh cache so this conversation gets its own snapshot.
      if (isView && data) {
        data = {...data,
          sessions: result.sessions ?? data.sessions,
          session_groups: result.session_groups ?? data.session_groups};
        void loadPresentation();
      }
      completed = result.result.status === 'completed';
      if (isView) {
        void announceTaskResult({
          status: result.result.status as TaskResultStatus, sessionName: currentSessionName,
          messages: result.snapshot.messages,
        });
        if (result.result.status === 'cancelled') {
          toast('任务已停止；后续任务已暂停，之前执行的操作不会自动撤销');
        }
      }
    } catch (failure) {
      // Ambiguous IPC failures are never automatically retried. Keep the
      // instruction available for a deliberate retry without overwriting
      // anything the user typed while the task was in flight.
      if (isViewing(taskProject, sessionTasks[key]?.session ?? taskSession)) {
        showError(failure);
        updateTask(key, current => ({...current, result: 'failed'}));
        if (!sawSnapshot && !appliedSnapshot && composer?.isEmpty()) {
          composer?.restoreInput(text, paths, taskProject, sessionTasks[key]?.session ?? taskSession);
        }
      }
    } finally {
      const current = sessionTasks[key];
      if (current?.runId === runId) {
        updateTask(key, t => ({
          ...t, running: false, cancellationRequested: false,
          draftPrompt: '', activities: [], ask: null,
          followupPaused: completed ? t.followupPaused : true,
        }));
      }
      // A failed or cancelled turn pauses only its own queue, stored under its
      // own project + session scope. A completed turn can advance that queue
      // even while a different conversation is visible.
      if (!completed) {
        updateTask(key, task => ({
          ...task, followupPaused: true, followupExpanded: task.followups.length > 0,
        }));
        persistTaskQueue(key);
      }
      const isView = isViewing(current?.project ?? taskProject, current?.session ?? taskSession);
      if (isView) {
        if (!sawSnapshot && !appliedSnapshot) {
          try { await commands.refresh(); } catch (failure) { showError(failure); }
        }
        await chat?.scrollIfBottom();
        void chat?.refreshReview();
      }
      const after = sessionTasks[key];
      if (after && shouldStartNextFollowup(
        completed ? 'completed' : 'failed', after.followupPaused, after.followups.length,
      )) {
        void runNextFollowup(key);
      }
    }
    return true;
  }

  function sendTask(text: string, paths: string[]) {
    void executeTask(text, paths, false);
  }
  function blockedSend() {
    if (!data?.ready) openModelSettings();
  }
  async function steerTask(instruction: string): Promise<SteerOutcome | null> {
    const task = viewTask;
    const project = task?.project ?? sessionProject();
    const session = task?.session ?? sessionId() ?? 'unknown';
    try {
      const outcome = await api.steer(instruction, project, session);
      toast(steerNotice(outcome));
      return outcome ?? {};
    } catch (failure) {
      showError(failure); // Keep the text intact for a deliberate retry.
      return null;
    }
  }

  async function queueFollowup(text: string, paths: string[]): Promise<boolean> {
    if (!viewTask?.running || (!text.trim() && !paths.length)) return false;
    const project = sessionProject();
    const session = sessionId() || 'unknown';
    restoreFollowups();
    const key = taskKey(project, session);
    const task = sessionTasks[key];
    if (!task || task.queueing) return false;
    const runId = task.runId;
    updateTask(key, current => ({...current, queueing: true}));
    try {
      // Copy the chosen files now: the source may change while the current
      // task still runs, and the queue must replay exactly what was queued.
      let stored: string[] = [];
      if (paths.length) {
        try {
          const prepared = await api.prepareFollowupAttachments(paths);
          stored = prepared.items.map(item => item.path);
        } catch (failure) {
          if (isViewing(project, session)) showError(failure);
          return false;
        }
        if (!isViewing(project, session) || sessionTasks[key]?.runId !== runId) {
          toast('会话已切换：这条后续任务未入队，输入仍保留在草稿中');
          return false;
        }
      }
      const current = sessionTasks[key];
      if (!current) return false;
      const queued = enqueueFollowup(
        current.followups, text, `followup-${Date.now()}-${current.followupSequence + 1}`, stored
      );
      if (!queued) {
        toast(`最多排队 ${MAX_FOLLOWUPS} 项，每项不超过 12000 字`);
        return false;
      }
      const firstPending = current.followups.length === 0;
      const stillRunning = current.running && !current.cancellationRequested && isViewing(project, session);
      // A queue created after its own task already ended stays paused: only a
      // deliberate resume starts it. A running owner unpauses its first item.
      updateTask(key, t => ({
        ...t,
        followups: queued,
        followupSequence: current.followupSequence + 1,
        followupExpanded: true,
        followupPaused: firstPending && stillRunning ? false : (stillRunning ? t.followupPaused : true),
        queueing: false,
      }));
      persistTaskQueue(key);
      toast(!stillRunning
        ? '已加入后续任务；当前任务已结束，队列保持暂停，请手动继续'
        : '已加入后续任务队列');
      return true;
    } finally {
      updateTask(key, current => ({...current, queueing: false}));
    }
  }

  /** Persist one conversation's queue under its own project + session scope. */
  function persistTaskQueue(key: string) {
    const task = sessionTasks[key];
    if (!task?.followupScope) return;
    saveFollowups(browserDraftStorage(), task.followupScope, task.followups);
  }

  /** A condition checked after every await: the owner is idle, unpaused, and
   * still has queued work. It deliberately does not require being the visible
   * conversation; a background queue advances on its own completion. */
  function followupExecutionAllowed(key: string): boolean {
    const task = sessionTasks[key];
    return !!task && !task.running && !task.followupPaused
      && task.followups.length > 0;
  }

  async function runNextFollowup(key: string) {
    const entry = sessionTasks[key];
    if (!entry || entry.draining || !followupExecutionAllowed(key)) return;
    updateTask(key, task => ({...task, draining: true}));
    try {
      while (followupExecutionAllowed(key)) {
        const owner = sessionTasks[key];
        if (!owner) return;
        const ownerRunId = owner.runId;
        const head = owner.followups[0];
        const headId = head.id;
        // The queued attachments are private copies, but a user may have
        // cleared the store. Never send an incomplete task: stop and say why.
        if (head.paths.length) {
          let check;
          try {
            check = await api.inspectAttachments(head.paths);
          } catch (failure) {
            if (followupExecutionAllowed(key) && sessionTasks[key]?.followups[0]?.id === headId
              && sessionTasks[key]?.runId === ownerRunId) {
              updateTask(key, task => ({...task, followupPaused: true}));
              persistTaskQueue(key);
              if (isViewing(owner.project, owner.session)) showError(failure);
            }
            return;
          }
          // A pause, removal, reorder or new task during the check expires
          // this attempt without removing or sending anything.
          if (!followupExecutionAllowed(key) || sessionTasks[key]?.followups[0]?.id !== headId
            || sessionTasks[key]?.runId !== ownerRunId) return;
          const broken = check.entries.filter(entry => entry.state !== 'ok');
          if (broken.length) {
            updateTask(key, task => ({...task, followupPaused: true}));
            persistTaskQueue(key);
            if (isViewing(owner.project, owner.session)) {
              toast(`队列第 1 项的附件已失效（${broken.length} 个），未发送；请移除该项或重新添加附件`);
            }
            return;
          }
        }
        if (!followupExecutionAllowed(key) || sessionTasks[key]?.followups[0]?.id !== headId
          || sessionTasks[key]?.runId !== ownerRunId) return;
        // Remove before sending. A crash/timeout must never silently rerun a
        // potentially destructive task. A refused submission restores the head
        // and pauses instead of losing it.
        updateTask(key, task => ({...task, followups: task.followups.slice(1)}));
        persistTaskQueue(key);
        // The task names its own owner, so a background queue can run while a
        // different conversation is visible.
        const accepted = await executeTask(
          head.text, head.paths, true, {project: owner.project, session: owner.session},
        );
        if (!accepted) {
          updateTask(key, task => ({
            ...task, followups: [head, ...task.followups], followupPaused: true,
          }));
          persistTaskQueue(key);
          return;
        }
      }
    } finally {
      updateTask(key, task => ({...task, draining: false}));
    }
  }

  async function cancelTask() {
    // Stopping the current conversation's task must never start its queued
    // successors, even if the cancellation races a normally completed model
    // response. Other conversations keep running.
    const task = viewTask;
    const project = task?.project ?? sessionProject();
    const session = task?.session ?? sessionId() ?? 'unknown';
    if (task) {
      updateTask(task.key, current => ({
        ...current, cancellationRequested: true, followupPaused: true,
      }));
      persistTaskQueue(task.key);
    }
    try { await api.cancel(project, session); } catch (failure) { showError(failure); }
  }

  /** Closing during tasks stops every one of them, keeps drafts, and never
   * waits for a long tool: the requests go out and the window closes at once.
   * Completed side effects are not claimed as undone. */
  async function closeWindowAndStopTask() {
    if (closingWindow) return;
    closingWindow = true;
    saveDraft();
    // Stop every conversation this window started, and pause every queue
    // (including background ones) so a completion cannot start an unseen task.
    for (const task of Object.values(sessionTasks)) {
      if (!task.running && !task.followups.length) continue;
      updateTask(task.key, current => ({
        ...current,
        cancellationRequested: current.running ? true : current.cancellationRequested,
        followupPaused: true,
      }));
      persistTaskQueue(task.key);
      if (task.running) void api.cancel(task.project, task.session).catch(() => {});
    }
    closeApproved = true;
    try {
      await getCurrentWindow().close();
    } catch (failure) {
      closeApproved = false;
      showError(failure);
    } finally {
      closingWindow = false;
    }
  }

  /** Follow-ups belong to the actual project + conversation. Each task object
   * holds its own queue, so a background completion can advance it without
   * touching the visible conversation's composer or draft. */
  function loadQueueIntoTask(project: string, session: string, create: boolean): SessionTask | undefined {
    const key = taskKey(project, session);
    const scope = sessionFollowupKey(project, session);
    const existing = sessionTasks[key];
    if (existing) {
      if (existing.followupScope === scope) return existing;
      const saved = queuedForSession(browserDraftStorage(), project, session);
      const merged = existing.followups.length ? mergeMigrated(saved, existing.followups) : saved;
      const next = {...existing, followupScope: scope, followups: merged,
        followupPaused: true, followupExpanded: merged.length > 0};
      sessionTasks = {...sessionTasks, [key]: next};
      return next;
    }
    const saved = queuedForSession(browserDraftStorage(), project, session);
    if (!create && !saved.length) return undefined;
    const task: SessionTask = {
      key, project, session, runId: 0, running: false,
      activities: [], sequence: 0, modelStartIndex: 0, draftPrompt: '',
      startedAt: 0, result: '', ask: null, cancellationRequested: false,
      snapshotReceived: false, followups: saved, followupPaused: true,
      followupExpanded: saved.length > 0, followupScope: scope,
      followupSequence: 0, queueing: false, draining: false,
    };
    sessionTasks = {...sessionTasks, [key]: task};
    return task;
  }

  function restoreFollowups() {
    if (!data) return;
    const session = data.snapshot?.session_id;
    // 'unknown' is shared by every unsent conversation; never restore a queue
    // under that placeholder into a different session.
    if (!hasPersistedSessionId(session)) return;
    loadQueueIntoTask(data.project, session!, true);
  }

  function migrateTaskQueue(key: string, previousSession: string | undefined, nextSession: string) {
    const task = sessionTasks[key];
    if (!task || !nextSession) return;
    const from = sessionFollowupKey(task.project, previousSession || 'unknown');
    const to = sessionFollowupKey(task.project, nextSession);
    if (task.followupScope !== from && task.followupScope !== to) return;
    const saved = loadFollowups(browserDraftStorage(), to);
    // A pre-existing queue stored under the new id survives ahead of the live one.
    const merged = mergeMigrated(saved, task.followups);
    saveFollowups(browserDraftStorage(), from, []);
    updateTask(key, current => ({...current, followupScope: to, followups: merged}));
    persistTaskQueue(key);
  }

  function removeFollowup(id: string) {
    const task = viewTask;
    if (!task) return;
    updateTask(task.key, current => ({
      ...current, followups: current.followups.filter(item => item.id !== id),
    }));
    persistTaskQueue(task.key);
  }
  function reorderFollowup(id: string, direction: -1 | 1) {
    const task = viewTask;
    if (!task) return;
    updateTask(task.key, current => ({
      ...current, followups: moveFollowup(current.followups, id, direction),
    }));
    persistTaskQueue(task.key);
  }
  function clearFollowups() {
    const task = viewTask;
    if (!task) return;
    updateTask(task.key, current => ({
      ...current, followups: [], followupPaused: true,
    }));
    persistTaskQueue(task.key);
  }
  function resumeFollowups() {
    const task = viewTask;
    if (!task?.followups.length) return;
    updateTask(task.key, current => ({...current, followupPaused: false}));
    persistTaskQueue(task.key);
    if (!task.running) void runNextFollowup(task.key);
  }
  function pauseFollowups() {
    const task = viewTask;
    if (!task) return;
    updateTask(task.key, current => ({...current, followupPaused: true}));
    persistTaskQueue(task.key);
  }
  function toggleFollowups() {
    const task = viewTask;
    if (!task) return;
    updateTask(task.key, current => ({...current, followupExpanded: !current.followupExpanded}));
  }

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
  function openModelSettings() { settingsSection = 'models'; tab = 'settings'; }
  function setData(next: Bootstrap) { data = next; }
  async function reloadData() { data = await api.bootstrap(); }
  async function refreshCapabilities() {
    await reloadData();
    openDesktopModal = null;
    await loadPresentation();
    chat?.refreshReview();
  }
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
        if (running) void queueFollowup(reply.task, []);
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
    if (surface.slot === 'tab' || surface.slot === 'workspace') {
      tab = `plugin:${surface.plugin_id}`;
    } else {
      openDesktopModal = surface;
    }
  }

  function handleGlobalKey(event: KeyboardEvent) {
    if (event.defaultPrevented || event.isComposing) return;
    if (document.querySelector('dialog:modal')) {
      if ((event.ctrlKey || event.metaKey) && !event.altKey && ['k', 'n', 'b', 'o', ','].includes(event.key.toLowerCase())) {
        event.preventDefault();
      }
      return;
    }
    // F1 is a reading aid, so it never preempts a dialog that is already open
    // and never stacks a second copy of itself.
    if (event.key === 'F1' && !event.altKey && !event.ctrlKey && !event.metaKey) {
      event.preventDefault();
      shortcutsOpen = true;
      return;
    }
    if (event.key === 'Escape') {
      if (openDesktopModal) { openDesktopModal = null; event.preventDefault(); }
      else if ($livePluginPanel) { livePluginPanel.set(null); event.preventDefault(); }
      else if (showAddModel) { showAddModel = false; event.preventDefault(); }
      else if (sidebar?.closeDeleteDialog()) { event.preventDefault(); }
      else if (chat?.closeReview()) { event.preventDefault(); }
      else if (chat?.closeFind()) { event.preventDefault(); }
      else if (sidebar?.closeSearch()) { event.preventDefault(); }
      else if (rename) { rename = false; event.preventDefault(); }
      else if (dockOverlay && dockOpen) { dockOpen = false; event.preventDefault(); }
      else if (narrowLayout && sidebarVisible) { sidebarChoice = false; event.preventDefault(); }
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
    if (key === 'f') {
      // Find in the open conversation. It never calls the model or a tool.
      event.preventDefault();
      tab = 'chat';
      chat?.openFind();
    } else if (key === 'k') {
      event.preventDefault();
      tab = 'chat';
      sidebarChoice = true;
      sidebar?.openSearch();
    } else if (key === 'n' && data?.ready && !busy && !compacting) {
      event.preventDefault();
      void commands.newSession().then(() => composer?.focus());
    } else if (key === 'b') {
      event.preventDefault();
      sidebarChoice = !sidebarVisible;
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
<div class="app-shell" class:sidebar-visible={sidebarVisible} class:narrow-layout={narrowLayout} class:compact={compactDensity} style={appThemeStyle}>
  {#if narrowLayout && sidebarVisible}
    <button class="sidebar-backdrop" aria-label="关闭侧边栏" onclick={() => sidebarChoice = false}></button>
  {/if}
  <SessionSidebar bind:this={sidebar}
    {data} {tab} {selectedProject} {activeSessionId} {anyRunning} statuses={taskStatuses}
    drawer={narrowLayout && sidebarVisible} onCloseDrawer={() => sidebarChoice = false}
    {busy} {navigationBusy}
    guard={bootstrapGuard} getSnapshot={() => snapshot}
    onNewSession={() => void commands.newSession()}
    onChooseProject={(path) => void commands.chooseProject(path)}
    onOpenSession={openGroupSession}
    onOpenMessage={openSessionMessage}
    onNewChatInProject={(project) => void newChatInGroup(project)}
    onSetTab={setTab}
    setData={setData} reloadData={reloadData}
    afterSingleDelete={afterSingleDelete} afterBulkDelete={afterBulkDelete}
    setBusy={setBusy} clearError={clearError} showError={showError} toast={toast}
    onDeleted={(targets) => recentlyDeleted = targets}
    onShowShortcuts={() => shortcutsOpen = true}/>
  <main class="main-area" inert={narrowLayout && sidebarVisible}>
    <Topbar {tab} {currentSessionName} {hasSession} {rename} {newTitle} {sidebarVisible} {busy} {running}
      ready={!!data?.ready} profiles={data?.profiles.profiles ?? []} {connectedProfile} {modelName}
      {reasoningLevel} {reasoningLevels} thinkingEnabled={!!data?.reasoning_level}
      onToggleSidebar={() => sidebarChoice = !sidebarVisible}
      onStartRename={() => { newTitle = currentSessionEntry?.name || ''; rename = true; }}
      onNewTitleChange={(value) => newTitle = value}
      onSaveTitle={() => void commands.saveTitle(newTitle)}
      onCancelRename={() => rename = false} onWindowError={showError}
      onExport={() => void commands.exportCurrentSession()}
      onBackup={() => backupOpen = true}
      onImportBackup={() => importBackupOpen = true}
      onSelectModel={commands.changeModel} onAddModel={() => showAddModel = true}
      onManageModels={openModelSettings}
      onSessionSettings={() => sessionSettingsOpen = true} onSelectReasoning={commands.changeReasoning}/>
    <PluginWidgets placement="header" busy={widgetActionBusy} onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
    <AppBanner error={error} notice={notice} undoAvailable={recentlyDeleted.length > 0} {busy} {running}
      onDismissError={clearError}
      onDismissNotice={() => { notice = ''; recentlyDeleted = []; }}
      onUndo={() => void commands.undoDelete(recentlyDeleted)}/>
    {#if !initialized}
      <div class="loading-screen"><LoaderCircle size={28} class="spin"/><span>正在载入 Kroma…</span></div>
    {:else if tab === 'settings' && data}
      <div class="page-scroll"><UnifiedSettings {data} bind:section={settingsSection} disabled={busy || running}
        onSaving={setBusy} onSaved={refreshCapabilities} onSave={commands.saveProfile}
        onConnect={async (name) => { if (!await commands.doConnect(name)) throw new Error('连接未完成'); }}
        onSetDefault={async (name) => { if (!await commands.setDefaultProfile(name)) throw new Error('默认模型未更改'); }}
        onAdd={() => showAddModel=true} onOpenStorage={() => void sidebar?.openStorage()}
        onPluginAction={commands.pluginAction} onInstallPlugin={commands.installPlugin}
        onOpenDesktopPanel={openDesktopPanel}/></div>
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
    {#if workspaceActive && workspaceSurface && data}
      <div class="workspace-frame">
        <PluginDesktopFrame surface={workspaceSurface} fullHeight={true}
          sessionId={snapshot?.session_id ?? ''}
          project={data.project}
          onRunTask={(text) => { workspaceDetached = true; void executeTask(text, [], true); }}
          onSetComposer={(text, mode) => composer?.setDraftText(text, mode)}
          onNotify={(msg, level) => level === 'error' ? (error = msg) : toast(msg)}
          onError={showError}/>
        <button class="workspace-exit" onclick={() => workspaceDetached = true}>回到对话</button>
      </div>
    {/if}
    <!-- The chat pane stays mounted while another tab is active so the draft,
         attachments and loaded history survive a tab switch, exactly as they
         did when this state lived in the shell. -->
    <div class="chat-screen" hidden={!chatVisible}>
        {#if draggingFiles && !running}
          <div class="drop-overlay" role="status"><Paperclip size={22}/> 松开鼠标，将文件添加到任务中</div>
        {/if}
        {#if hasRightDock && dockOverlay}
          <div class="chat-tools">
            <button class="quiet-action" aria-expanded={dockOpen} aria-controls="plugin-right-dock"
              onclick={() => dockOpen = !dockOpen}><Puzzle size={14}/> 插件区</button>
          </div>
        {/if}
        <div class="chat-body-row" bind:clientWidth={chatWidth}>
          <div class="chat-main-column">
            <ChatTranscript bind:this={chat} {data} {snapshot} {switchingTo} {previewSnapshot} {running} {busy}
              ready={!!data?.ready} {draftPrompt} {liveActivities} {activeElapsed} {taskSnapshotReceived}
              getData={() => data} getTaskGeneration={() => taskGeneration}
              onRewind={turns.askRewind} onFork={(index) => void turns.forkFromTurn(index)}
              onEdit={(index, text) => void turns.editFromTurn(index, text)}
              onUsePrompt={(text) => composer?.setPrompt(text)}
              onSetTab={(next) => { if (next === 'settings') openModelSettings(); else tab = next; }} showError={showError}
              {widgetActionBusy} onWidgetAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
            <div class="composer-extensions">
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
            </div>
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
            <aside id="plugin-right-dock" class="plugin-right-dock" class:floating={dockOverlay}
              hidden={dockOverlay && !dockOpen} aria-label="插件侧边区">
              {#if dockOverlay}
                <div class="plugin-dock-heading"><span>插件区</span>
                  <button class="icon-button" aria-label="收起插件区" onclick={() => dockOpen = false}><X size={15}/></button>
                </div>
              {/if}
              <PluginWidgets placement="sidebar" busy={widgetActionBusy}
                onAction={(p, w, a) => void handleWidgetAction(p, w, a)}/>
              {#each sidebarDesktopSurfaces as surface (surface.plugin_id)}
                <section class="plugin-widget-card plugin-dock-frame">
                  <header class="plugin-widget-head">
                    <strong>{surface.title}</strong>
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
      onSaveConnect={commands.addAndConnectModel} onClose={() => {
        showAddModel = false;
        void tick().then(() => {
          if (tab === 'chat' && document.activeElement === document.body) composer?.focus();
        });
      }}/>
  {/if}
  {#if sessionSettingsOpen}
    <dialog class="panel-overlay" aria-label="会话设置" use:modal={{onClose: () => sessionSettingsOpen = false, busy}}>
      <button class="panel-scrim" tabindex="-1" aria-hidden="true" disabled={busy} onclick={() => sessionSettingsOpen = false}></button>
      <div class="dialog-panel session-settings-panel">
        <div class="panel-head"><h3>会话设置</h3><button class="icon-button" aria-label="关闭会话设置" disabled={busy} onclick={() => sessionSettingsOpen = false}><X size={16}/></button></div>
        <CapabilitySettings scope="session" disabled={busy || running} onSaving={setBusy} onSaved={refreshCapabilities}/>
      </div>
    </dialog>
  {/if}
  {#if abilitiesOpen}
    <AbilitiesDialog toolNames={snapshot?.tool_names ?? []} skills={snapshot?.skills ?? []}
      onClose={() => abilitiesOpen = false}/>
  {/if}
  {#if shortcutsOpen}
    <KeyboardShortcutsDialog onClose={() => shortcutsOpen = false}/>
  {/if}
  {#if rewindTarget}
    <RewindDialog files={rewindTarget.files} added={rewindTarget.added} removed={rewindTarget.removed}
      busy={rewinding} {error} onCancel={() => rewindTarget = null}
      onConfirm={(restore) => void turns.confirmRewind(restore)}/>
  {/if}
  {#if importBackupOpen}
    <BackupImportDialog busy={busy || running}
      onCancel={() => importBackupOpen = false}
      onConfirm={() => { importBackupOpen = false; void commands.importSessionBackup(); }}/>
  {/if}
  {#if backupOpen}
    <BackupExportDialog busy={busy || running}
      onCancel={() => backupOpen = false}
      onConfirm={() => { backupOpen = false; void commands.backupCurrentSession(); }}/>
  {/if}
  {#if closeConfirmOpen}
    <CloseConfirmDialog closing={closingWindow}
      onCancel={() => closeConfirmOpen = false}
      onConfirm={() => void closeWindowAndStopTask()}/>
  {/if}
  {#if $livePluginPanel}
    <dialog class="panel-overlay" aria-label={$livePluginPanel.title}
      use:modal={{ onClose: () => livePluginPanel.set(null) }}>
      <button class="panel-scrim" tabindex="-1" aria-hidden="true" onclick={() => livePluginPanel.set(null)}></button>
      <div class="panel-card">
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
    </dialog>
  {/if}
  {#if openDesktopModal}
    <dialog class="panel-overlay" aria-label={openDesktopModal.title}
      use:modal={{ onClose: () => openDesktopModal = null }}>
      <button class="panel-scrim" tabindex="-1" aria-hidden="true" onclick={() => openDesktopModal = null}></button>
      <div class="panel-card panel-card-wide">
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
    </dialog>
  {/if}
</div>

<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import {
    ArrowLeft, ChevronRight, FolderClosed, FolderPlus, LoaderCircle,
    Keyboard, MessageSquareText, Pin, PinOff, Plus, Puzzle, Search, Settings2, Trash2, X, ListChecks,
  } from '@lucide/svelte';
  import { api, type Bootstrap, type SessionSearchHit, type SessionSnapshot } from '../lib/api';
  import {
    isWorkspaceExpanded, sessionActivityLabel, sessionLabel,
    workspaceDisplayName, workspaceLabels,
  } from '../lib/chat-ui';
  import { browserDraftStorage } from '../lib/draft-cache';
  import {
    arrangePinnedSessions, loadPinnedIds, savePinnedIds, togglePinnedId,
  } from '../lib/pinned-sessions';
  import { countGroupSessions, filterSessionGroups, matchedContentIds } from '../lib/session-groups';
  import {
    extendSelection, parseSelectionKey, sessionSelectionKey, toggleSelection,
  } from '../lib/selection';
  import type { OperationGuard } from '../lib/snapshot';
  import type { SessionTarget, Tab } from '../lib/ui-types';
  import { desktopSurfaces } from '../lib/plugin-ui';
  import DeleteConfirmDialog from './DeleteConfirmDialog.svelte';
  import StorageDialog from './StorageDialog.svelte';

  export let data: Bootstrap | null = null;
  export let tab: Tab = 'chat';
  export let drawer = false;
  export let onCloseDrawer: () => void = () => {};
  let navigation: HTMLElement;

  export function focusNavigation() {
    const target = navigation?.querySelector<HTMLElement>('.session-search, .session-main[aria-current], .new-chat:not(:disabled)');
    (target ?? navigation)?.focus({preventScroll: true});
  }

  function trapDrawerTab(event: KeyboardEvent) {
    if (!drawer || event.key !== 'Tab' || event.isComposing || document.querySelector('dialog:modal')) return;
    const targets = [...navigation.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), [tabindex="0"]')]
      .filter(node => node.getClientRects().length);
    const first = targets[0], last = targets.at(-1);
    if (!first) { event.preventDefault(); navigation.focus(); return; }
    if (!navigation.contains(document.activeElement) || (event.shiftKey && document.activeElement === first)) {
      event.preventDefault(); (event.shiftKey ? last : first)?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault(); first.focus();
    }
  }
  export let selectedProject: string | undefined;
  export let activeSessionId: string | undefined;
  // Any task in any conversation. Navigation and starting a new conversation
  // stay available while a background task runs.
  export let anyRunning = false;
  // project::session -> 'running' | 'waiting' | 'busy', reactive from the shell.
  export let statuses: Record<string, string> = {};
  export let busy = false;
  export let navigationBusy = false;
  export let guard: OperationGuard;
  export let getSnapshot: () => SessionSnapshot | null;
  export let onNewSession: () => void;
  export let onChooseProject: (path?: string) => void;
  export let onOpenSession: (project: string, id: string) => void;
  export let onOpenMessage: (project: string, id: string, messageIndex: number) => void;
  export let onNewChatInProject: (project: string) => void;
  export let onSetTab: (tab: Tab) => void;
  export let setData: (next: Bootstrap) => void;
  export let reloadData: () => Promise<void>;
  export let afterSingleDelete: (target: SessionTarget, deletedCurrent: boolean) => Promise<void> | void;
  export let afterBulkDelete: () => Promise<void> | void;
  export let setBusy: (value: boolean) => void;
  export let clearError: () => void;
  export let showError: (failure: unknown) => void;
  export let toast: (message: string) => void;
  export let onDeleted: (targets: SessionTarget[]) => void;
  export let onShowShortcuts: () => void;

  let sessionSearch = '';
  let sessionSearchOpen = false;
  let sessionSearchHits: SessionSearchHit[] = [];
  let sessionSearchBusy = false;
  let sessionSearchEpoch = 0;
  let sessionSearchTimer: ReturnType<typeof setTimeout> | null = null;
  let sessionSearchInput: HTMLInputElement;
  let groupExpansion: Record<string, boolean> = {};
  let groupLimits: Record<string, number> = {};
  // Pinned conversations per workspace. This stays a rendered dependency so a
  // pin change immediately reorders the tree; loading it inside a helper that
  // only runs on render would silently keep the old order.
  let pinnedByProject: Record<string, string[]> = {};
  let pinnedProjectsLoaded = '';
  let selectedSessions: Set<string> = new Set();
  let selectionAnchor: {project: string; id: string} | null = null;
  let selectMode = false;
  let bulkDeleting = false;
  let pendingDelete: SessionTarget | null = null;
  let deletingSession = false;
  let deleteError = '';
  let bulkConfirm = false;
  let storageOpen = false;
  let storageRows: Array<{project: string; sessions: number; empty: number; bytes: number; empty_ids: string[]}> = [];
  let storageBusy = false;
  let storagePurging = false;
  let storageError = '';

  $: sessionGroups = data?.session_groups ?? [];
  function statusFor(source: Record<string, string>, project: string, id: string): string {
    return source[`${project}::${id}`] ?? '';
  }
  function groupRunningCount(source: Record<string, string>, project: string): number {
    const group = sessionGroups.find(entry => entry.project === project);
    if (!group) return 0;
    return group.sessions.filter(session => !!statusFor(source, project, session.id)).length;
  }
  // Two folders named "kroma" must not look identical in the tree.
  $: groupLabels = workspaceLabels(sessionGroups.map(group => group.project), data?.user_home);
  $: groupName = (project: string) => groupLabels[project] ?? workspaceDisplayName(project, data?.user_home);
  $: matchedIds = matchedContentIds(sessionSearchHits);
  $: filteredSessionGroups = filterSessionGroups(sessionGroups, sessionSearch, matchedIds);
  $: matchingSessionCount = countGroupSessions(filteredSessionGroups);
  $: scheduleSessionContentSearch(sessionSearchOpen ? sessionSearch.trim() : '');
  $: projectsKey = sessionGroups.map(group => group.project).join('\u0000');
  $: if (projectsKey !== pinnedProjectsLoaded) {
    pinnedProjectsLoaded = projectsKey;
    pinnedByProject = readPins(sessionGroups.map(group => group.project));
  }

  // A pending local search must never fire into an unmounted component.
  onDestroy(() => {
    sessionSearchEpoch += 1;
    if (sessionSearchTimer !== null) { clearTimeout(sessionSearchTimer); sessionSearchTimer = null; }
  });

  function scheduleSessionContentSearch(query: string) {
    const epoch = ++sessionSearchEpoch;
    if (sessionSearchTimer !== null) clearTimeout(sessionSearchTimer);
    sessionSearchTimer = null;
    sessionSearchHits = [];
    if (query.length < 2 || query.length > 120) {
      sessionSearchBusy = false;
      return;
    }
    sessionSearchBusy = true;
    sessionSearchTimer = setTimeout(async () => {
      sessionSearchTimer = null;
      try {
        const result = await api.searchSessions(query);
        if (epoch === sessionSearchEpoch) sessionSearchHits = result.hits;
      } catch (failure) {
        if (epoch === sessionSearchEpoch) showError(failure);
      } finally {
        if (epoch === sessionSearchEpoch) sessionSearchBusy = false;
      }
    }, 350);
  }

  export function openSearch() {
    sessionSearchOpen = true;
    void tick().then(() => sessionSearchInput?.focus());
  }

  export function closeSearch(): boolean {
    if (!sessionSearchOpen) return false;
    sessionSearchOpen = false;
    sessionSearch = '';
    return true;
  }

  /** Navigation opens the target workspace so the moved session is visible. */
  export function expandProject(project: string) {
    groupExpansion = {...groupExpansion, [project]: true};
  }

  export function closeDeleteDialog(): boolean {
    if (!pendingDelete || deletingSession) return false;
    pendingDelete = null;
    return true;
  }

  export function hasBlockingDialog(): boolean {
    return !!pendingDelete;
  }

  function toggleSessionSearch() {
    sessionSearchOpen = !sessionSearchOpen;
    if (!sessionSearchOpen) sessionSearch = '';
    else void tick().then(() => sessionSearchInput?.focus());
  }

  function toggleGroup(project: string) {
    const wasOpen = isWorkspaceExpanded(
      groupExpansion, project, selectedProject, data?.default_project, sessionSearch
    );
    groupExpansion = {...groupExpansion, [project]: !wasOpen};
  }

  function visibleGroupSessions(
    group: {sessions: Bootstrap['sessions']; project: string}, pinnedIds: readonly string[],
    limit: number,
  ) {
    // The fold limit is passed in from the template so "show more" is a real
    // rendered dependency instead of a value hidden inside this helper.
    return arrangePinnedSessions(
      group.sessions, pinnedIds, limit,
      group.project === selectedProject ? activeSessionId : undefined,
      sessionSearch.trim().length > 0
    );
  }

  /** Known workspaces are read once; an already loaded list is kept so a pin
   * that only lives in memory (storage refused the write) is not silently
   * replaced by older storage. */
  function readPins(projects: string[]): Record<string, string[]> {
    const store = browserDraftStorage();
    const next: Record<string, string[]> = {};
    for (const project of projects) {
      next[project] = pinnedByProject[project] ?? loadPinnedIds(store, project);
    }
    return next;
  }

  function togglePin(project: string, id: string) {
    const ids = togglePinnedId(pinnedByProject[project] ?? [], id);
    const persisted = savePinnedIds(browserDraftStorage(), project, ids);
    pinnedByProject = {...pinnedByProject, [project]: ids};
    if (!persisted) toast('置顶状态仅本次有效：本机存储不可用');
    else toast(ids.includes(id) ? '已置顶会话' : '已取消置顶');
  }

  function toggleSessionSelection(project: string, id: string) {
    selectedSessions = toggleSelection(selectedSessions, project, id);
    selectionAnchor = {project, id};
  }

  // Batch actions only appear after asking for them, so ordinary reading rows
  // never reserve room for a checkbox.
  function toggleSelectMode() {
    selectMode = !selectMode;
    if (!selectMode) { selectedSessions = new Set(); selectionAnchor = null; }
  }

  function onSessionClick(event: MouseEvent, project: string, id: string, messageIndex?: number) {
    if (selectMode) {
      event.preventDefault();
      toggleSessionSelection(project, id);
      return;
    }
    if (event.ctrlKey || event.metaKey) {
      event.preventDefault();
      toggleSessionSelection(project, id);
      return;
    }
    if (event.shiftKey && selectionAnchor && selectionAnchor.project === project) {
      event.preventDefault();
      const group = filteredSessionGroups.find(group => group.project === project);
      const ids = group ? visibleGroupSessions(group, pinnedByProject[project] ?? [], groupLimits[project] ?? 7).map(entry => entry.id) : [];
      selectedSessions = extendSelection(selectedSessions, ids, project, selectionAnchor.id, id);
      return;
    }
    // A content hit opens the conversation at the matched message.
    if (messageIndex !== undefined) onOpenMessage(project, id, messageIndex);
    else onOpenSession(project, id);
  }

  async function deleteSelectedSessions() {
    if (!selectedSessions.size || bulkDeleting) return;
    const targets = [...selectedSessions]
      .map(key => parseSelectionKey(key))
      .filter((target): target is SessionTarget => target !== null)
      .map(target => ({...target, title: ''}));
    bulkDeleting = true;
    clearError();
    let removed = 0;
    let failed = 0;
    let lastFailure = '';
    const removedTargets: SessionTarget[] = [];
    try {
      for (const target of targets) {
        const status = statusFor(statuses, target.project, target.id);
        if (status === 'running' || status === 'waiting' || status === 'busy') {
          failed += 1;
          lastFailure = '运行中或等待中的会话无法删除';
          continue;
        }
        try {
          await api.deleteSession(target.project, target.id);
          removed += 1;
          removedTargets.push({...target, title: ''});
        } catch (failure) {
          failed += 1;
          lastFailure = failure instanceof Error ? failure.message : String(failure);
        }
      }
      await reloadData();
      selectedSessions = new Set();
      selectionAnchor = null;
      selectMode = false;
      onDeleted(removedTargets);
      await afterBulkDelete();
      toast(failed
        ? `已删除 ${removed} 个会话，${failed} 个失败：${lastFailure}`
        : `已删除 ${removed} 个会话`);
    } catch (failure) {
      showError(failure);
    } finally {
      bulkDeleting = false;
      bulkConfirm = false;
    }
  }

  function askDelete(project: string, id: string, title: string) {
    deleteError = '';
    pendingDelete = {project, id, title};
  }

  async function confirmDelete() {
    const target = pendingDelete;
    if (!target || busy || deletingSession) return;
    deletingSession = true;
    deleteError = '';
    const generation = guard.begin();
    setBusy(true);
    clearError();
    try {
      const next = await guard.settleBootstrap(generation, api.deleteSession(target.project, target.id));
      if (!next) return;
      const shown = getSnapshot();
      const deletedCurrent = data?.project === target.project && shown?.session_id === target.id;
      setData(next);
      if (deletedCurrent) await afterSingleDelete(target, true);
      else await afterSingleDelete(target, false);
      pendingDelete = null;
      onDeleted([target]);
      toast('会话已移至本地回收目录');
    } catch (failure) {
      deleteError = failure instanceof Error ? failure.message : String(failure);
      showError(failure);
    } finally {
      deletingSession = false;
      if (guard.isCurrent(generation)) setBusy(false);
    }
  }

  /** Settings owns the low-frequency local-data report; this only opens it. */
  export async function openStorage() {
    storageOpen = true;
    storageRows = [];
    storageError = '';
    storageBusy = true;
    try { storageRows = await api.storageReport(); }
    catch (failure) {
      storageError = failure instanceof Error ? failure.message : String(failure);
      showError(failure);
    }
    finally { storageBusy = false; }
  }

  async function purgeEmptySessions() {
    const targets = storageRows.flatMap(row => row.empty_ids.map(id => ({project: row.project, id, title: ''})));
    if (!targets.length || storageBusy) return;
    storageBusy = true;
    storagePurging = true;
    storageError = '';
    let removed = 0;
    let failed = 0;
    try {
      const removedTargets: SessionTarget[] = [];
      for (const target of targets) {
        try {
          await api.deleteSession(target.project, target.id);
          removed += 1;
          removedTargets.push(target);
        } catch (failure) {
          failed += 1;
          storageError = failure instanceof Error ? failure.message : String(failure);
        }
      }
      onDeleted(removedTargets);
      await reloadData();
      storageRows = await api.storageReport();
      if (failed) storageError = `${failed} 个空会话未能清理：${storageError}`;
      toast(failed ? `已清理 ${removed} 个空会话，${failed} 个未能清理` : `已清理 ${removed} 个空会话`);
    } catch (failure) {
      storageError = failure instanceof Error ? failure.message : String(failure);
      showError(failure);
    }
    finally { storageBusy = false; storagePurging = false; }
  }
</script>

<svelte:window onkeydowncapture={trapDrawerTab}/>
<aside class="sidebar" aria-label="会话与主导航" bind:this={navigation} tabindex="-1"
  role={drawer ? 'dialog' : 'complementary'} aria-modal={drawer ? true : undefined}>
  <header class="sidebar-brand">
    <svg class="sidebar-brand-mark" width="24" height="24" viewBox="0 0 24 24"
      fill="currentColor" aria-hidden="true" focusable="false">
      <path d="M4 3h4v18H4zM10 12l7-9h5l-7 9 7 9h-5z"/>
    </svg>
    <span class="sidebar-brand-name">Kroma</span>
    {#if drawer}<button class="icon-button sidebar-close" aria-label="收起导航" onclick={onCloseDrawer}><X size={17}/></button>{/if}
  </header>
  <div class="sidebar-actions">
    <button class="new-chat" onclick={onNewSession} disabled={!data?.ready || busy}
      title={`在 ${data?.project ? workspaceDisplayName(data.project, data.user_home) : '当前工作区'} 中新建会话`}>
      <Plus size={16}/> 新建会话
    </button>
    <button class="sidebar-search-toggle" class:active={sessionSearchOpen}
      onclick={toggleSessionSearch} aria-label="搜索会话" aria-expanded={sessionSearchOpen}
      title="搜索会话 · Ctrl+K"><Search size={17}/></button>
    {#if data?.sessions.length}
      <button class="sidebar-search-toggle" class:active={selectMode}
        onclick={toggleSelectMode} aria-label="选择会话" aria-pressed={selectMode}
        title="批量选择会话"><ListChecks size={17}/></button>
    {/if}
  </div>
  <div class="sidebar-section conversation-library">
    {#if sessionSearchOpen}
      <div class="session-search-wrap"><Search size={14}/>
        <input class="session-search" bind:this={sessionSearchInput} bind:value={sessionSearch}
          aria-label="搜索会话" placeholder="搜索标题或历史消息"/>
        {#if sessionSearchBusy}<LoaderCircle size={13} class="spin search-progress"/>{/if}
        <button class="search-dismiss" title="收起搜索" aria-label="收起搜索"
          onclick={() => {sessionSearch='';sessionSearchOpen=false;}}><X size={14}/></button>
      </div>
      {#if sessionSearch.trim().length >= 2}
        <div class="session-search-info">本机搜索最近会话中的提问与回答，不包含工具日志和模型思考；结果只覆盖最近的一部分会话</div>
      {/if}
    {/if}
    <div class="session-tree" aria-label="会话目录">
      {#each filteredSessionGroups as group (group.project)}
        {#if !sessionSearch.trim() || group.sessions.length}
          {@const expanded = isWorkspaceExpanded(groupExpansion, group.project, selectedProject, data?.default_project, sessionSearch)}
          {@const pinnedIds = pinnedByProject[group.project] ?? []}
          <div class="session-folder">
            <div class="folder-row" class:current={group.project === selectedProject}>
              <button class="group-heading" aria-expanded={expanded}
                aria-label={(expanded ? '收起' : '展开') + (group.is_default ? '个人对话' : groupName(group.project))}
                title={group.is_default ? '个人对话 · ' + group.project : group.project}
                onclick={() => toggleGroup(group.project)}>
                <ChevronRight size={13} class={expanded ? 'rotated' : ''}/>
                {#if group.is_default}
                  <MessageSquareText size={15}/>
                {:else}
                  <FolderClosed size={15}/>
                {/if}
                <span>{group.is_default ? '个人对话' : groupName(group.project)}</span>
                {#if groupRunningCount(statuses, group.project)}
                  <span class="group-running" title="该分组中有会话正在运行">
                    <span class="task-dot"></span>{groupRunningCount(statuses, group.project)}
                  </span>
                {/if}
              </button>
              <button class="folder-new sidebar-context-action" aria-label={'在' + (group.is_default ? '个人对话' : groupName(group.project)) + '中新建会话'}
                title="在此分组新建会话" disabled={busy || !data?.ready}
                onclick={() => onNewChatInProject(group.project)}><Plus size={15}/></button>
            </div>
            {#if expanded}
              <div class="folder-children">
                {#each visibleGroupSessions(group, pinnedIds, groupLimits[group.project] ?? 7) as session (session.id)}
                  {@const contentHit = sessionSearch.trim() ? sessionSearchHits.find(hit => hit.project===group.project && hit.id===session.id) : null}
                  {@const selected = selectedSessions.has(sessionSelectionKey(group.project, session.id))}
                  {@const pinned = pinnedIds.includes(session.id)}
                  {@const rowStatus = statusFor(statuses, group.project, session.id)}
                  <div class="session-row" class:search-content={!!contentHit}
                    class:active={group.project===selectedProject && activeSessionId===session.id && tab==='chat'}
                    class:pinned
                    class:selected={selected}>
                    {#if selectMode || selected}
                      <button class="session-select sidebar-context-action" class:on={selected}
                        aria-label={'选择会话 ' + sessionLabel(session)}
                        aria-pressed={selected}
                        title="选择（配合 Ctrl / Shift 多选）"
                        onclick={(event) => { event.stopPropagation(); toggleSessionSelection(group.project, session.id); }}></button>
                    {/if}
                    <button class="session-main" title={session.name || sessionLabel(session)}
                      aria-current={group.project === selectedProject && activeSessionId === session.id ? (tab === 'chat' ? 'page' : 'true') : undefined}
                      disabled={(busy && !navigationBusy) || (!data?.ready && !navigationBusy)}
                      onclick={(event) => onSessionClick(event, group.project, session.id, contentHit?.message_index)}>
                      <span>{sessionLabel(session)}</span>
                      {#if contentHit}
                        <small class="session-search-snippet">
                          <b class="session-search-role">{contentHit.role === 'assistant' ? '回答' : '你'}</b>
                          {contentHit.snippet}
                        </small>
                      {:else if rowStatus}
                        <small class="session-task-status" class:waiting={rowStatus === 'waiting'}>
                          <span class="task-dot"></span>{rowStatus === 'waiting' ? '等待输入' : '运行中'}
                        </small>
                      {:else if session.updated_at}
                        <small class="session-activity">{sessionActivityLabel(session.updated_at)}</small>
                      {/if}
                    </button>
                    <button class="session-pin sidebar-context-action" class:on={pinned}
                      aria-label={(pinned ? '取消置顶 ' : '置顶 ') + sessionLabel(session)}
                      aria-pressed={pinned}
                      title={pinned ? '取消置顶，回到最近的顺序' : '置顶会话，始终显示在本工作区顶部'}
                      onclick={(event) => { event.stopPropagation(); togglePin(group.project, session.id); }}>
                      {#if pinned}<PinOff size={13}/>{:else}<Pin size={13}/>{/if}
                    </button>
                    <button class="session-trash sidebar-context-action" aria-label={'删除 ' + sessionLabel(session)}
                      title="删除会话" disabled={busy || rowStatus === 'running' || rowStatus === 'waiting'}
                      onclick={() => askDelete(group.project,session.id,sessionLabel(session))}>
                      <Trash2 size={14}/>
                    </button>
                  </div>
                {/each}
                {#if !sessionSearch.trim() && group.sessions.length > visibleGroupSessions(group, pinnedIds, groupLimits[group.project] ?? 7).length}
                  <button class="show-more-sessions"
                    onclick={() => groupLimits={...groupLimits,[group.project]:(groupLimits[group.project]??7)+15}}>
                    查看更多…
                  </button>
                {/if}
              </div>
            {/if}
          </div>
        {/if}
      {/each}
      <button class="add-workspace-row" onclick={() => onChooseProject()} disabled={busy}>
        <FolderPlus size={15}/> 添加工作区
      </button>
      {#if sessionSearch.trim() && !matchingSessionCount && !sessionSearchBusy}
        <p class="sessions-empty">未找到匹配的会话</p>
      {:else if !sessionGroups.length && data}
        <p class="sessions-empty">还没有会话，先创建一个吧。</p>
      {/if}
    </div>
  </div>
  <div class="sidebar-bottom">
    {#if selectedSessions.size}
      <div class="bulk-bar">
        <span>已选 {selectedSessions.size} 个</span>
        <button class="bulk-delete" disabled={bulkDeleting} onclick={() => bulkConfirm = true}>
          {bulkDeleting ? '正在删除…' : '删除'}
        </button>
        <button class="bulk-cancel" disabled={bulkDeleting} onclick={() => { selectMode = false; selectedSessions = new Set(); selectionAnchor = null; }}>取消</button>
      </div>
    {/if}
    <nav class="primary-nav" aria-label="主导航">
      {#if tab !== 'chat'}
        <button class="sidebar-return-chat" title="返回对话" onclick={() => onSetTab('chat')}><ArrowLeft size={17}/> 返回对话</button>
      {/if}
      {#each $desktopSurfaces.filter(s => s.slot === 'tab' || s.slot === 'workspace') as surface (surface.plugin_id)}
        <button class:active={tab === `plugin:${surface.plugin_id}`}
          aria-current={tab === `plugin:${surface.plugin_id}` ? 'page' : undefined}
          title={surface.title}
          onclick={() => onSetTab(`plugin:${surface.plugin_id}`)}>
          <Puzzle size={17}/> {surface.title}
        </button>
      {/each}
      <div class="sidebar-utility-nav">
        <button title="快捷键 · F1" onclick={onShowShortcuts}><Keyboard size={17}/> 快捷键</button>
        <button class:active={tab === 'settings'} aria-current={tab === 'settings' ? 'page' : undefined}
          title="设置 · Ctrl+," onclick={() => onSetTab('settings')}><Settings2 size={17}/> 设置</button>
      </div>
    </nav>
    {#if data && !data.ready}
      <div class="sidebar-version"><span class="status-dot"></span> 尚未连接模型</div>
    {/if}
  </div>
</aside>

{#if pendingDelete}
  <DeleteConfirmDialog title={pendingDelete.title} busy={deletingSession} error={deleteError}
    onCancel={() => pendingDelete = null}
    onConfirm={() => void confirmDelete()}/>
{/if}
{#if bulkConfirm && selectedSessions.size}
  <DeleteConfirmDialog heading={`删除 ${selectedSessions.size} 个会话？`}
    title="这些会话会一起移入本地回收目录"
    description="不会删除工作目录中的代码文件，清理后仍可从回收目录恢复。"
    confirmLabel={`删除 ${selectedSessions.size} 个会话`}
    busy={bulkDeleting} error=""
    onCancel={() => bulkConfirm = false}
    onConfirm={() => void deleteSelectedSessions()}/>
{/if}
{#if storageOpen}
  <StorageDialog rows={storageRows} busy={storageBusy} purging={storagePurging} error={storageError} running={anyRunning} userHome={data?.user_home}
    onClose={() => storageOpen = false}
    onPurge={() => void purgeEmptySessions()}/>
{/if}

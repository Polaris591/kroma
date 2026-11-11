<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import {
    ArrowLeft, ChevronRight, FolderClosed, FolderPlus, HardDrive, LoaderCircle,
    MessageSquareText, Plus, Puzzle, Search, Settings2, Trash2, X,
  } from '@lucide/svelte';
  import { api, type Bootstrap, type SessionSearchHit, type SessionSnapshot } from '../lib/api';
  import {
    filterSessions, isWorkspaceExpanded, sessionActivityLabel, sessionLabel,
    visibleSessionsForGroup, workspaceDisplayName,
  } from '../lib/chat-ui';
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
  export let selectedProject: string | undefined;
  export let activeSessionId: string | undefined;
  export let running = false;
  export let busy = false;
  export let compacting = false;
  export let navigationBusy = false;
  export let guard: OperationGuard;
  export let getSnapshot: () => SessionSnapshot | null;
  export let onNewSession: () => void;
  export let onChooseProject: (path?: string) => void;
  export let onOpenSession: (project: string, id: string) => void;
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

  let sessionSearch = '';
  let sessionSearchOpen = false;
  let sessionSearchHits: SessionSearchHit[] = [];
  let sessionSearchBusy = false;
  let sessionSearchEpoch = 0;
  let sessionSearchTimer: ReturnType<typeof setTimeout> | null = null;
  let sessionSearchInput: HTMLInputElement;
  let groupExpansion: Record<string, boolean> = {};
  let groupLimits: Record<string, number> = {};
  let selectedSessions: Set<string> = new Set();
  let selectionAnchor: {project: string; id: string} | null = null;
  let bulkDeleting = false;
  let pendingDelete: SessionTarget | null = null;
  let deletingSession = false;
  let storageOpen = false;
  let storageRows: Array<{project: string; sessions: number; empty: number; bytes: number; empty_ids: string[]}> = [];
  let storageBusy = false;

  $: sessionGroups = data?.session_groups ?? [];
  $: matchedIds = matchedContentIds(sessionSearchHits);
  $: filteredSessionGroups = filterSessionGroups(sessionGroups, sessionSearch, matchedIds);
  $: matchingSessionCount = countGroupSessions(filteredSessionGroups);
  $: scheduleSessionContentSearch(sessionSearchOpen ? sessionSearch.trim() : '');

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

  function visibleGroupSessions(group: {sessions: Bootstrap['sessions']; project: string}) {
    if (sessionSearch.trim()) return group.sessions;
    return visibleSessionsForGroup(
      group.sessions, '', groupLimits[group.project] ?? 7,
      group.project === selectedProject ? activeSessionId : undefined
    );
  }

  function toggleSessionSelection(project: string, id: string) {
    selectedSessions = toggleSelection(selectedSessions, project, id);
    selectionAnchor = {project, id};
  }

  function onSessionClick(event: MouseEvent, project: string, id: string) {
    if (event.ctrlKey || event.metaKey) {
      event.preventDefault();
      toggleSessionSelection(project, id);
      return;
    }
    if (event.shiftKey && selectionAnchor && selectionAnchor.project === project) {
      event.preventDefault();
      const ids = (sessionGroups.find(group => group.project === project)?.sessions ?? []).map(entry => entry.id);
      selectedSessions = extendSelection(selectedSessions, ids, project, selectionAnchor.id, id);
      return;
    }
    onOpenSession(project, id);
  }

  async function deleteSelectedSessions() {
    if (!selectedSessions.size || bulkDeleting || running) return;
    const targets = [...selectedSessions]
      .map(key => parseSelectionKey(key))
      .filter((target): target is SessionTarget => target !== null)
      .map(target => ({...target, title: ''}));
    bulkDeleting = true;
    clearError();
    let removed = 0;
    let failed = 0;
    const removedTargets: SessionTarget[] = [];
    try {
      for (const target of targets) {
        try {
          await api.deleteSession(target.project, target.id);
          removed += 1;
          removedTargets.push({...target, title: ''});
        } catch {
          failed += 1;
        }
      }
      await reloadData();
      selectedSessions = new Set();
      selectionAnchor = null;
      onDeleted(removedTargets);
      await afterBulkDelete();
      toast(failed
        ? `已删除 ${removed} 个会话，${failed} 个失败`
        : `已删除 ${removed} 个会话`);
    } catch (failure) {
      showError(failure);
    } finally {
      bulkDeleting = false;
    }
  }

  function askDelete(project: string, id: string, title: string) {
    pendingDelete = {project, id, title};
  }

  async function confirmDelete() {
    const target = pendingDelete;
    if (!target || running || busy || deletingSession) return;
    deletingSession = true;
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
      showError(failure);
    } finally {
      deletingSession = false;
      if (guard.isCurrent(generation)) setBusy(false);
    }
  }

  async function openStorage() {
    storageOpen = true;
    storageBusy = true;
    try { storageRows = await api.storageReport(); }
    catch (failure) { showError(failure); }
    finally { storageBusy = false; }
  }

  async function purgeEmptySessions() {
    const targets = storageRows.flatMap(row => row.empty_ids.map(id => ({project: row.project, id, title: ''})));
    if (!targets.length || storageBusy || running) return;
    storageBusy = true;
    let removed = 0;
    try {
      const removedTargets: SessionTarget[] = [];
      for (const target of targets) {
        try {
          await api.deleteSession(target.project, target.id);
          removed += 1;
          removedTargets.push(target);
        } catch { /* skip */ }
      }
      onDeleted(removedTargets);
      await reloadData();
      storageRows = await api.storageReport();
      toast(`已清理 ${removed} 个空会话`);
    } catch (failure) { showError(failure); }
    finally { storageBusy = false; }
  }
</script>

<aside class="sidebar">
  <div class="brand"><div class="brand-mark"><span>K</span></div><div><strong>Kroma</strong><small>DESKTOP</small></div></div>
  <div class="sidebar-actions">
    <button class="new-chat" onclick={onNewSession} disabled={!data?.ready || running || busy || compacting}
      title={`在 ${data?.project ? workspaceDisplayName(data.project, data.user_home) : '当前工作区'} 中新建会话`}>
      <Plus size={16}/> 新建会话
    </button>
    <button class="sidebar-search-toggle" class:active={sessionSearchOpen}
      onclick={toggleSessionSearch} aria-label="搜索会话" aria-expanded={sessionSearchOpen}
      title="搜索会话 · Ctrl+K"><Search size={17}/></button>
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
        <div class="session-search-info">本机搜索最近会话的用户消息，不包含工具日志</div>
      {/if}
    {/if}
    <div class="session-tree" aria-label="会话目录">
      <div class="session-tree-label">会话</div>
      {#each filteredSessionGroups as group (group.project)}
        {#if !sessionSearch.trim() || group.sessions.length}
          {@const expanded = isWorkspaceExpanded(groupExpansion, group.project, selectedProject, data?.default_project, sessionSearch)}
          <div class="session-folder">
            <div class="folder-row" class:current={group.project === selectedProject}>
              <button class="group-heading" aria-expanded={expanded}
                aria-label={(expanded ? '收起' : '展开') + (group.is_default ? '个人对话' : workspaceDisplayName(group.project, data?.user_home))}
                title={group.is_default ? '个人对话 · ' + group.project : group.project}
                onclick={() => toggleGroup(group.project)}>
                <ChevronRight size={13} class={expanded ? 'rotated' : ''}/>
                {#if group.is_default}
                  <MessageSquareText size={15}/>
                {:else}
                  <FolderClosed size={15}/>
                {/if}
                <span>{group.is_default ? '个人对话' : workspaceDisplayName(group.project, data?.user_home)}</span>
              </button>
              <button class="folder-new" aria-label={'在' + (group.is_default ? '个人对话' : workspaceDisplayName(group.project,data?.user_home)) + '中新建会话'}
                title="在此分组新建会话" disabled={running || busy || compacting || !data?.ready}
                onclick={() => onNewChatInProject(group.project)}><Plus size={15}/></button>
            </div>
            {#if expanded}
              <div class="folder-children">
                {#each visibleGroupSessions(group) as session (session.id)}
                  {@const contentHit = sessionSearch.trim() ? sessionSearchHits.find(hit => hit.project===group.project && hit.id===session.id) : null}
                  <div class="session-row" class:search-content={!!contentHit}
                    class:active={group.project===selectedProject && activeSessionId===session.id && tab==='chat'}
                    class:selected={selectedSessions.has(sessionSelectionKey(group.project, session.id))}>
                    <button class="session-select" class:on={selectedSessions.has(sessionSelectionKey(group.project, session.id))}
                      aria-label={'选择会话 ' + sessionLabel(session)}
                      aria-pressed={selectedSessions.has(sessionSelectionKey(group.project, session.id))}
                      title="选择（配合 Ctrl / Shift 多选）"
                      onclick={(event) => { event.stopPropagation(); toggleSessionSelection(group.project, session.id); }}></button>
                    <button class="session-main" title={session.name || sessionLabel(session)}
                      disabled={running || (busy && !navigationBusy) || (!data?.ready && !navigationBusy)}
                      onclick={(event) => onSessionClick(event, group.project, session.id)}>
                      <span>{sessionLabel(session)}</span>
                      {#if contentHit}
                        <small class="session-search-snippet">{contentHit.snippet}</small>
                      {:else if session.updated_at}
                        <small class="session-activity">{sessionActivityLabel(session.updated_at)}</small>
                      {/if}
                    </button>
                    <button class="session-trash" aria-label={'删除 ' + sessionLabel(session)}
                      title="删除会话" disabled={running || busy}
                      onclick={() => askDelete(group.project,session.id,sessionLabel(session))}>
                      <Trash2 size={14}/>
                    </button>
                  </div>
                {/each}
                {#if !sessionSearch.trim() && group.sessions.length > visibleGroupSessions(group).length}
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
      <button class="add-workspace-row" onclick={() => onChooseProject()} disabled={running || busy}>
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
        <button class="bulk-delete" disabled={bulkDeleting || running} onclick={() => void deleteSelectedSessions()}>
          {bulkDeleting ? '正在删除…' : '删除'}
        </button>
        <button class="bulk-cancel" disabled={bulkDeleting} onclick={() => { selectedSessions = new Set(); selectionAnchor = null; }}>取消</button>
      </div>
    {/if}
    <nav class="primary-nav" aria-label="主导航">
      {#if tab !== 'chat'}
        <button onclick={() => onSetTab('chat')}><ArrowLeft size={17}/> 返回对话</button>
      {/if}
      {#each $desktopSurfaces.filter(s => s.slot === 'tab') as surface (surface.plugin_id)}
        <button class:active={tab === `plugin:${surface.plugin_id}`}
          onclick={() => onSetTab(`plugin:${surface.plugin_id}`)}>
          <Puzzle size={17}/> {surface.title}
        </button>
      {/each}
      <button class:active={tab === 'plugins'} onclick={() => onSetTab('plugins')}><Puzzle size={17}/> 插件</button>
      <button class:active={tab === 'settings'} onclick={() => onSetTab('settings')}><Settings2 size={17}/> 模型与设置</button>
      <button onclick={() => void openStorage()}><HardDrive size={17}/> 本机数据</button>
    </nav>
    {#if data && !data.ready}
      <div class="sidebar-version"><span class="status-dot"></span> 尚未连接模型</div>
    {/if}
  </div>
</aside>

{#if pendingDelete}
  <DeleteConfirmDialog title={pendingDelete.title} busy={deletingSession}
    onCancel={() => pendingDelete = null}
    onConfirm={() => void confirmDelete()}/>
{/if}
{#if storageOpen}
  <StorageDialog rows={storageRows} busy={storageBusy} {running} userHome={data?.user_home}
    onClose={() => storageOpen = false}
    onPurge={() => void purgeEmptySessions()}/>
{/if}

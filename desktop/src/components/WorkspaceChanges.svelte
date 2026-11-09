<script lang="ts">
  import { onMount } from 'svelte';
  import { Copy, FileDiff, GitBranch, LoaderCircle, RefreshCw, X } from '@lucide/svelte';
  import { api, type WorkspaceFileDiff, type WorkspaceReview, type WorkspaceChange } from '../lib/api';

  export let project: string;
  export let review: WorkspaceReview | null = null;
  export let busy = false;
  export let error = '';
  export let onClose: () => void;
  export let onRefresh: () => void;

  let closeButton: HTMLButtonElement;
  let chosenPath = '';
  let detail: WorkspaceFileDiff | null = null;
  let detailBusy = false;
  let detailError = '';
  let request = 0;
  let copied = false;

  const captions: Record<WorkspaceChange['status'], string> = {
    modified:'修改', added:'新增', deleted:'删除',
    renamed:'重命名', conflict:'冲突', untracked:'未跟踪',
  };

  onMount(() => closeButton?.focus());

  async function selectFile(path: string) {
    const current = ++request;
    chosenPath = path;
    detail = null;
    detailError = '';
    copied = false;
    detailBusy = true;
    try {
      const next = await api.workspaceFileDiff(path);
      if (current === request) detail = next;
    } catch(e) {
      if (current === request) detailError = e instanceof Error ? e.message : String(e);
    } finally {
      if (current === request) detailBusy = false;
    }
  }

  async function copyDiff() {
    if (!detail?.diff) return;
    try {
      await navigator.clipboard.writeText(detail.diff);
      copied = true;
    } catch {
      copied = false;
    }
  }

  $: if (review && !busy && review.entries.length && !chosenPath) {
    void selectFile(review.entries[0].path);
  }
</script>

<svelte:window onkeydown={e=>{ if (e.key === 'Escape') {e.preventDefault();onClose();} }}/>

<div class="review-overlay">
  <button class="review-scrim" aria-label="关闭工作区变更" onclick={onClose}></button>
  <div class="review-panel" role="dialog" aria-modal="true" aria-label="查看工作区变更">
    <header class="review-header">
      <div class="review-heading">
        <span class="review-emblem"><FileDiff size={19}/></span>
        <div><h2>工作区变更</h2><p title={project}>{project.split(/[\\/]/).filter(Boolean).pop()}</p></div>
      </div>
      <div class="review-header-actions">
        <button title="刷新工作区变更" aria-label="刷新工作区变更" disabled={busy} onclick={onRefresh}>
          {#if busy}<LoaderCircle class="spin" size={16}/>{:else}<RefreshCw size={16}/>{/if}
        </button>
        <button bind:this={closeButton} title="关闭" aria-label="关闭变更查看" onclick={onClose}><X size={18}/></button>
      </div>
    </header>
    <p class="review-disclaimer">这里显示的是 <strong>Git 工作区的所有未提交变更</strong>，可能包含你之前或其他程序的修改，不代表全部由本次 Kroma 任务产生。</p>
    {#if error}
      <p class="review-error" role="alert">{error}</p>
    {/if}
    <div class="review-layout">
      <aside class="review-files" aria-label="变更文件列表">
        {#if busy && !review}
          <div class="review-placeholder"><LoaderCircle size={18} class="spin"/> 正在读取 Git 工作区…</div>
        {:else if !review?.is_repo}
          <div class="review-placeholder"><GitBranch size={20}/>{review?.message || '此目录暂无 Git 仓库信息。'}</div>
        {:else}
          <div class="review-file-count">文件变更 <strong>{review.entries.length}{review.truncated ? '+' : ''}</strong></div>
          {#if !review.entries.length}
            <div class="review-placeholder">当前工作区没有 Git 变更。</div>
          {:else}
            {#each review.entries as file (file.path)}
              <button class="review-file-row" class:active={chosenPath === file.path}
                title={file.path} onclick={() => void selectFile(file.path)}>
                <span class="review-file-status" class:untracked={file.untracked}>{captions[file.status]}</span>
                <span class="review-file-name">{file.path}</span>
                {#if file.staged}<span class="review-staged" title="包含已暂存的改动">暂存</span>{/if}
              </button>
            {/each}
            {#if review.truncated}<p class="review-limit">变更较多，仅列出前 200 项。</p>{/if}
          {/if}
        {/if}
      </aside>
      <div class="review-diff" aria-label="文件文本差异">
        {#if chosenPath}
          <div class="review-diff-title"><strong title={chosenPath}>{chosenPath.split(/[\\/]/).pop()}</strong>
            <button disabled={!detail?.diff} onclick={copyDiff}><Copy size={14}/>{copied ? '已复制' : '复制差异'}</button>
          </div>
          {#if detailBusy}
            <div class="review-placeholder"><LoaderCircle size={18} class="spin"/> 正在读取差异…</div>
          {:else if detailError}
            <p class="review-error" role="alert">{detailError}</p>
          {:else if detail?.diff}
            <pre class="review-diff-pre"><code>{#each detail.diff.split('\n').slice(0,1500) as line,i (i)}<span class:added={line.startsWith('+')&&!line.startsWith('+++')} class:removed={line.startsWith('-')&&!line.startsWith('---')} class:hunk={line.startsWith('@@')}>{line || ' '}{'\n'}</span>{/each}</code></pre>
            {#if detail.truncated || detail.diff.split('\n').length>1500}
              <p class="review-limit">差异过长，已截断显示；完整代码仍保留在磁盘中。</p>
            {/if}
          {:else}
            <div class="review-placeholder">{detail?.message || '此文件没有可显示的文本差异。'}</div>
          {/if}
        {:else}
          <div class="review-placeholder">选择文件查看具体变更</div>
        {/if}
      </div>
    </div>
  </div>
</div>

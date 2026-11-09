<script lang="ts">
  import { onMount } from 'svelte';
  import { FileDiff, LoaderCircle, X, AlertCircle } from '@lucide/svelte';
  import { api } from '../lib/api';
  import type { TurnDiffSummary } from '../lib/api';
  import { createRequestTokens } from '../lib/latest';

  export let turn: number;
  export let summary: TurnDiffSummary | null = null;
  export let files: Array<{path: string; status: string; added: number; removed: number}> = [];
  export let onClose: () => void = () => {};

  let active = '';
  let diff = '';
  let loading = false;
  let error = '';
  // Only the newest request may paint the panel: switching files quickly must
  // never show one file's highlighted row next to another file's diff.
  const requests = createRequestTokens();

  async function load(path: string) {
    const token = requests.begin();
    active = path;
    loading = true;
    error = '';
    diff = '';
    try {
      const result = await api.turnFileDiff(turn, path);
      if (requests.isCurrent(token)) diff = result.diff || '';
    } catch (failure) {
      if (requests.isCurrent(token)) error = String(failure);
    } finally {
      if (requests.isCurrent(token)) loading = false;
    }
  }

  onMount(() => { if (files.length) void load(files[0].path); });

  // Keep the diff readable: colour whole lines, keep the leading marker.
  $: lines = diff.split('\n').map(line => {
    const kind = line.startsWith('+') && !line.startsWith('+++') ? 'added'
      : line.startsWith('-') && !line.startsWith('---') ? 'removed'
      : line.startsWith('@@') ? 'hunk' : '';
    return { kind, line };
  });
</script>

<div class="review-overlay" role="presentation">
  <button class="review-scrim" aria-label="关闭本轮改动" onclick={onClose}></button>
  <div class="review-panel" role="dialog" aria-modal="true" aria-labelledby="turn-diff-heading">
    <div class="review-header">
      <div class="review-heading">
        <span class="review-emblem"><FileDiff size={18}/></span>
        <div>
          <h2 id="turn-diff-heading">本轮改动 · 第 {turn} 轮</h2>
          <p>{summary ? `${summary.files} 个文件 · +${summary.added} −${summary.removed}` : '按快照对比'}</p>
        </div>
      </div>
      <div class="review-header-actions">
        <button title="关闭" aria-label="关闭" onclick={onClose}><X size={16}/></button>
      </div>
    </div>
    <p class="review-disclaimer">
      数据来自任务开始前的文件快照，只包含这一轮真正产生的变化，不包含任务前几天就有的修改。
    </p>
    <div class="review-layout">
      <div class="review-files">
        <div class="review-file-count">文件 <strong>{files.length}</strong></div>
        {#each files as file (file.path)}
          <button class="review-file-row" class:active={file.path === active} onclick={() => void load(file.path)}>
            <span class="review-file-status" class:untracked={file.status === 'A'}>{file.status}</span>
            <span class="review-file-name" title={file.path}>{file.path}</span>
            <span class="review-staged">+{file.added} −{file.removed}</span>
          </button>
        {/each}
        {#if summary?.truncated}<p class="review-limit">文件过多，仅显示前 {files.length} 个</p>{/if}
      </div>
      <div class="review-diff">
        <div class="review-diff-title">
          <strong>{active || '选择一个文件'}</strong>
          {#if loading}<LoaderCircle size={14} class="spin"/>
          {:else if diff}<span class="review-limit">{lines.length} 行</span>{/if}
        </div>
        {#if error}
          <div class="review-error"><AlertCircle size={14}/> {error}</div>
        {:else if loading}
          <div class="review-placeholder"><LoaderCircle size={18} class="spin"/> 正在载入差异…</div>
        {:else if lines.length}
          <pre class="review-diff-pre">{#each lines as row}<span class={row.kind}>{row.line}</span>{/each}</pre>
        {:else}
          <div class="review-placeholder">没有可显示的差异</div>
        {/if}
      </div>
    </div>
  </div>
</div>

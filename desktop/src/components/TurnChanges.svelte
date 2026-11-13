<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { modal } from '../lib/modal';
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
  onDestroy(() => { requests.begin(); });

  // Keep the diff readable: colour whole lines, keep the leading marker.
  $: lines = diff ? diff.split('\n').map(line => {
    const kind = line.startsWith('+') && !line.startsWith('+++') ? 'added'
      : line.startsWith('-') && !line.startsWith('---') ? 'removed'
      : line.startsWith('@@') ? 'hunk' : '';
    return { kind, line };
  }) : [];
</script>

<dialog class="review-overlay" aria-labelledby="turn-diff-heading" use:modal={{ onClose, initialFocus: '[aria-label="关闭本轮改动面板"]' }}>
  <button class="review-scrim" tabindex="-1" aria-label="关闭本轮改动" onclick={onClose}></button>
  <div class="review-panel">
    <div class="review-header">
      <div class="review-heading">
        <span class="review-emblem"><FileDiff size={18}/></span>
        <div>
          <h2 id="turn-diff-heading">本轮改动 · 第 {turn} 轮</h2>
          <p>{summary ? `${summary.files} 个文件 · +${summary.added} −${summary.removed}` : '按快照对比'}</p>
        </div>
      </div>
      <div class="review-header-actions">
        <button title="关闭" aria-label="关闭本轮改动面板" onclick={onClose}><X size={16}/></button>
      </div>
    </div>
    <p class="review-disclaimer">
      与本轮任务开始前的文件快照对比，不包含此前已有的改动；执行期间其他程序的修改也可能出现在这里。
    </p>
    <div class="review-layout">
      <div class="review-files">
        <div class="review-file-count">文件 <strong>{files.length}</strong></div>
        {#each files as file (file.path)}
          <button class="review-file-row" class:active={file.path === active} aria-pressed={file.path === active} onclick={() => void load(file.path)}>
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
          <div class="review-error" role="alert"><AlertCircle size={14}/> {error}
            <button class="secondary small" onclick={() => void load(active)}>重试</button></div>
        {:else if loading}
          <div class="review-placeholder"><LoaderCircle size={18} class="spin"/> 正在载入差异…</div>
        {:else if lines.length}
          <!-- svelte-ignore a11y_no_noninteractive_tabindex (Keyboard users need to scroll this read-only diff.) -->
          <pre class="review-diff-pre" tabindex="0" aria-label="本轮文件差异">{#each lines as row}<span class={row.kind}>{row.line || ' '}</span>{/each}</pre>
        {:else}
          <div class="review-placeholder">没有可显示的差异</div>
        {/if}
      </div>
    </div>
  </div>
</dialog>

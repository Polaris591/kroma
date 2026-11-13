<script lang="ts">
  export let messages: string[] = [];
  export let title = '能力检查提示';
  export let hint = '以下是清单检查结果，不代表所有项目都不可用。';
  $: entries = [...new Set(messages.map(message => message.trim()).filter(Boolean))];
</script>

{#if entries.length}
  <details class="settings-diagnostics">
    <summary>{title}<span>（{entries.length}）</span></summary>
    <p class="diagnostics-hint">{hint}</p>
    <ul>
      {#each entries as message}
        {@const duplicate = /^duplicate plugin id '([^']+)': /u.exec(message)}
        <li>
          {#if duplicate}
            <strong>插件 {duplicate[1]} 存在同名来源</strong>
            <p>已保留先发现的来源，跳过重复项。</p>
          {/if}
          <code>{message}</code>
        </li>
      {/each}
    </ul>
  </details>
{/if}

<style>
  .settings-diagnostics { margin: 14px 0; padding: 0 12px; border: 1px solid var(--line); border-radius: var(--radius-md); background: var(--wash-1); }
  summary { padding: 11px 0; color: var(--text); font-size: 12px; cursor: pointer; overflow-wrap: anywhere; }
  summary::marker { color: var(--warning); }
  summary span { color: var(--muted); font-variant-numeric: tabular-nums; }
  .diagnostics-hint { margin: 0 0 10px; color: var(--muted); font-size: 12px; line-height: 1.7; }
  ul { max-height: min(240px, 32dvh); margin: 0; padding: 0 6px 12px 0; overflow: auto; list-style: none; }
  li { padding: 10px 0; border-top: 1px solid var(--hairline); overflow-wrap: anywhere; }
  strong { font-size: 12px; font-weight: 550; }
  li p { margin: 4px 0; color: var(--muted); font-size: 12px; line-height: 1.7; }
  code { display: block; color: var(--muted); font: 11px/1.7 var(--mono); white-space: pre-wrap; overflow-wrap: anywhere; }
</style>

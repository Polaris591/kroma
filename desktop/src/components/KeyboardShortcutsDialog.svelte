<script lang="ts">
  import { Keyboard } from '@lucide/svelte';
  import { modal } from '../lib/modal';

  export let onClose: () => void;

  // Every entry below exists in App.handleGlobalKey or
  // ComposerPanel.handleEditor; nothing is promised beyond what those do.
  const sections: Array<{ title: string; rows: Array<{ keys: string[]; note: string }> }> = [
    {
      title: '窗口',
      rows: [
        { keys: ['Ctrl/Cmd', 'K'], note: '打开搜索，查找会话标题和本机最近消息' },
        { keys: ['Ctrl/Cmd', 'N'], note: '新建会话（模型已连接且当前空闲时）' },
        { keys: ['Ctrl/Cmd', 'B'], note: '显示或隐藏侧栏' },
        { keys: ['Ctrl/Cmd', 'O'], note: '切换工作区' },
        { keys: ['Ctrl/Cmd', ','], note: '打开设置' },
        { keys: ['F1'], note: '打开这份快捷键说明' },
        { keys: ['Esc'], note: '关闭最上层的对话框、面板、搜索或窄窗口侧栏' },
      ],
    },
    {
      title: '输入框',
      rows: [
        { keys: ['Enter'], note: '发送任务；任务运行中则把内容插入当前任务' },
        { keys: ['Shift', 'Enter'], note: '换行，不发送' },
        { keys: ['Ctrl', 'Enter'], note: '任务运行中把内容加入后续任务队列（排队，不支持 Cmd）' },
        { keys: ['@'], note: '引用工作区文件，输入后会给出文件建议' },
        { keys: ['↑ / ↓'], note: '在文件建议中移动选择' },
        { keys: ['Enter / Tab'], note: '采用选中的文件建议' },
        { keys: ['Esc'], note: '收起文件建议' },
      ],
    },
  ];
</script>

<dialog class="dialog-overlay" aria-labelledby="shortcuts-heading"
  use:modal={{ onClose, initialFocus: '.dialog-primary' }}>
  <div class="dialog-dim"></div>
  <div class="dialog-panel shortcuts-panel">
    <div class="dialog-heading">
      <span class="dialog-emblem"><Keyboard size={19}/></span>
      <div><h2 id="shortcuts-heading">快捷键</h2><p>与当前输入位置和执行状态有关</p></div>
    </div>
    {#each sections as section (section.title)}
      <h3 class="shortcut-title">{section.title}</h3>
      <dl class="shortcut-list">
        {#each section.rows as row (row.note)}
          <div class="shortcut-row">
            <dt>{#each row.keys as key, index (key)}<kbd>{key}</kbd>{#if index < row.keys.length - 1}<span class="shortcut-plus">+</span>{/if}{/each}</dt>
            <dd>{row.note}</dd>
          </div>
        {/each}
      </dl>
    {/each}
    <p class="shortcut-hint">
      这些组合与上下文相关：已有对话框打开、任务运行中或输入法正在组合时，其中一部分不会生效。
      Kroma 目前不支持自定义快捷键。
    </p>
    <div class="dialog-actions">
      <button class="dialog-primary" onclick={onClose}>关闭</button>
    </div>
  </div>
</dialog>

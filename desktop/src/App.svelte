<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import {
    MessageSquareText, Settings2, Puzzle, FolderOpen, ChevronDown, Plus,
    Send, Square, Paperclip, X, Terminal, BrainCircuit, Cpu, RefreshCw,
    LoaderCircle, ArrowDown, Sparkles, AlertCircle, Columns3, FileText
  } from '@lucide/svelte';
  import { api, type Bootstrap, type DesktopEvent, type StreamEnvelope, type ProfileInput } from './lib/api';
  import { compactCount, cachePercentage } from './lib/format';
  import MessageView from './components/MessageView.svelte';
  import Markdown from './components/Markdown.svelte';
  import SettingsView from './components/SettingsView.svelte';
  import PluginsView from './components/PluginsView.svelte';

  type Tab = 'chat' | 'settings' | 'plugins';
  type ActiveTool = {id: string; name: string; args: string; output: string; result: string; failed: boolean; done: boolean};
  let data: Bootstrap | null = null;
  let tab: Tab = 'chat';
  let error = '';
  let notice = '';
  let busy = false;
  let running = false;
  let prompt = '';
  let attached: string[] = [];
  let stream = '';
  let thinking = '';
  let activeTools: ActiveTool[] = [];
  let draftPrompt = '';
  let historyCount = 150;
  let scroller: HTMLDivElement;
  let atBottom = true;
  let initialized = false;
  let rename = false;
  let newTitle = '';

  $: snapshot = data?.snapshot ?? null;
  $: history = snapshot?.messages.filter(m => m.role !== 'system') ?? [];
  $: visibleHistory = history.slice(-historyCount);
  $: modelName = data?.profiles.profiles.find(p => p.name === data?.profiles.active)?.model || '未配置模型';
  $: usage = snapshot?.usage?.coding;
  $: pluginCount = data?.plugins.filter(p => p.enabled).length ?? 0;
  $: canSend = !!data?.ready && !busy && !running && (prompt.trim().length > 0 || attached.length > 0);

  function showError(e: unknown) { error = e instanceof Error ? e.message : String(e); }
  function toast(message: string) { notice = message; }
  async function refresh() { data = await api.bootstrap(); }
  async function initialize() {
    try {
      data = await api.bootstrap();
      if (data.profiles.profiles.length || data.plugins.some(p => p.enabled && p.has_model)) {
        await doConnect();
      } else tab = 'settings';
    } catch (e) { showError(e); }
    finally { initialized = true; }
  }
  onMount(() => {
    void initialize();
    let stop: (() => void) | undefined;
    void getCurrentWebview().onDragDropEvent(event => {
      if (event.payload.type === 'drop') addPaths(event.payload.paths);
    }).then(fn => { stop = fn; }).catch(() => {});
    return () => stop?.();
  });

  async function doConnect(name?: string) {
    busy = true; error = ''; notice = '';
    try {
      data = await api.connect(name);
      running = false; stream = ''; thinking = ''; activeTools = [];
      tab = 'chat'; await scrollDown();
    } catch (e) { showError(e); }
    finally { busy = false; }
  }
  async function chooseProject() {
    try {
      const path = await open({directory: true, multiple: false, title: '选择工作目录'});
      if (typeof path === 'string') {
        data = await api.openProject(path);
        stream = ''; thinking = ''; activeTools = [];
        if (data.profiles.profiles.length) await doConnect();
        else tab = 'settings';
      }
    } catch (e) {showError(e)}
  }
  async function switchSession(id: string) {
    if (running || busy) return;
    busy = true; error = '';
    try {
      data = await api.switchSession(id);
      stream = ''; thinking = ''; activeTools = []; historyCount = 150;
      tab = 'chat'; await scrollDown();
    } catch(e) {showError(e)}
    finally {busy = false}
  }
  async function newSession() {
    if (running || busy) return;
    busy = true; error = '';
    try {
      data = await api.newSession();
      stream = ''; thinking = ''; activeTools = []; attached = []; prompt = ''; historyCount = 150;
      tab = 'chat'; await scrollDown();
    } catch(e) {showError(e)}
    finally {busy = false}
  }
  async function saveTitle() {
    try { data = await api.renameSession(newTitle); rename = false; }
    catch (e) { showError(e) }
  }
  function addPaths(paths: string[]) { attached = [...new Set([...attached, ...paths])].slice(0, 8); }
  async function attachFile() {
    try {
      const selected = await open({multiple: true, title: '添加图片或文件'});
      if (typeof selected === 'string') addPaths([selected]);
      else if (Array.isArray(selected)) addPaths(selected);
    } catch (e) {showError(e)}
  }
  function applyEvents(events: DesktopEvent[]) {
    for (const e of events) {
      switch(e.kind) {
        case 'text_delta': stream += e.text ?? ''; break;
        case 'reasoning_delta': thinking += e.text ?? ''; break;
        case 'model_finished':
          if (e.reasoning && !thinking) thinking = String(e.reasoning);
          break;
        case 'tool_started':
          activeTools = [...activeTools, {id: String(e.id), name: String(e.name),
            args: String(e.arguments ?? ''), output: '', result: '', failed: false, done: false}];
          break;
        case 'tool_output':
          activeTools = activeTools.map(t => t.id === e.id
            ? {...t, output: (t.output + (e.text ?? '')).slice(-32000)} : t);
          break;
        case 'tool_finished':
          activeTools = activeTools.map(t => t.id === e.id
            ? {...t, result: String(e.result ?? ''), failed: !!e.failed, done: true} : t);
          break;
      }
    }
    void scrollIfBottom();
  }
  function onStream(event: StreamEnvelope) {
    if (event.kind === 'batch') applyEvents(event.events);
    if (event.kind === 'snapshot' && data) data = {...data, snapshot: event.snapshot};
    if (event.kind === 'done' && event.result.error) error = event.result.error;
  }
  async function submit() {
    if (running) {
      if (!prompt.trim()) return;
      try {await api.steer(prompt); prompt = ''; toast('已向当前任务插入指令');}
      catch(e) {showError(e)}
      return;
    }
    if (!canSend) {if (!data?.ready) tab = 'settings';return;}
    const text = prompt.trim();
    const paths = [...attached];
    draftPrompt = text || ('已添加 ' + paths.length + ' 个文件');
    prompt = ''; attached = []; running = true; stream = ''; thinking = ''; activeTools = []; error = '';
    await scrollDown();
    try {
      const result = await api.send(text, paths, onStream);
      if (result.result.error) error = result.result.error;
    } catch(e) { showError(e); }
    finally {
      running = false; draftPrompt = ''; stream = ''; thinking = ''; activeTools = [];
      try {await refresh();} catch(e) {showError(e)}
      await scrollDown();
    }
  }
  function handleEditor(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault(); void submit();
    }
  }
  async function cancelTask() { try {await api.cancel();} catch(e) {showError(e)} }
  async function saveProfile(profile: ProfileInput) {
    try {
      const profiles = await api.saveProfile(profile);
      if (data) data = {...data, profiles};
      toast('配置已保存');
    } catch(e) {showError(e);throw e}
  }
  async function pluginAction(action: string, id?: string, path?: string, key?: string, value?: unknown) {
    try {
      const result = await api.pluginAction(action,id,path,key,value);
      if (data) data = {...data, plugins: result.plugins};
      toast(result.restart_required ? (result.warning || '需要重新连接模型后生效') : result.message);
    } catch(e) {showError(e)}
  }
  async function installPlugin() {
    try {
      const dir = await open({directory: true, title: '选择包含 plugin.toml 的插件文件夹'});
      if(typeof dir === 'string') await pluginAction('install',undefined,dir);
    } catch(e) {showError(e)}
  }
  async function scrollDown() { await tick(); if(scroller) scroller.scrollTop = scroller.scrollHeight; }
  async function scrollIfBottom() { if (atBottom) await scrollDown(); }
  function trackScroll() {
    if (!scroller) return;
    atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 115;
  }
  function compact(value: number | undefined) { return compactCount(value); }
  function cacheRate() { return cachePercentage(usage?.input, usage?.cached); }
</script>

<svelte:head><title>Kroma Desktop</title></svelte:head>
<div class="app-shell">
  <aside class="sidebar">
    <div class="brand"><div class="brand-mark"><span>Ｋ</span></div><div><strong>Kroma</strong><small>DESKTOP</small></div></div>
    <button class="new-chat" onclick={newSession} disabled={!data?.ready || running || busy}><Plus size={17}/> 新建会话</button>
    <nav class="primary-nav" aria-label="主导航">
      <button class:active={tab === 'chat'} onclick={() => tab = 'chat'}><MessageSquareText size={17}/> 对话</button>
      <button class:active={tab === 'plugins'} onclick={() => tab = 'plugins'}><Puzzle size={17}/> 插件 <span class="nav-counter">{pluginCount}</span></button>
      <button class:active={tab === 'settings'} onclick={() => tab = 'settings'}><Settings2 size={17}/> 模型与设置</button>
    </nav>
    <div class="sidebar-section">
      <div class="sidebar-section-title">历史会话 <span>{data?.sessions.length ?? 0}</span></div>
      <div class="session-list">
        {#each data?.sessions ?? [] as session (session.id)}
          <button class:active={tab === 'chat' && snapshot?.session_id === session.id}
            disabled={running || busy || !data?.ready}
            title={session.id} onclick={() => switchSession(session.id)}>
            <MessageSquareText size={15}/>
            <span>{session.name || (session.id === 'default' ? '默认会话' : session.id)}</span>
          </button>
        {/each}
        {#if data && !data.sessions.length}<p class="sessions-empty">开始对话后自动创建会话</p>{/if}
      </div>
    </div>
    <div class="sidebar-bottom">
      <button class="workspace-button" onclick={chooseProject} disabled={running || busy} title={data?.project || ''}>
        <FolderOpen size={16}/><div><small>工作目录</small><span>{(data?.project || '选择工作目录').split(/[\\/]/).filter(Boolean).pop()}</span></div><ChevronDown size={14}/>
      </button>
      <div class="sidebar-version"><span class="status-dot" class:online={data?.ready}></span>{data?.ready ? '已连接 Rust Core' : '未连接模型'}<span>v0.1</span></div>
    </div>
  </aside>
  <main class="main-area">
    <header class="topbar">
      <div class="breadcrumb">
        {#if tab === 'chat'}
          <MessageSquareText size={16}/>
          {#if rename}
            <input class="rename-input" bind:value={newTitle} onkeydown={(e) => e.key === 'Enter' && void saveTitle()} />
            <button class="small secondary" onclick={saveTitle}>完成</button>
          {:else}
            <button class="title-button" onclick={() => {newTitle = data?.sessions.find(s => s.id === snapshot?.session_id)?.name || ''; rename = true;}} disabled={!snapshot?.session_id || snapshot.session_id === 'unknown'}>
              {data?.sessions.find(s => s.id === snapshot?.session_id)?.name || '新的对话'}
            </button>
          {/if}
        {:else if tab === 'settings'}<Settings2 size={16}/> 模型与设置
        {:else}<Puzzle size={16}/> 插件管理{/if}
      </div>
      <div class="topbar-right">
        {#if data?.ready}<span class="model-pill"><Cpu size={14}/>{modelName}</span>
        {:else}<span class="model-pill disconnected">未配置模型</span>{/if}
        <button class="icon-button" title="刷新状态" onclick={() => refresh().catch(showError)} disabled={busy || running}><RefreshCw size={16}/></button>
      </div>
    </header>
    {#if error}
      <div class="banner error-banner" role="alert"><AlertCircle size={16}/><span>{error}</span><button onclick={() => error = ''} title="关闭"><X size={16}/></button></div>
    {:else if notice}
      <div class="banner notice-banner" role="status"><span>{notice}</span><button onclick={() => notice = ''}><X size={15}/></button></div>
    {/if}
    {#if !initialized}
      <div class="loading-screen"><LoaderCircle size={28} class="spin"/><span>正在载入 Kroma…</span></div>
    {:else if tab === 'settings' && data}
      <div class="page-scroll"><SettingsView data={data.profiles} onSave={saveProfile} onConnect={doConnect}/></div>
    {:else if tab === 'plugins' && data}
      <div class="page-scroll"><PluginsView plugins={data.plugins} onAction={pluginAction} onInstall={installPlugin}/></div>
    {:else}
      <div class="chat-screen">
        <div class="transcript" bind:this={scroller} onscroll={trackScroll}>
          {#if history.length === 0 && !running}
            <div class="welcome">
              <div class="welcome-mark">Ｋ</div>
              <h1>让想法成为现实。</h1>
              <p>以自然语言驱动 Kroma 编写、分析与修改代码。</p>
              {#if !data?.ready}
                <button class="primary" onclick={() => tab = 'settings'}><Settings2 size={16}/> 配置模型以开始</button>
              {:else}
                <div class="welcome-hint"><Terminal size={14}/> Rust Core 已就绪，输入任务即可开始</div>
              {/if}
            </div>
          {:else}
            <div class="transcript-inner">
              {#if history.length > historyCount}
                <button class="load-earlier" onclick={() => historyCount += 150}>显示更早的 {Math.min(150, history.length - historyCount)} 条消息</button>
              {/if}
              {#each visibleHistory as message, i (history.length - visibleHistory.length + i)}
                <MessageView {message}/>
              {/each}
              {#if running}
                <article class="message user draft">
                  <div class="avatar user-avatar"><span>你</span></div>
                  <div class="message-body"><div class="message-author">你</div><p>{draftPrompt}</p></div>
                </article>
                <article class="message">
                  <div class="avatar agent-avatar"><Sparkles size={17}/></div>
                  <div class="message-body">
                    <div class="message-author">Kroma <span class="streaming-status"><span class="live-dot"></span> 正在处理</span></div>
                    {#if thinking}
                      <details class="reasoning-block">
                        <summary><BrainCircuit size={15}/> 思考过程</summary><div class="reasoning-text">{thinking}</div>
                      </details>
                    {/if}
                    {#if stream}<div class="stream-plain">{stream.length > 160000 ? "[较早的流式片段将在完成后从会话加载]\n" : ""}{stream.slice(-160000)}</div>{:else}<div class="working"><span class="typing-dot"></span><span class="typing-dot"></span><span class="typing-dot"></span></div>{/if}
                    {#each activeTools as tool (tool.id)}
                      <details class="tool-result" class:failed={tool.failed}>
                        <summary><Terminal size={15}/><span>{tool.name} · {tool.done ? (tool.failed ? '失败' : '完成') : '执行中'}</span></summary>
                        <pre>{tool.output || tool.result || tool.args || '等待执行…'}</pre>
                      </details>
                    {/each}
                  </div>
                </article>
              {/if}
            </div>
          {/if}
        </div>
        {#if history.length > 20}
          <button class="scroll-latest" title="滚动到底部" onclick={scrollDown}><ArrowDown size={17}/></button>
        {/if}
        <div class="composer-outer">
          <div class="composer">
            {#if attached.length}
              <div class="draft-files">
                {#each attached as file, i}
                  <span class="file-chip"><FileText size={14}/>{file.split(/[\\/]/).pop()}<button title="移除附件" onclick={() => attached = attached.filter((_,j) => j !== i)}><X size={14}/></button></span>
                {/each}
              </div>
            {/if}
            <textarea bind:value={prompt} onkeydown={handleEditor}
              placeholder={running ? '输入指令以调整当前任务…' : data?.ready ? '向 Kroma 描述你的任务…' : '配置模型后开始对话'}
              rows="3" aria-label="输入任务"></textarea>
            <div class="composer-actions">
              <div class="composer-left">
                <button class="icon-button" title="添加图片或文件" disabled={running || !data?.ready} onclick={attachFile}><Paperclip size={17}/></button>
                <span class="composer-tip">{running ? 'Enter 插入指令' : 'Enter 发送 · Shift+Enter 换行'}</span>
              </div>
              <div class="composer-right">
                {#if running}
                  <button class="stop-button" title="取消当前任务" onclick={cancelTask}><Square size={14} fill="currentColor"/> 停止</button>
                  {#if prompt.trim()}<button class="send-button" title="插入指令" onclick={submit}><Send size={16}/></button>{/if}
                {:else}
                  <button class="send-button" title="发送" disabled={!canSend} onclick={submit}><Send size={17}/></button>
                {/if}
              </div>
            </div>
          </div>
          <div class="footer-metrics">
            <span><Columns3 size={13}/> {snapshot ? compact(snapshot.context_window) + ' ctx' : 'Context —'}</span>
            <span>输入 {compact(usage?.input)}</span><span>输出 {compact(usage?.output)}</span>
            <span>缓存 {cacheRate()}</span>
            {#if running}<span class="footer-working"><span class="live-dot"></span> 正在运行</span>{/if}
          </div>
        </div>
      </div>
    {/if}
  </main>
</div>
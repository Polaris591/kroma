// Synthetic Desktop Host used only by browser checks; never imported by the app.
export async function installDesktopFixture(page, options = {}) {
  await page.addInitScript((options) => {
    if (window.parent !== window) return;
    const usage = () => ({ calls: 0, input: 0, output: 0, cached: 0, reasoning: 0 });
    const profiles = ['everyday', 'thinking', 'local', 'fast', 'custom'].map((name) => ({
      name, model: name === 'everyday' ? 'Everyday' : `Model · ${name}`,
      base_url: 'https://example.invalid/v1', has_key: false, protocol: 'chat',
      reasoning: 'auto', deepseek_thinking: 'off', context_window: 200000,
      max_output_tokens: null, native_web_search: false,
    }));
    if (options.existingKey) profiles[0].has_key = true;
    const project = 'D:/Kroma/Design-Sandbox';
    // Mutable Git workspace state; a browser check can rewrite entries and diffs
    // between calls to model a repository changing on disk.
    const workspace = {
      is_repo: true, message: null, entries: [], diffs: {},
      ...(options.changes ? {
        entries: [{ path: 'src/App.svelte', status: 'modified', staged: false, untracked: false }],
      } : {}),
      ...options.workspace,
    };
    const sessions = [
      { id: 'design', name: '让想法自然发生', recent: true, updated_at: 1794416400 },
      { id: 'review', name: '整理项目的下一步', recent: true, updated_at: 1794330000 },
      { id: 'notes', name: '一些还没完成的想法', recent: true, updated_at: 1794243600 },
    ];
    // Per-session turn snapshots and per-turn file lists, so a late answer can
    // be told apart from the one that belongs to the visible conversation.
    const turnReview = structuredClone(options.turnReview ?? {});
    function checkpointAnswer(session) {
      const configured = turnReview[session]?.checkpoints;
      if (configured) return structuredClone(configured);
      return options.changes ? {
        turns: [{ turn: 0, anchor: 0 }, { turn: 1, anchor: 2 }],
        diffs: [{ turn: 1, files: 1, added: 1, removed: 1, truncated: false }],
      } : { turns: [], diffs: [] };
    }
    function turnFilesAnswer(session, turn) {
      const configured = turnReview[session]?.files?.[turn];
      if (configured) return structuredClone(configured);
      return [{ path: 'src/App.svelte', status: 'M', added: 1, removed: 1 }];
    }
    const messages = [
      { role: 'user', content: '帮我把这个项目的界面整理得更清爽。' },
      { role: 'assistant', content: '可以。先让内容成为主角，把不常用的信息放到需要时再展开。' },
      { role: 'user', content: '保留功能，但让信息更有层次。' },
      { role: 'assistant', content: '我先检查一下界面的结构。', tool_calls: [
        { id: 'read-layout', function: { name: 'read_file', arguments: '{"path":"src/App.svelte"}' } },
      ] },
      { role: 'tool', tool_call_id: 'read-layout', content: 'Layout inspected. Existing controls retained.' },
      { role: 'assistant', reasoning_content: '保持已有功能和插件边界，只调整呈现方式。', content: '## 少一点干扰，多一点空间\n\n界面应该让你专注在正在做的事上，而不是不断提醒你它有多少功能。\n\n- **对话更自然**：提问与回答清楚分开，阅读不再像翻日志。\n- **能力按需出现**：工具、思考和文件改动仍然可查，但不挤占正文。\n- **输入始终顺手**：附件、插话、排队和停止都保留。\n\n先从每天都会用到的地方开始，让 Kroma 安静一点，也更像你自己的工作空间。' },
    ];
    // A conversation turn whose only content is an attachment, so a browser
    // check can prove that a history image and a plain file stay distinct.
    if (options.historyImage) {
      messages.push({ role: 'user', content: '', provider_items: [
        { type: 'kroma_attachment', kind: 'image', name: '历史的截图.png', mime: 'image/png',
          path: `C:/Users/Example/.kroma/attachments/${options.historyImage}`, bytes: 68 },
        { type: 'kroma_attachment', kind: 'file', name: '报告.pdf',
          path: `C:/Users/Example/.kroma/attachments/${options.historyImage}.pdf`, bytes: 2048 },
      ] });
      messages.push({ role: 'assistant', content: '这是那时的截图。' });
    }
    // Full conversations the window may page through. The snapshot only holds
    // the newest window, exactly like the native host.
    const histories = new Map();
    function applyWindow(sessionId) {
      const entry = histories.get(sessionId);
      if (!entry) return;
      const total = entry.full.length;
      const start = Math.max(0, total - (entry.windowSize ?? 120));
      state.snapshot.messages = structuredClone(entry.full.slice(start));
      state.snapshot.total_messages = total;
      state.snapshot.history_truncated = start > 0;
      state.snapshot.next_before = start > 0 ? start : null;
    }
    const state = {
      project, default_project: 'D:/Kroma/Personal', user_home: 'C:/Users/Example',
      // The renderer uses this flag to require a per-conversation backend.
      session_runtime: true,
      ready: true, running: false, connected_model: 'Everyday', connected_profile: 'everyday',
      reasoning_level: 'auto', thinking_levels: ['off', 'low', 'medium', 'high'],
      profiles: { active: 'everyday', profiles }, sessions,
      session_groups: [
        { project, is_default: false, sessions },
        { project: 'D:/Kroma/Personal', is_default: true, sessions: [{ id: 'personal', name: '随手记下', recent: true }] },
        ...(options.sessionGroups ?? []),
      ],
      snapshot: {
        session_id: 'design', messages: options.empty ? [] : messages,
        usage: { coding: usage(), title: usage(), apec: usage(), context_input: 8100, context_estimated: true },
        context_window: 200000, available_input: 183900,
        context: { window: 200000, available_input: 183900, output_reserve: 8000,
          segments: [{ key: 'system', tokens: 1100 }, { key: 'tools', tokens: 2600 }, { key: 'messages', tokens: 4400 }] },
        tool_names: ['read_file', 'write_file', 'shell'], skills: [],
      },
      plugins: options.plugins ?? [], plugin_warnings: [],
    };
    for (const [id, entry] of Object.entries(options.histories ?? {})) {
      histories.set(id, {
        full: structuredClone(entry.full),
        windowSize: entry.windowSize ?? 120,
      });
    }
    // One message list per conversation. A session id alone is not enough:
    // two workspaces may hold the same id and must stay separate.
    const sessionKey = (forProject, forSession) => `${forProject}::${forSession}`;
    const sessionMessages = new Map();
    sessionMessages.set(sessionKey(project, 'design'), state.snapshot.messages);
    function messagesFor(forProject, forSession) {
      const entry = histories.get(forSession);
      if (entry) return entry.full;
      const key = sessionKey(forProject, forSession);
      if (!sessionMessages.has(key)) sessionMessages.set(key, []);
      return sessionMessages.get(key);
    }
    function applySession(forProject, forSession) {
      const entry = histories.get(forSession);
      const full = messagesFor(forProject, forSession);
      const total = full.length;
      const start = Math.max(0, total - (entry?.windowSize ?? 120));
      state.snapshot = {
        ...state.snapshot, session_id: forSession,
        messages: structuredClone(full.slice(start)),
        total_messages: total,
        history_truncated: start > 0,
        next_before: start > 0 ? start : null,
      };
    }
    if (histories.has(state.snapshot.session_id)) applyWindow(state.snapshot.session_id);
    const presentation = {
      tool_labels: {}, activity_frames: [], statuses: [], shortcuts: [], panels: [],
      colors: {}, composer_max_rows: null, widgets: [], desktop_surfaces: [],
      ...options.presentation,
    };
    const savedCapabilities = JSON.parse(sessionStorage.getItem('fixture-capabilities') || 'null');
    const defaults = savedCapabilities?.defaults ?? {tools: {}, mcp: {}, skills: {}, plugins: {}};
    const overrides = savedCapabilities?.overrides ?? {};
    const inventory = {
      tools: ['read_file', 'write_file', 'shell'].map(id => ({id, name: id, source: 'builtin'})),
      mcp: ['sample-server', 'other-server'].map(id => ({id, name: id, description: '本地程序', source: 'project'})),
      skills: ['sample-skill', 'other-skill'].map(id => ({id, name: id, description: '合成技能', source: 'project'})),
      plugins: state.plugins.map(plugin => ({id: plugin.id, name: plugin.name, description: plugin.description, source: 'user'})),
    };
    const capabilities = () => {
      const session = overrides[state.snapshot.session_id] ?? {};
      const items = Object.fromEntries(Object.entries(inventory).map(([group, entries]) => [group, entries.map(entry => {
        const default_enabled = defaults[group][entry.id] ?? (group === 'plugins' ? state.plugins.find(plugin => plugin.id === entry.id).enabled : true);
        const session_enabled = session[group]?.[entry.id] ?? null;
        return {...entry, default_enabled, session_enabled, effective_enabled: session_enabled ?? default_enabled};
      })]));
      const effective = Object.fromEntries(Object.entries(items).map(([group, entries]) => [group, !entries.length || entries.some(entry => entry.effective_enabled)]));
      state.snapshot.capabilities = effective;
      state.snapshot.tool_names = items.tools.filter(item => item.effective_enabled).map(item => item.id);
      return {items, effective, warnings: []};
    };
    let sequence = 0;
    let pasted = 0;
    const callbacks = new Map();
    const listeners = new Map();
    let active = null;
    const actives = new Map();
    const held = new Map();
    const fileCalls = new Map();
    // A hold covers the next matching call: the same command or query asked
    // again is treated as a new call, not a second waiter on the old gate.
    // A numeric turn names one turn, falling back to a hold on the command.
    function gateFor(command, args) {
      const specific = typeof args.query === 'string' ? `${command}:${args.query}`
        : typeof args.turn === 'number' ? `${command}:${args.turn}` : null;
      for (const key of specific === null ? [command] : [specific, command]) {
        const gate = held.get(key);
        if (!gate || gate.used) continue;
        gate.used = true;
        return gate;
      }
      return null;
    }
    function nextFileCall(query) {
      const key = String(query ?? '');
      const call = (fileCalls.get(key) ?? 0) + 1;
      fileCalls.set(key, call);
      return call;
    }

    const fixture = {
      state, presentation, calls: [], failures: {}, workspace, turnReview,
      // Image bytes the next preview_command answers with, keyed by path. A
      // path with no entry is reported as missing, exactly like the backend.
      previews: {},
      // Error messages the next submit refuses the input with; the input never
      // reaches the session, which is what `input_rejected` means.
      rejections: [],
      previewImage(path, bytes) { fixture.previews[path] = Array.from(bytes); },
      rejectInput(message = 'Attachment could not be imported') { fixture.rejections.push(message); },
      async present(value) {
        const url = performance.getEntriesByType('resource')
          .find(entry => new URL(entry.name).pathname === '/src/lib/plugin-ui.ts')?.name;
        if (!url) throw new Error('The app plugin UI module has not loaded');
        const ui = await import(url);
        ui.applyPresentation(value);
      },
      storage: [{ project, sessions: 4, empty: 1, bytes: 2048, empty_ids: ['empty-fixture'] }],
      // The paths the next native open dialog answers with; null is a cancel.
      dialog: options.dialog ?? null,
      // The destination the next native save dialog answers with; null cancels.
      dialogSave: options.dialogSave ?? null,
      searchHits: options.searchHits ?? [],
      attachmentStates: options.attachmentStates ?? {},
      savedAs: [], backups: [], imports: [], queuedPrepared: [], answers: [],
      saveMissing: false,
      importOffline: false,
      backupFailure: null,
      importFailure: null,
      // Replace a conversation with a full history and window the snapshot.
      setHistory(sessionId, full, windowSize = 120) {
        histories.set(sessionId, {full: structuredClone(full), windowSize});
        if (state.snapshot.session_id === sessionId) applyWindow(sessionId);
      },
      hold(command, query) {
        let resolve, reject;
        const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
        held.set(query === undefined ? command : `${command}:${query}`, { promise, resolve, reject, used: false });
      },
      release(command, error, query) {
        const key = query === undefined ? command : `${command}:${query}`;
        const gate = held.get(key);
        if (!gate) throw new Error(`No held call for ${key}`);
        held.delete(key);
        if (error) gate.reject(new Error(error)); else gate.resolve();
      },
      emit(payload) {
        for (const { event, handler } of listeners.values()) {
          if (event === 'plugin_ui') callbacks.get(handler)?.({ event, payload });
        }
      },
      stream(message) {
        if (active) callbacks.get(active.channel.id)?.({ index: active.index++, message });
      },
      streamTo(forProject, forSession, message) {
        const entry = actives.get(sessionKey(forProject, forSession));
        if (entry) callbacks.get(entry.channel.id)?.({ index: entry.index++, message });
      },
      finishSession(forProject, forSession, status = 'completed', error = null) {
        const key = sessionKey(forProject, forSession);
        const entry = actives.get(key);
        if (!entry) return;
        const full = messagesFor(forProject, forSession);
        full.push({ role: 'assistant', content: status === 'completed' ? '已完成本次界面检查。' : '本次任务已停止。' });
        const snapshot = state.project === forProject && state.snapshot.session_id === forSession
          ? {...structuredClone(state.snapshot), messages: structuredClone(full), total_messages: full.length}
          : {...structuredClone(state.snapshot), session_id: forSession,
              messages: structuredClone(full), total_messages: full.length};
        if (state.project === forProject && state.snapshot.session_id === forSession) {
          state.snapshot.messages = full;
          state.snapshot.total_messages = full.length;
        }
        const result = error ? { status, error } : { status };
        callbacks.get(entry.channel.id)?.({ index: entry.index++, message: { kind: 'done', project: forProject, session: forSession, result } });
        callbacks.get(entry.channel.id)?.({ index: entry.index++, message: { kind: 'snapshot', project: forProject, session: forSession, snapshot } });
        actives.delete(key);
        if (active === entry) active = null;
        entry.resolve({ project: forProject, session: forSession, result, snapshot });
      },
      finish(status = 'completed', error = null) {
        if (!active) return;
        fixture.finishSession(active.project, active.session, status, error);
      },
      // The window asks to close exactly as the title bar X and Alt+F4 do: the
      // real @tauri-apps/api listener runs, including its destroy() call.
      async closeRequested() {
        for (const { event, handler } of [...listeners.values()]) {
          if (event !== 'tauri://close-requested') continue;
          await callbacks.get(handler)?.({ event, id: -1, payload: null });
        }
      },
    };
    window.__KROMA_UI_FIXTURE__ = fixture;
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
      transformCallback(callback, once = false) {
        const id = ++sequence;
        callbacks.set(id, (...args) => { if (once) callbacks.delete(id); return callback(...args); });
        return id;
      },
      unregisterCallback(id) { callbacks.delete(id); },
      convertFileSrc(file) { return file; },
      async invoke(command, args = {}) {
        fixture.calls.push({ command, args });
        // The answer belongs to the conversation that asked, even when a hold
        // delivers it after the visible session has already changed.
        const sessionAtCall = state.snapshot.session_id;
        const gate = gateFor(command, args);
        const fileCall = command === 'project_files' ? nextFileCall(args.query) : 0;
        if (gate) await gate.promise;
        if (fixture.failures[command]) {
          const message = fixture.failures[command];
          delete fixture.failures[command];
          throw new Error(message);
        }
        if (command === 'capability_settings') return structuredClone(capabilities());
        if (command === 'set_capability_items') {
          if (active) throw new Error('请先停止任务');
          const target = args.scope === 'defaults' ? defaults : (overrides[state.snapshot.session_id] ??= {});
          target[args.group] ??= {};
          for (const [id, enabled] of Object.entries(args.items)) {
            if (!inventory[args.group].some(item => item.id === id)) throw new Error('不存在的能力');
            if (args.scope === 'defaults' && enabled === null) throw new Error('默认值不能为空');
            if (enabled === null) delete target[args.group][id];
            else target[args.group][id] = enabled;
            if (args.group === 'plugins' && args.scope === 'defaults') state.plugins.find(plugin => plugin.id === id).enabled = enabled;
          }
          sessionStorage.setItem('fixture-capabilities', JSON.stringify({defaults, overrides}));
          return structuredClone(capabilities());
        }
        if (command === 'new_session') {
          const id = `new-${++sequence}`;
          sessionMessages.set(sessionKey(state.project, id), []);
          applySession(state.project, id);
          state.sessions = [{id, name: '新会话', recent: true}, ...state.sessions];
          const group = state.session_groups.find(entry => entry.project === state.project);
          if (group) group.sessions = [{id, name: '新会话', recent: true}, ...group.sessions];
          capabilities();
          return structuredClone(state);
        }
        if (command === 'switch_session') {
          const forProject = args.project || state.project;
          applySession(forProject, args.id);
          capabilities();
          return {project: forProject, session: args.id, snapshot: structuredClone(state.snapshot),
            connected_model: state.connected_model, reasoning_level: state.reasoning_level,
            thinking_levels: state.thinking_levels};
        }
        if (command === 'open_project') {
          state.project = args.path;
          const group = state.session_groups.find(entry => entry.project === args.path);
          const first = group?.sessions?.[0]?.id ?? 'unknown';
          applySession(args.path, first);
          state.ready = true;
          capabilities();
          return structuredClone(state);
        }
        if (command === 'bootstrap' || command === 'connect') {
          capabilities();
          if (args.profile) { state.connected_profile = args.profile; state.connected_model = state.profiles.profiles.find(p => p.name === args.profile)?.model; }
          return structuredClone(state);
        }
        if (command === 'plugin_presentation') {
          const current = capabilities();
          if (!current.effective.plugins) return {tool_labels: {}, activity_frames: [], statuses: [], shortcuts: [], panels: [], colors: {}, composer_max_rows: null, widgets: [], desktop_surfaces: []};
          const result = structuredClone(presentation);
          if (state.plugins.length) {
            const activeIds = new Set(current.items.plugins.filter(item => item.effective_enabled).map(item => item.id));
            result.desktop_surfaces = result.desktop_surfaces.filter(surface => activeIds.has(surface.plugin_id));
            result.widgets = result.widgets.filter(widget => activeIds.has(widget.plugin_id));
            result.panels = result.panels.filter(panel => activeIds.has(panel.id));
          }
          return result;
        }
        if (command === 'save_profile') {
          const { api_key, clear_key, ...input } = args.profile;
          const previous = state.profiles.profiles.find(p => p.name === input.name);
          const next = { ...profiles[0], ...previous, ...input,
            has_key: clear_key ? false : api_key ? true : previous?.has_key ?? false };
          if (previous) Object.assign(previous, next); else state.profiles.profiles.push(next);
          return structuredClone(state.profiles);
        }
        if (command === 'set_default_profile') { state.profiles.active = args.name; return structuredClone(state.profiles); }
        if (command === 'storage_report') return structuredClone(fixture.storage);
        if (command === 'delete_session') {
          state.sessions = state.sessions.filter(s => s.id !== args.id);
          for (const group of state.session_groups) if (group.project === args.project) group.sessions = group.sessions.filter(s => s.id !== args.id);
          for (const row of fixture.storage) {
            row.empty_ids = row.empty_ids.filter(id => id !== args.id);
            row.empty = row.empty_ids.length;
          }
          return structuredClone(state);
        }
        if (command === 'plugin_action') {
          if ((args.action === 'enable' || args.action === 'disable') && args.id) {
            const enabled = args.action === 'enable';
            state.plugins.find(plugin => plugin.id === args.id).enabled = enabled;
            defaults.plugins[args.id] = enabled;
            sessionStorage.setItem('fixture-capabilities', JSON.stringify({defaults, overrides}));
          }
          if (args.action === 'uninstall' && args.id) state.plugins = state.plugins.filter(plugin => plugin.id !== args.id);
          if (args.action === 'set' && args.id) {
            const plugin = state.plugins.find(p => p.id === args.id);
            if (plugin) plugin.settings[args.key] = args.value;
          }
          return { message: '配置已更新', restart_required: false, plugins: structuredClone(state.plugins) };
        }
        if (command === 'checkpoint_turns') return checkpointAnswer(sessionAtCall);
        if (command === 'turn_files') return turnFilesAnswer(sessionAtCall, args.turn);
        if (command === 'turn_file_diff') return { path: args.path,
          diff: turnReview[sessionAtCall]?.diffs?.[args.turn]?.[args.path] ?? '@@ -1 +1 @@\n-old\n+new\n', truncated: false };
        if (command === 'workspace_file_diff') return {
          path: args.path, truncated: false, message: null,
          diff: workspace.diffs[args.path] ?? '@@ -1 +1 @@\n-old\n+new\n',
        };
        if (command === 'rewind_turn') return { saved: 'synthetic-branch', view: structuredClone(state) };
        if (command === 'plugin_panel_view') return { title: '合成插件面板', blocks: [] };
        if (command === 'workspace_changes') return { is_repo: workspace.is_repo, root: project,
          entries: structuredClone(workspace.entries), truncated: false, message: workspace.message };
        if (command === 'set_reasoning') { state.reasoning_level = args.level; return structuredClone(state); }
        if (command === 'search_sessions') {
          // Same rule as the native scan: only visible text containing the
          // query is a hit. A stale result set can never leak another query.
          const query = String(args.query ?? '').toLowerCase();
          return {hits: structuredClone(fixture.searchHits
            .filter(hit => hit.snippet.toLowerCase().includes(query)))};
        }
        if (command === 'inspect_attachments') {
          return {entries: args.paths.map(path => ({path, state: fixture.attachmentStates[path] ?? 'ok'}))};
        }
        if (command === 'prepare_followup_attachments') {
          const items = args.paths.map(path => {
            const suffix = path.slice(path.lastIndexOf('.'));
            const name = path.split(/[\\/]/).pop();
            const stored = `C:/Users/Example/.kroma/attachments/snap-${fixture.queuedPrepared.length + 1}${suffix}`;
            fixture.queuedPrepared.push(stored);
            return {path: stored, name, kind: /\.(png|jpe?g|webp)$/i.test(path) ? 'image' : 'file', bytes: 3};
          });
          return {items};
        }
        if (command === 'save_attachment_as') {
          if (fixture.saveMissing) throw new Error(`附件不存在：${args.source}`);
          fixture.savedAs.push({source: args.source, destination: args.destination});
          return null;
        }
        if (command === 'export_session_backup') {
          if (fixture.backupFailure) {
            const message = fixture.backupFailure;
            fixture.backupFailure = null;
            throw new Error(message);
          }
          fixture.backups.push({id: args.id, path: args.path});
          return {title: '备份标题', messages: state.snapshot.total_messages ?? state.snapshot.messages.length,
            attachments: 1, external_references: 0};
        }
        if (command === 'import_session_backup') {
          fixture.imports.push(args.path);
          if (fixture.importFailure) {
            const message = fixture.importFailure;
            fixture.importFailure = null;
            throw new Error(message);
          }
          state.snapshot.session_id = 'imported';
          state.snapshot.messages = [
            {role: 'system', content: 'restored system'},
            {role: 'user', content: '导入的问题'},
            {role: 'assistant', content: '导入的回答'},
          ];
          state.snapshot.total_messages = 3;
          state.snapshot.history_truncated = false;
          state.snapshot.next_before = null;
          state.sessions.unshift({id: 'imported', name: '导入的会话', recent: true});
          const group = state.session_groups.find(entry => entry.project === project);
          group?.sessions.unshift({id: 'imported', name: '导入的会话', recent: true});
          const result = {...structuredClone(state), imported: {
            id: 'imported', title: '导入的标题', messages: 3, attachments: 1,
          }};
          if (fixture.importOffline) {
            // A stored conversation the window has no runtime for: importing
            // succeeded, opening did not.
            result.ready = false;
            result.open_warning = '已导入会话，但尚未连接模型；连接后可在会话列表打开';
          }
          return result;
        }
        if (command === 'history_page') {
          const full = messagesFor(state.project, state.snapshot.session_id);
          const total = full.length;
          const end = Math.min(typeof args.before === 'number' ? args.before : total, total);
          const limit = Math.min(args.limit ?? 200, 1000);
          const start = Math.max(0, end - limit);
          return {
            session_id: state.snapshot.session_id,
            messages: structuredClone(full.slice(start, end)),
            total_messages: total,
            history_truncated: start > 0,
            next_before: start > 0 ? start : null,
          };
        }
        // The call number makes a repeated query distinguishable per request.
        if (command === 'project_files') return [`src/${args.query || 'root'}-${fileCall}.ts`];
        if (command === 'send_message') {
          if (fixture.rejections.length) {
            // Nothing is written to the session and no channel message is sent.
            const error = fixture.rejections.shift();
            return { result: { status: 'failed', error, input_rejected: true }, snapshot: structuredClone(state.snapshot) };
          }
          const forProject = args.project || state.project;
          const forSession = args.session || state.snapshot.session_id;
          const full = messagesFor(forProject, forSession);
          full.push({ role: 'user', content: args.text });
          if (state.project === forProject && state.snapshot.session_id === forSession) {
            state.snapshot.messages = full;
            state.snapshot.total_messages = full.length;
          }
          const key = sessionKey(forProject, forSession);
          return new Promise(resolve => {
            const entry = { resolve, channel: args.channel, index: 0, project: forProject, session: forSession };
            actives.set(key, entry);
            active = entry;
          });
        }
        if (command === 'cancel_task') {
          fixture.finishSession(args.project || state.project, args.session || state.snapshot.session_id, 'cancelled');
          return null;
        }
        if (command === 'answer_question') {
          fixture.answers.push({ project: args.project, session: args.session, id: args.id, dismissed: args.dismissed });
          return null;
        }
        if (command === 'steer_task') return { status: 'queued' };
        if (command === 'plugin:event|listen') { const id = ++sequence; listeners.set(id, args); return id; }
        if (command === 'plugin:event|unlisten') { listeners.delete(args.eventId); return null; }
        if (command === 'plugin:window|is_focused') return true;
        if (command === 'plugin:window|is_maximized') return false;
        // A real close request ends with the runtime asking the window to close,
        // which the JS listener turns into destroy() unless it is prevented.
        if (command === 'plugin:window|close') return fixture.closeRequested();
        if (command === 'plugin:notification|is_permission_granted') return true;
        if (command === 'save_pasted_image') return `C:/Users/Example/AppData/Local/Temp/kroma-paste/paste-${++pasted}.${args.extension}`;
        if (command === 'preview_image') {
          const bytes = fixture.previews[args.path];
          if (!bytes) throw new Error(`附件不存在：${args.path}`);
          return new Uint8Array(bytes).buffer;
        }
        if (command === 'plugin:dialog|open') return fixture.dialog;
        if (command === 'plugin:dialog|save') return fixture.dialogSave;
        if (command === 'plugin_desktop_bundle') return {
          plugin_id: args.id, title: '合成插件面板', slot: 'panel',
          html: '<!doctype html><html><body style="color:#e8e8e5;background:#202321;font:14px system-ui;padding:16px">插件仍可自由绘制自己的界面。<button>插件按钮</button></body></html>',
        };
        return null;
      },
    };
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener(id) { listeners.delete(id); } };
  }, options);
}

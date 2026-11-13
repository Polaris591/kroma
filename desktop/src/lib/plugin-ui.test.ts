import { describe, expect, it, vi } from 'vitest';
import { api, type PluginPresentation } from './api';
import { get } from 'svelte/store';
import {
  activityGlyph, applyPluginUiEvent, applyPresentation, applySnapshotPluginUi, clearSessionPluginUi, livePluginPanel, loadPresentation,
  pluginPanels, pluginShortcuts, pluginStatuses, pluginThemeStyle, pluginWidgets,
  composerCap, slotColor, toolLabel, toolLabels, shortcutFor,
  EMPTY_LAYOUT, layoutStyle, normalizeLayout,
} from './plugin-ui';

describe('plugin presentation', () => {
  it('clears session-scoped plugin UI on clearSessionPluginUi while leaving global definitions intact', () => {
    applyPresentation({
      tool_labels: {'todo_add': '添加待办'}, activity_frames: [], statuses: ['任务进行中'],
      widgets: [{
        plugin_id: 'todo', id: 'list', placement: 'sidebar', title: 'Todo',
        blocks: [{type: 'text', text: 'item'}], text: 'item', actions: [],
      }],
      desktop_surfaces: [], live_panel: {
        plugin_id: 'todo', title: 'Panel', blocks: [], text: 'hi', actions: [],
      },
      shortcuts: [], panels: [], colors: {}, composer_max_rows: null,
    });
    clearSessionPluginUi();
    expect(get(pluginStatuses)).toEqual([]);
    expect(get(pluginWidgets)).toEqual([]);
    expect(get(livePluginPanel)).toBeNull();
    expect(get(toolLabels)['todo_add']).toBe('添加待办');
  });

  it('clears everything when a read fails or nothing is declared', () => {
    applyPresentation({
      tool_labels: {'todo_add': '添加待办'}, activity_frames: ['|', '/'], statuses: ['待办已同步'],
      widgets: [{
        plugin_id: 'todo', id: 'list', placement: 'sidebar', title: 'Todo',
        blocks: [{type: 'text', text: 'item'}], text: 'item', actions: [],
      }],
      desktop_surfaces: [], live_panel: null,
      shortcuts: [{key: 't', command: '/todo'}], panels: [{id: 'todo', title: '待办', dynamic: false}],
      colors: {accent: '#8fc79a'}, composer_max_rows: 4,
    });
    expect(get(toolLabels)['todo_add']).toBe('添加待办');
    expect(get(pluginWidgets)).toHaveLength(1);
    applyPresentation(null);
    expect(get(toolLabels)).toEqual({});
    expect(get(pluginStatuses)).toEqual([]);
    expect(get(pluginWidgets)).toEqual([]);
    expect(get(pluginShortcuts)).toEqual([]);
    expect(get(pluginPanels)).toEqual([]);
  });

  it('applies live plugin UI events without waiting for turn end', () => {
    let notified = '';
    let drafted = '';
    applyPluginUiEvent({kind: 'status_changed', statuses: ['同步 2/4']});
    expect(get(pluginStatuses)).toEqual(['同步 2/4']);
    applyPluginUiEvent(
      {kind: 'notify', plugin_id: 'demo', message: '完成同步', level: 'info'},
      {onNotify: msg => { notified = msg; }},
    );
    expect(notified).toBe('完成同步');
    applyPluginUiEvent(
      {kind: 'composer_set', plugin_id: 'demo', text: '/review ', mode: 'replace'},
      {onComposerSet: text => { drafted = text; }},
    );
    expect(drafted).toBe('/review ');
    applyPluginUiEvent({
      kind: 'panel_opened', plugin_id: 'demo', title: 'Live',
      blocks: [{type: 'text', text: 'hello'}], text: 'hello', actions: [],
    });
    expect(get(livePluginPanel)?.title).toBe('Live');
    applyPluginUiEvent({kind: 'panel_closed', plugin_id: 'demo'});
    expect(get(livePluginPanel)).toBeNull();
  });

  it('shows a declared label and otherwise the tool name itself', () => {
    expect(toolLabel('todo_add', {'todo_add': '添加待办'})).toBe('添加待办');
    expect(toolLabel('shell', {'todo_add': '添加待办'})).toBe('shell');
  });

  it('matches an Alt shortcut only without Ctrl or Cmd', () => {
    const shortcuts = [{key: 't', command: '/todo'}];
    expect(shortcutFor({altKey: true, ctrlKey: false, metaKey: false, key: 'T'}, shortcuts)?.command)
      .toBe('/todo');
    expect(shortcutFor({altKey: false, ctrlKey: false, metaKey: false, key: 't'}, shortcuts)).toBeNull();
    expect(shortcutFor({altKey: true, ctrlKey: true, metaKey: false, key: 't'}, shortcuts)).toBeNull();
    expect(shortcutFor({altKey: true, ctrlKey: false, metaKey: false, key: 'q'}, shortcuts)).toBeNull();
  });

  it('cycles the declared frames and stays empty when none exist', () => {
    expect(activityGlyph(['|', '/', '-'], 4)).toBe('/');
    expect(activityGlyph([], 3)).toBe('');
  });
});

describe('plugin colours and composer height', () => {
  it('accepts only well-formed hex slots', () => {
    expect(slotColor('accent', {accent: '#8fc79a'})).toBe('#8fc79a');
    expect(slotColor('accent', {accent: '#abc'})).toBe('#abc');
    expect(slotColor('accent', {accent: 'red'})).toBeNull();
    expect(slotColor('border', {})).toBeNull();
    expect(pluginThemeStyle({background: '#111111', accent: 'bad'})).toBe(
      '--surface:#111111;color-scheme:dark;--text:#ffffff;--muted:#aaa7b4;--line:#aaa4bc30',
    );
  });

  it('adapts background-only themes without overriding explicit plugin slots', () => {
    expect(pluginThemeStyle({background: '#fff'})).toContain('color-scheme:light');
    expect(pluginThemeStyle({background: '#fff'})).toContain('--text:#211b32');
    const styled = pluginThemeStyle({background: '#fff', text: '#202020', muted: '#666666', border: '#cccccc', accent: '#262060'});
    expect(styled).toBe('--surface:#fff;--line:#cccccc;--text:#202020;--muted:#666666;--plugin-accent:#262060;color-scheme:light;--plugin-accent-ink:#ffffff');
    expect(pluginThemeStyle({accent: 'red;position:fixed', background: 'invalid'})).toBe('');
    expect(pluginThemeStyle({})).toBe('');
  });

  it('caps the composer at the declared rows without exceeding the built-in limit', () => {
    expect(composerCap(3)).toBe(78);
    expect(composerCap(40)).toBe(186);
    expect(composerCap(null)).toBe(186);
    expect(composerCap(0)).toBe(186);
  });
});

describe('plugin desktop layout', () => {
  it('accepts a complete layout and clamps the reading width', () => {
    expect(normalizeLayout({content_width: 620, sidebar: 'hidden', density: 'compact'}))
      .toEqual({content_width: 620, sidebar: 'hidden', density: 'compact'});
    expect(normalizeLayout({content_width: 5000}).content_width).toBe(1400);
    expect(normalizeLayout({content_width: 40}).content_width).toBe(360);
  });

  it('drops anything it cannot use instead of breaking the built-in look', () => {
    expect(normalizeLayout(null)).toEqual(EMPTY_LAYOUT);
    expect(normalizeLayout({content_width: NaN, sidebar: 'aside', density: 'dense'}))
      .toEqual(EMPTY_LAYOUT);
  });

  it('turns a declared width into one custom property', () => {
    expect(layoutStyle({content_width: 640, sidebar: null, density: null}))
      .toBe('--content-width:640px');
    expect(layoutStyle(EMPTY_LAYOUT)).toBe('');
  });
});

describe('plugin presentation request ordering', () => {
  it('ignores an older reply after a newer session presentation', async () => {
    let resolveOld!: (value: PluginPresentation) => void;
    const empty: PluginPresentation = {tool_labels: {}, activity_frames: [], statuses: [], shortcuts: [], panels: [], colors: {}, composer_max_rows: null};
    const mock = vi.spyOn(api, 'pluginPresentation')
      .mockImplementationOnce(() => new Promise(resolve => { resolveOld = resolve; }))
      .mockResolvedValueOnce(empty);
    try {
      const old = loadPresentation();
      await loadPresentation();
      resolveOld({...empty, tool_labels: {stale: '旧会话'}});
      await old;
      expect(get(toolLabels)).toEqual({});
    } finally { mock.mockRestore(); applyPresentation(null); }
  });

  it('clears disabled plugin UI and invalidates an in-flight reply', async () => {
    let resolve!: (value: PluginPresentation) => void;
    const presentation: PluginPresentation = {tool_labels: {stale: '旧插件'}, activity_frames: [], statuses: [], shortcuts: [], panels: [], colors: {}, composer_max_rows: null};
    const mock = vi.spyOn(api, 'pluginPresentation').mockImplementationOnce(() => new Promise(done => { resolve = done; }));
    try {
      applyPresentation(presentation);
      const pending = loadPresentation();
      applySnapshotPluginUi({capabilities: {tools: true, mcp: true, skills: true, plugins: false}} as import('./api').SessionSnapshot);
      resolve(presentation);
      await pending;
      expect(get(toolLabels)).toEqual({});
      expect(get(pluginWidgets)).toEqual([]);
      expect(get(livePluginPanel)).toBeNull();
    } finally { mock.mockRestore(); applyPresentation(null); }
  });
});

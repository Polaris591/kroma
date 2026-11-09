import { describe, expect, it } from 'vitest';
import { get } from 'svelte/store';
import {
  activityGlyph, applyPresentation, pluginPanels, pluginShortcuts, pluginStatuses,
  composerCap, slotColor, toolLabel, toolLabels, shortcutFor,
} from './plugin-ui';

describe('plugin presentation', () => {
  it('clears everything when a read fails or nothing is declared', () => {
    applyPresentation({
      tool_labels: {'todo_add': '添加待办'}, activity_frames: ['|', '/'], statuses: ['待办已同步'],
      shortcuts: [{key: 't', command: '/todo'}], panels: [{id: 'todo', title: '待办', dynamic: false}],
      colors: {accent: '#8fc79a'}, composer_max_rows: 4,
    });
    expect(get(toolLabels)['todo_add']).toBe('添加待办');
    applyPresentation(null);
    expect(get(toolLabels)).toEqual({});
    expect(get(pluginStatuses)).toEqual([]);
    expect(get(pluginShortcuts)).toEqual([]);
    expect(get(pluginPanels)).toEqual([]);
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
  });

  it('caps the composer at the declared rows without exceeding the built-in limit', () => {
    expect(composerCap(3)).toBe(78);
    expect(composerCap(40)).toBe(186);
    expect(composerCap(null)).toBe(186);
    expect(composerCap(0)).toBe(186);
  });
});

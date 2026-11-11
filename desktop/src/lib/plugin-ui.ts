// Plugin-declared presentation and live UI bus state.
// Nothing here knows a plugin by name: a plugin gains a tool label, a status
// line, an indicator, widgets, panels, or a sandboxed desktop slot through the
// generic host contract.
import { writable } from 'svelte/store';
import {
  api, type DesktopSurface, type PluginLivePanel, type PluginPresentation,
  type PluginUiEvent, type PluginWidget, type SessionSnapshot,
} from './api';

const EMPTY: PluginPresentation = {
  tool_labels: {}, activity_frames: [], statuses: [], widgets: [], desktop_surfaces: [],
  live_panel: null, shortcuts: [], panels: [], colors: {}, composer_max_rows: null,
};

export const toolLabels = writable<Record<string, string>>({});
export const activityFrames = writable<string[]>([]);
export const pluginStatuses = writable<string[]>([]);
export const pluginWidgets = writable<PluginWidget[]>([]);
export const desktopSurfaces = writable<DesktopSurface[]>([]);
export const livePluginPanel = writable<PluginLivePanel | null>(null);
export const desktopBusMessage = writable<{plugin_id: string; payload: unknown; seq: number} | null>(null);
export const pluginShortcuts = writable<{key: string; command: string}[]>([]);
export const pluginPanels = writable<{id: string; title: string; dynamic: boolean}[]>([]);
export const pluginColors = writable<Record<string, string>>({});
export const composerMaxRows = writable<number | null>(null);

let desktopMessageSeq = 0;

export function applyPresentation(presentation: PluginPresentation | null) {
  const value = presentation ?? EMPTY;
  toolLabels.set(value.tool_labels ?? {});
  activityFrames.set(value.activity_frames ?? []);
  pluginStatuses.set(value.statuses ?? []);
  pluginWidgets.set(value.widgets ?? []);
  desktopSurfaces.set(value.desktop_surfaces ?? []);
  livePluginPanel.set(value.live_panel ?? null);
  pluginShortcuts.set(value.shortcuts ?? []);
  pluginPanels.set(value.panels ?? []);
  pluginColors.set(value.colors ?? {});
  composerMaxRows.set(typeof value.composer_max_rows === "number" ? value.composer_max_rows : null);
}

export function applySnapshotPluginUi(snapshot: SessionSnapshot | null | undefined) {
  if (!snapshot) return;
  if (Array.isArray(snapshot.plugin_status)) pluginStatuses.set(snapshot.plugin_status);
  if (Array.isArray(snapshot.plugin_widgets)) pluginWidgets.set(snapshot.plugin_widgets);
}

export function clearSessionPluginUi() {
  pluginStatuses.set([]);
  pluginWidgets.set([]);
  livePluginPanel.set(null);
}

export interface PluginUiHostCallbacks {
  onNotify?: (message: string, level: string) => void;
  onComposerSet?: (text: string, mode: string) => void;
}

export function applyPluginUiEvent(event: PluginUiEvent, host?: PluginUiHostCallbacks) {
  switch (event.kind) {
    case 'status_changed':
      pluginStatuses.set(Array.isArray(event.statuses) ? event.statuses : []);
      break;
    case 'widgets_changed':
      pluginWidgets.set(Array.isArray(event.widgets) ? event.widgets : []);
      break;
    case 'notify':
      if (event.message) host?.onNotify?.(event.message, event.level || 'info');
      break;
    case 'composer_set':
      host?.onComposerSet?.(event.text ?? '', event.mode || 'replace');
      break;
    case 'panel_opened':
      livePluginPanel.set({
        plugin_id: event.plugin_id,
        title: event.title,
        blocks: Array.isArray(event.blocks) ? event.blocks : [],
        text: event.text ?? '',
        actions: Array.isArray(event.actions) ? event.actions : [],
      });
      break;
    case 'panel_closed':
      livePluginPanel.update(current =>
        current && current.plugin_id === event.plugin_id ? null : current,
      );
      break;
    case 'desktop_message':
      desktopBusMessage.set({
        plugin_id: event.plugin_id,
        payload: event.payload,
        seq: ++desktopMessageSeq,
      });
      break;
  }
}

/// A broken or missing plugin must never break the interface, so a failed read
/// simply clears the presentation.
export async function loadPresentation() {
  try { applyPresentation(await api.pluginPresentation()); }
  catch { applyPresentation(null); }
}

/// The label a plugin declared for a tool, or the tool's own name.
export function toolLabel(name: string, labels: Record<string, string>) {
  return labels[name] ?? name;
}

/// Alt+letter matches one declared shortcut; the command then runs exactly as
/// if the user had typed it.
export function shortcutFor(event: {altKey: boolean; ctrlKey: boolean; metaKey: boolean; key: string},
  shortcuts: {key: string; command: string}[]) {
  if (!event.altKey || event.ctrlKey || event.metaKey) return null;
  const key = event.key.toLowerCase();
  return shortcuts.find(shortcut => shortcut.key.toLowerCase() === key) ?? null;
}

/// The indicator glyph for the current frame; text when no frames are declared.
export function activityGlyph(frames: string[], tick: number) {
  if (!frames.length) return '';
  return frames[Math.abs(tick) % frames.length];
}

/// A declared slot colour, accepted only as `#rgb` or `#rrggbb`. A malformed
/// value is ignored instead of breaking the stylesheet.
export function slotColor(slot: string, colors: Record<string, string>) {
  const value = (colors[slot] ?? '').trim();
  return /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.test(value) ? value : null;
}

/// Safe global CSS custom property overrides derived from `[ui.colors]`.
export function pluginThemeStyle(colors: Record<string, string>): string {
  const mapping: [string, string][] = [
    ['background', '--surface'],
    ['border', '--line'],
    ['text', '--text'],
    ['muted', '--muted'],
    ['accent', '--plugin-accent'],
  ];
  return mapping
    .map(([slot, cssVar]) => {
      const hex = slotColor(slot, colors);
      return hex ? `${cssVar}:${hex}` : '';
    })
    .filter(Boolean)
    .join(';');
}

/// Composer height cap in pixels: the declared rows, never beyond the built-in
/// limit the composer already uses.
export function composerCap(rows: number | null, fallback = 186, rowPx = 26) {
  if (!rows || rows < 1) return fallback;
  return Math.min(fallback, Math.max(rowPx, rows * rowPx));
}

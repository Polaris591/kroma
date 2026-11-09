// Plugin-declared presentation, loaded once and shared by every component.
// Nothing here knows a plugin by name: a plugin gains a tool label, a status
// line, an indicator or a panel by declaring it in its own manifest.
import { writable } from 'svelte/store';
import { api, type PluginPresentation } from './api';

const EMPTY: PluginPresentation = {
  tool_labels: {}, activity_frames: [], statuses: [], shortcuts: [], panels: [],
  colors: {}, composer_max_rows: null,
};

export const toolLabels = writable<Record<string, string>>({});
export const activityFrames = writable<string[]>([]);
export const pluginStatuses = writable<string[]>([]);
export const pluginShortcuts = writable<{key: string; command: string}[]>([]);
export const pluginPanels = writable<{id: string; title: string; dynamic: boolean}[]>([]);
export const pluginColors = writable<Record<string, string>>({});
export const composerMaxRows = writable<number | null>(null);

export function applyPresentation(presentation: PluginPresentation | null) {
  const value = presentation ?? EMPTY;
  toolLabels.set(value.tool_labels ?? {});
  activityFrames.set(value.activity_frames ?? []);
  pluginStatuses.set(value.statuses ?? []);
  pluginShortcuts.set(value.shortcuts ?? []);
  pluginPanels.set(value.panels ?? []);
  pluginColors.set(value.colors ?? {});
  composerMaxRows.set(typeof value.composer_max_rows === "number" ? value.composer_max_rows : null);
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

/// Composer height cap in pixels: the declared rows, never beyond the built-in
/// limit the composer already uses.
export function composerCap(rows: number | null, fallback = 186, rowPx = 26) {
  if (!rows || rows < 1) return fallback;
  return Math.min(fallback, Math.max(rowPx, rows * rowPx));
}

import { invoke, Channel } from '@tauri-apps/api/core';
import type { SteerOutcome } from './steering';
import type { AskAnswer, AskQuestion } from './ask-question';
import type { PanelBlock, PanelView } from './panel-blocks';

export interface Message {
  role: string;
  content: string | null;
  reasoning_content?: string | null;
  tool_calls?: Array<{id: string; function?: {name: string; arguments: string}}>;
  tool_call_id?: string;
  provider_items?: Array<Record<string, unknown>>;
}
export interface Usage {
  calls: number; input: number; output: number; cached: number; reasoning: number;
}
export interface SessionUsage {
  coding: Usage; title: Usage; apec: Usage; context_input: number | null; context_estimated: boolean;
}
export interface ContextSegment { key: string; tokens: number }
export interface ContextReport {
  window: number; available_input: number; output_reserve: number; segments: ContextSegment[];
}
export interface PluginUiAction {
  id: string;
  label: string;
  primary?: boolean;
  danger?: boolean;
}
export type WidgetPlacement = 'above_composer' | 'below_composer' | 'sidebar' | 'header' | 'footer';
export interface PluginWidget {
  plugin_id: string;
  id: string;
  placement: WidgetPlacement;
  title: string;
  blocks: PanelBlock[];
  text: string;
  actions: PluginUiAction[];
  scope?: 'session' | 'global';
}
export interface DesktopSurface {
  plugin_id: string;
  title: string;
  slot: 'sidebar' | 'tab' | 'above_composer' | 'panel';
  height?: number | null;
}
export interface DesktopBundle {
  plugin_id: string;
  title: string;
  slot: string;
  height?: number | null;
  html: string;
}
export interface PluginLivePanel {
  plugin_id: string;
  title: string;
  blocks: PanelBlock[];
  text: string;
  actions: PluginUiAction[];
}
export type PluginUiEvent =
  | {kind: 'status_changed'; statuses: string[]}
  | {kind: 'widgets_changed'; widgets: PluginWidget[]}
  | {kind: 'notify'; plugin_id: string; message: string; level: string}
  | {kind: 'composer_set'; plugin_id: string; text: string; mode: string}
  | {kind: 'panel_opened'; plugin_id: string; title: string; blocks: PanelBlock[]; text: string; actions: PluginUiAction[]}
  | {kind: 'panel_closed'; plugin_id: string}
  | {kind: 'desktop_message'; plugin_id: string; payload: unknown};

export interface SessionSnapshot {
  session_id: string; messages: Message[]; usage: SessionUsage;
  total_messages?: number; history_truncated?: boolean;
  next_before?: number | null;
  context_window: number; available_input: number; tool_names: string[];
  context?: ContextReport | null;
  skills?: Array<{name: string; description?: string}> | null;
  plugin_status?: string[];
  plugin_widgets?: PluginWidget[];
}
export interface PluginSummary {
  id: string; name: string; version: string; description: string;
  enabled: boolean; project: boolean; settings: Record<string, unknown>;
  secret_settings?: string[];
  /// Sensitive fields that currently have a non-empty value.
  set_secrets?: string[];
  has_model: boolean; has_context: boolean; has_ui: boolean; has_desktop_ui?: boolean;
}
export interface PluginPresentation {
  tool_labels: Record<string, string>;
  activity_frames: string[];
  statuses: string[];
  widgets?: PluginWidget[];
  desktop_surfaces?: DesktopSurface[];
  live_panel?: PluginLivePanel | null;
  shortcuts: {key: string; command: string}[];
  panels: {id: string; title: string; dynamic: boolean}[];
  /// Slot colours a plugin declared, as `#rgb`/`#rrggbb`.
  colors: Record<string, string>;
  /// Height cap a plugin asked for its composer, in rows.
  composer_max_rows: number | null;
}
export interface ProfileSummary {
  name: string; base_url: string; model: string; has_key: boolean;
  protocol: string; reasoning: string; deepseek_thinking: string;
  context_window: number | null; max_output_tokens: number | null;
  native_web_search: boolean;
  thinking_levels?: string[] | null;
}
export interface Profiles {
  active: string | null; profiles: ProfileSummary[];
}
export interface WorkspaceChange {
  path: string;
  status: 'modified' | 'added' | 'deleted' | 'renamed' | 'conflict' | 'untracked';
  staged: boolean;
  untracked: boolean;
}
export interface WorkspaceReview {
  is_repo: boolean;
  root: string | null;
  entries: WorkspaceChange[];
  truncated: boolean;
  message: string | null;
}
export interface WorkspaceFileDiff {
  path: string;
  diff: string;
  truncated: boolean;
  message: string | null;
}
export interface SessionEntry { id: string; name: string | null; recent: boolean; updated_at?: number | null; }
export interface SessionGroup { project: string; is_default: boolean; sessions: SessionEntry[]; }
export interface SessionSearchHit {project: string; id: string; snippet: string;}
export interface McpProjectStatus { configured: boolean; trusted: boolean; }
export interface Bootstrap {
  project: string; recent_projects?: string[]; default_project?: string | null;
  user_home?: string | null;
  session_groups?: SessionGroup[]; ready: boolean; running: boolean;
  connected_model: string | null;
  connected_profile?: string | null;
  reasoning_level?: string | null;
  thinking_levels?: string[] | null;
  snapshot: SessionSnapshot | null; sessions: SessionEntry[];
  profiles: Profiles; plugins: PluginSummary[]; plugin_warnings: string[];
  mcp_project: McpProjectStatus;
}
export interface DesktopEvent {
  kind: string; text?: string; id?: string; name?: string;
  stream?: string; result?: string; failed?: boolean; arguments?: string;
  [key: string]: unknown;
}
export type StreamEnvelope =
  | {kind: 'batch'; events: DesktopEvent[]}
  | {kind: 'snapshot'; snapshot: SessionSnapshot; sessions?: SessionEntry[]; session_groups?: SessionGroup[]}
  // A plugin asks the person mid-run; the composer slot turns into the question.
  | {kind: 'ask'; ask: AskQuestion}
  | {kind: 'done'; result: {status: string; error?: string}};

export type ProfileInput = {
  name: string; base_url: string; model: string; api_key?: string;
  clear_key?: boolean; protocol: string; reasoning: string;
  deepseek_thinking?: string; context_window?: number | null;
  max_output_tokens?: number | null; native_web_search?: boolean;
};

export interface CheckpointTurn { turn: number; anchor: number | null }
export interface TurnDiffSummary { turn: number; added: number; removed: number; files: number; truncated: boolean }
export interface CheckpointTurns { turns: CheckpointTurn[]; diffs: TurnDiffSummary[] }
export interface FileDiff { diff: string; truncated: boolean }

export const api = {
  bootstrap: () => invoke<Bootstrap>('bootstrap'),
  searchSessions: (query: string) => invoke<{hits: SessionSearchHit[]}>('search_sessions', {query}),
  checkpointTurns: () => invoke<CheckpointTurns>('checkpoint_turns'),
  storageReport: () => invoke<Array<{project: string; sessions: number; empty: number; bytes: number; empty_ids: string[]}>>('storage_report'),
  savePastedImage: (bytes: number[], extension: string) => invoke<string>('save_pasted_image', {bytes, extension}),
  projectFiles: (query: string) => invoke<string[]>('project_files', {query}),
  compactContext: (instructions: string) =>
    invoke<{compacted: boolean; before: number; after: number; view: Bootstrap}>('compact_context', {instructions}),
  rewindTurn: (messageIndex: number, snapshotTurn: number | null, restoreFiles: boolean) =>
    invoke<{saved: string; view: Bootstrap}>('rewind_turn', {messageIndex, snapshotTurn, restoreFiles}),
  forkTurn: (messageIndex: number) =>
    invoke<{session_id: string; view: Bootstrap}>('fork_turn', {messageIndex}),
  turnFiles: (turn: number) => invoke<Array<{path: string; status: string; added: number; removed: number}>>('turn_files', {turn}),
  turnFileDiff: (turn: number, path: string) => invoke<FileDiff>('turn_file_diff', {turn, path}),
  workspaceChanges: () => invoke<WorkspaceReview>('workspace_changes'),
  workspaceFileDiff: (path: string) => invoke<WorkspaceFileDiff>('workspace_file_diff', {path}),
  openProject: (path: string) => invoke<Bootstrap>('open_project', {path}),
  connect: (profile?: string) => invoke<Bootstrap>('connect', {profile: profile || null}),
  setReasoning: (level: string) => invoke<Bootstrap>('set_reasoning', {level}),
  saveProfile: (profile: ProfileInput) => invoke<Profiles>('save_profile', {profile}),
  setDefaultProfile: (name: string) => invoke<Profiles>('set_default_profile', {name}),
  historyPage: (before: number, limit = 200) => invoke<SessionSnapshot>("history_page", {before, limit}),
  switchSession: (id: string) => invoke<{snapshot: SessionSnapshot; title?: string | null; connected_model: string | null; reasoning_level: string | null; thinking_levels: string[] | null}>('switch_session', {id}),
  newSession: () => invoke<Bootstrap>('new_session'),
  deleteSession: (project: string, id: string) => invoke<Bootstrap>('delete_session', {project, id}),
  restoreDeletedSession: (project: string, id: string, title: string) => invoke<Bootstrap>('restore_deleted_session', {project, id, title}),
  renameSession: (id: string, title: string) => invoke<Bootstrap>('rename_session', {id, title}),
  exportSession: (id: string, path: string) => invoke<void>('export_session', {id, path}),
  cancel: () => invoke<void>('cancel_task'),
  // The host reports what the text actually did (steer/status/pause/cancel).
  steer: (text: string) => invoke<SteerOutcome>('steer_task', {text}),
  trustProjectMcp: () => invoke<{path: string; trusted: boolean}>('trust_project_mcp'),
  pluginAction: (action: string, id?: string, path?: string, key?: string, value?: unknown) =>
    invoke<{message: string; warning?: string; restart_required: boolean; plugins: PluginSummary[]}>(
      'plugin_action', {action, id: id || null, path: path || null, key: key || null, value: value ?? null}),
  // Plugin-declared presentation. Nothing here names a specific plugin.
  pluginPresentation: () => invoke<PluginPresentation>('plugin_presentation'),
  pluginPanel: (id: string) => invoke<string>('plugin_panel', {id}),
  // Structured panel content; older hosts only know the string form.
  pluginPanelView: (id: string) => invoke<PanelView>('plugin_panel_view', {id}),
  pluginDesktopBundle: (id: string) => invoke<DesktopBundle>('plugin_desktop_bundle', {id}),
  pluginUiAction: (pluginId: string, widgetId: string, actionId: string, payload?: unknown) =>
    invoke<{content: string; task?: string | null}>('plugin_ui_action', {
      pluginId, widgetId, actionId, payload: payload ?? null,
    }),
  pluginUiMessage: (pluginId: string, payload: unknown) =>
    invoke<unknown>('plugin_ui_message', {pluginId, payload}),
  // Answering a plugin's question; `dismissed` means the person closed the bar.
  answerQuestion: (answer: AskAnswer) => invoke<null>('answer_question', {
    id: answer.id, picked: answer.picked, typed: answer.typed, dismissed: answer.dismissed,
  }),
  send: async (text: string, paths: string[], onEvent: (event: StreamEnvelope) => void) => {
    const channel = new Channel<StreamEnvelope>();
    channel.onmessage = onEvent;
    return invoke<{result: {status: string; error?: string; text?: string}; snapshot: SessionSnapshot; sessions?: SessionEntry[]; session_groups?: SessionGroup[]}>(
      'send_message', {text, paths, channel});
  }
};

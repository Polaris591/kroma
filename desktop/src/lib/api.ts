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
export type WidgetPlacement = 'above_composer' | 'below_composer' | 'sidebar' | 'header' | 'footer' | 'transcript';
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
  slot: 'sidebar' | 'tab' | 'above_composer' | 'panel' | 'workspace';
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
  capabilities?: CapabilityFlags;
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
  /// Desktop layout a plugin declared: reading width, sidebar, density.
  layout?: {
    content_width?: number | null;
    sidebar?: string | null;
    density?: string | null;
  } | null;
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
export interface SessionSearchHit {
  project: string; id: string; snippet: string;
  /** Visible text the hit came from, and its absolute message index. */
  role: string; message_index: number;
}
export type CapabilityKey = 'tools' | 'mcp' | 'skills' | 'plugins';
export type CapabilityFlags = Record<CapabilityKey, boolean>;
export interface CapabilityItem {
  id: string;
  name: string;
  description?: string;
  source?: string;
  default_enabled: boolean;
  session_enabled: boolean | null;
  effective_enabled: boolean;
  available?: boolean;
}
export interface CapabilitySettings {
  items: Record<CapabilityKey, CapabilityItem[]>;
  warnings?: string[];
}
export interface SessionState {
  project: string;
  session: string;
  running: boolean;
  waiting: boolean;
  busy: boolean;
}
export interface Bootstrap {
  project: string; recent_projects?: string[]; default_project?: string | null;
  user_home?: string | null;
  session_groups?: SessionGroup[]; ready: boolean; running: boolean;
  /// A backend that owns one context per conversation. An older binary omits
  /// it and would silently switch the wrong session, so the renderer refuses
  /// concurrent per-session actions in that case.
  session_runtime?: boolean; session?: string; waiting?: boolean;
  session_states?: SessionState[];
  connected_model: string | null;
  connected_profile?: string | null;
  reasoning_level?: string | null;
  thinking_levels?: string[] | null;
  snapshot: SessionSnapshot | null; sessions: SessionEntry[];
  profiles: Profiles; plugins: PluginSummary[]; plugin_warnings: string[];
}
export interface DesktopEvent {
  kind: string; text?: string; id?: string; name?: string;
  stream?: string; result?: string; failed?: boolean; arguments?: string;
  [key: string]: unknown;
}
export type StreamEnvelope =
  | {kind: 'batch'; events: DesktopEvent[]; project?: string; session?: string}
  // A new conversation received its real id before its first turn ran.
  | {kind: 'session'; project: string; session: string}
  | {kind: 'snapshot'; project?: string; session?: string; snapshot: SessionSnapshot; sessions?: SessionEntry[]; session_groups?: SessionGroup[]}
  // A plugin asks the person mid-run; the composer slot turns into the question.
  | {kind: 'ask'; session?: string; ask: AskQuestion}
  | {kind: 'done'; project?: string; session?: string; result: TaskResult};

/** The outcome of one submitted task. `input_rejected` is set only when the
 * backend refused the input before any user turn was written (attachments
 * that could not be imported), so the composer may restore the exact prompt.
 * Model, network and IPC failures never carry it. */
export interface TaskResult {
  status: string; error?: string; text?: string; input_rejected?: boolean;
}

/** Which file a read-only image preview may read. A `history` path must
 * resolve inside the private attachment store. */
export type AttachmentOrigin = 'draft' | 'history';

export type ProfileInput = {
  name: string; base_url: string; model: string; api_key?: string;
  clear_key?: boolean; protocol: string; reasoning: string;
  deepseek_thinking?: string; context_window?: number | null;
  max_output_tokens?: number | null; native_web_search?: boolean;
};

/** A draft attachment check after restore; never carries file content. */
export interface InspectedAttachment { path: string; state: string; }
/** A queued task's attachment, already copied into the private store. */
export interface PreparedAttachment {
  path: string; name?: string | null; kind?: string | null; bytes?: number | null;
}
export interface SessionBackupReport {
  title: string; messages: number; attachments: number; external_references: number;
}
export interface ImportedBackupSummary {
  id: string; title: string; messages: number; attachments: number;
}
export type ImportedBootstrap = Bootstrap & {
  imported?: ImportedBackupSummary;
  /// Set when the conversation was stored but this window could not open it.
  open_warning?: string | null;
};

export interface CheckpointTurn { turn: number; anchor: number | null }
export interface TurnDiffSummary { turn: number; added: number; removed: number; files: number; truncated: boolean }
export interface CheckpointTurns { turns: CheckpointTurn[]; diffs: TurnDiffSummary[] }
export interface FileDiff { diff: string; truncated: boolean }

export const api = {
  capabilitySettings: () => invoke<CapabilitySettings>('capability_settings'),
  setCapabilityItems: (scope: 'defaults' | 'session', group: CapabilityKey, items: Record<string, boolean | null>) =>
    invoke<CapabilitySettings>('set_capability_items', {scope, group, items}),
  bootstrap: () => invoke<Bootstrap>('bootstrap'),
  searchSessions: (query: string) => invoke<{hits: SessionSearchHit[]}>('search_sessions', {query}),
  checkpointTurns: () => invoke<CheckpointTurns>('checkpoint_turns'),
  storageReport: () => invoke<Array<{project: string; sessions: number; empty: number; bytes: number; empty_ids: string[]}>>('storage_report'),
  savePastedImage: (bytes: number[], extension: string) => invoke<string>('save_pasted_image', {bytes, extension}),
  // Restored drafts: existence/size only, no content and no Agent lock.
  inspectAttachments: (paths: string[]) =>
    invoke<{entries: InspectedAttachment[]}>('inspect_attachments', {paths}),
  // Queue-time copy into the private attachment store.
  prepareFollowupAttachments: (paths: string[]) =>
    invoke<{items: PreparedAttachment[]}>('prepare_followup_attachments', {paths}),
  // Save one stored history attachment to a destination the person picked.
  saveAttachmentAs: (source: string, destination: string) =>
    invoke<void>('save_attachment_as', {source, destination}),
  // Complete local backup (transcript + context + attachment bytes).
  exportSessionBackup: (id: string, path: string) =>
    invoke<SessionBackupReport>('export_session_backup', {id, path}),
  importSessionBackup: (path: string) => invoke<ImportedBootstrap>('import_session_backup', {path}),
  // Verified PNG/JPEG/WebP bytes for a read-only preview. A missing command
  // means an older Desktop binary, which the caller must say out loud.
  previewImage: async (origin: AttachmentOrigin, path: string) => {
    const bytes = await invoke<ArrayBuffer>('preview_image', {origin, path});
    return new Uint8Array(bytes);
  },
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
  switchSession: (project: string, id: string) => invoke<{project: string; session: string; snapshot: SessionSnapshot; title?: string | null; connected_model: string | null; reasoning_level: string | null; thinking_levels: string[] | null}>('switch_session', {project, id}),
  newSession: () => invoke<Bootstrap>('new_session'),
  deleteSession: (project: string, id: string) => invoke<Bootstrap>('delete_session', {project, id}),
  restoreDeletedSession: (project: string, id: string, title: string) => invoke<Bootstrap>('restore_deleted_session', {project, id, title}),
  renameSession: (id: string, title: string) => invoke<Bootstrap>('rename_session', {id, title}),
  exportSession: (id: string, path: string) => invoke<void>('export_session', {id, path}),
  cancel: (project: string, session: string) => invoke<void>('cancel_task', {project, session}),
  // The host reports what the text actually did (steer/status/pause/cancel).
  steer: (text: string, project: string, session: string) => invoke<SteerOutcome>('steer_task', {text, project, session}),
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
  answerQuestion: (answer: AskAnswer, project: string, session: string) => invoke<null>('answer_question', {
    id: answer.id, picked: answer.picked, typed: answer.typed, dismissed: answer.dismissed, project, session,
  }),
  send: async (text: string, paths: string[], target: {project: string; session: string}, onEvent: (event: StreamEnvelope) => void) => {
    const channel = new Channel<StreamEnvelope>();
    channel.onmessage = onEvent;
    return invoke<{project: string; session: string; result: TaskResult; snapshot: SessionSnapshot; sessions?: SessionEntry[]; session_groups?: SessionGroup[]}>(
      'send_message', {text, paths, channel, project: target.project, session: target.session});
  }
};

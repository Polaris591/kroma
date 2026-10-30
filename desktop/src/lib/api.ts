import { invoke, Channel } from '@tauri-apps/api/core';

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
export interface SessionSnapshot {
  session_id: string; messages: Message[]; usage: SessionUsage;
  context_window: number; available_input: number; tool_names: string[];
}
export interface PluginSummary {
  id: string; name: string; version: string; description: string;
  enabled: boolean; project: boolean; settings: Record<string, unknown>;
  has_model: boolean; has_context: boolean; has_ui: boolean;
}
export interface ProfileSummary {
  name: string; base_url: string; model: string; has_key: boolean;
  protocol: string; reasoning: string; deepseek_thinking: string;
  context_window: number | null; max_output_tokens: number | null;
}
export interface Profiles {
  active: string | null; profiles: ProfileSummary[];
}
export interface SessionEntry { id: string; name: string | null; recent: boolean; }
export interface Bootstrap {
  project: string; ready: boolean; running: boolean;
  snapshot: SessionSnapshot | null; sessions: SessionEntry[];
  profiles: Profiles; plugins: PluginSummary[]; plugin_warnings: string[];
}
export interface DesktopEvent {
  kind: string; text?: string; id?: string; name?: string;
  stream?: string; result?: string; failed?: boolean; arguments?: string;
  [key: string]: unknown;
}
export type StreamEnvelope =
  | {kind: 'batch'; events: DesktopEvent[]}
  | {kind: 'snapshot'; snapshot: SessionSnapshot}
  | {kind: 'done'; result: {status: string; error?: string}};

export type ProfileInput = {
  name: string; base_url: string; model: string; api_key?: string;
  clear_key?: boolean; protocol: string; reasoning: string;
  deepseek_thinking?: string; context_window?: number | null;
  max_output_tokens?: number | null;
};

export const api = {
  bootstrap: () => invoke<Bootstrap>('bootstrap'),
  openProject: (path: string) => invoke<Bootstrap>('open_project', {path}),
  connect: (profile?: string) => invoke<Bootstrap>('connect', {profile: profile || null}),
  saveProfile: (profile: ProfileInput) => invoke<Profiles>('save_profile', {profile}),
  switchSession: (id: string) => invoke<Bootstrap>('switch_session', {id}),
  newSession: () => invoke<Bootstrap>('new_session'),
  renameSession: (title: string) => invoke<Bootstrap>('rename_session', {title}),
  cancel: () => invoke<void>('cancel_task'),
  steer: (text: string) => invoke<void>('steer_task', {text}),
  queue: (text: string) => invoke<number>('queue_followup', {text}),
  pluginAction: (action: string, id?: string, path?: string, key?: string, value?: unknown) =>
    invoke<{message: string; warning?: string; restart_required: boolean; plugins: PluginSummary[]}>(
      'plugin_action', {action, id: id || null, path: path || null, key: key || null, value: value ?? null}),
  send: async (text: string, paths: string[], onEvent: (event: StreamEnvelope) => void) => {
    const channel = new Channel<StreamEnvelope>();
    channel.onmessage = onEvent;
    return invoke<{result: {status: string; error?: string}; snapshot: SessionSnapshot}>(
      'send_message', {text, paths, channel});
  }
};

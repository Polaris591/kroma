import type { DesktopEvent, SessionSnapshot } from './api';

/**
 * The native host publishes one goal projection per committed control state as
 * `kind: "task_control"` with `owner/id/revision/status/reason/summary`
 * (see `DesktopEvents::on_event` in src/desktop_host.rs).
 */
export type GoalStatus = 'running' | 'paused' | 'completed' | 'cancelled' | 'none';

export interface GoalState {
  command?: string;
  owner: string;
  id: string;
  revision: number;
  status: GoalStatus;
  /** Internal reason code from the goal record, never shown verbatim. */
  reason: string;
  summary: string;
}

/** The plugin declares the slash command, but the host never exposes its name
 * to the renderer; `/goal` is the command the desktop documents. */
export const GOAL_COMMAND = '/goal';

const STATUSES: readonly GoalStatus[] = ['running', 'paused', 'completed', 'cancelled', 'none'];

function field(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

/** Read one native goal event, or null when the event is something else. */
export function goalFromEvent(event: DesktopEvent): GoalState | null {
  if (event.kind !== 'task_control') return null;
  const status = field(event.status) as GoalStatus;
  // An unknown status is a newer host: ignore it instead of guessing a state.
  if (!STATUSES.includes(status)) return null;
  const revision = Number(event.revision);
  return {
    ...(field(event.command) ? {command: field(event.command)} : {}),
    owner: field(event.owner),
    id: field(event.id),
    revision: Number.isFinite(revision) ? revision : 0,
    status,
    reason: field(event.reason),
    summary: field(event.summary),
  };
}

/** Snapshot state survives reloads, session switches and lost live events. */
export function goalFromSnapshot(snapshot: SessionSnapshot | null | undefined): GoalState | null {
  return snapshot?.control ? goalFromEvent({...snapshot.control, kind: 'task_control'}) : null;
}

export function goalAllowsFollowup(goal: GoalState | null): boolean {
  return goal?.status !== 'running' && goal?.status !== 'paused';
}

export function goalStatusLabel(status: GoalStatus): string {
  switch (status) {
    case 'running': return '执行中';
    case 'paused': return '已暂停';
    case 'completed': return '已完成';
    case 'cancelled': return '已取消';
    default: return '未开始';
  }
}

const REASONS: Record<string, string> = {
  user_stop: '你手动暂停了这个目标',
  interrupted: '上一轮执行被中断',
  completed: '目标已达成',
  cancelled: '目标已被取消',
  provider_error: '模型服务出错，目标停在此处',
  blocked: '目标被阻塞，需要你处理后继续',
  plugin_error: '目标插件出错，进度已保留，可修复后继续',
  invalid_state: '目标记录无法读取，可补充说明后重新开始',
  scope_changed: '会话范围已改变，原目标不再适用',
  carried_over: '从上一轮会话继承的目标',
};

/** User-readable pause/finish reason. Internal codes stay internal. */
export function goalReasonLabel(reason: string): string {
  if (!reason) return '';
  return REASONS[reason.toLowerCase()] ?? '目标状态发生变化';
}

export function goalNextStep(status: GoalStatus): string {
  switch (status) {
    case 'running': return '目标正在执行；可暂停，暂停后已完成的修改会保留';
    case 'paused': return `发送「${GOAL_COMMAND} resume」从暂停处继续`;
    case 'completed': return `目标已完成；发送「${GOAL_COMMAND} 新指令」开始下一个目标`;
    case 'cancelled': return `目标已取消；发送「${GOAL_COMMAND} 新指令」开始新的目标`;
    default: return `发送「${GOAL_COMMAND} 指令」开始一个目标`;
  }
}

export type GoalActionId = 'pause' | 'resume' | 'cancel';

export interface GoalAction {
  id: GoalActionId;
  label: string;
  hint: string;
}

/** Only states the host can actually act on offer buttons. */
export function goalActions(status: GoalStatus): GoalAction[] {
  if (status === 'running') {
    return [
      { id: 'pause', label: '暂停', hint: '暂停目标；已完成的修改不会回滚' },
      { id: 'cancel', label: '取消', hint: '取消目标；已完成的修改不会被删除' },
    ];
  }
  if (status === 'paused') {
    return [
      { id: 'resume', label: '继续', hint: '从暂停处继续这个目标' },
      { id: 'cancel', label: '取消', hint: '取消目标；已完成的修改不会被删除' },
    ];
  }
  if (status === 'completed' || status === 'cancelled') {
    return [{id: 'resume', label: '重新继续', hint: '按你的要求重新打开这个目标，不自动重复此前操作'}];
  }
  return [];
}

/** The exact control line the host parses for one explicit action. */
export function goalControlLine(action: GoalActionId, command = GOAL_COMMAND): string {
  return `${command} ${action}`;
}

/** One-line summary: id, status, reason and the next step. */
export function goalNotice(goal: GoalState): string {
  const parts = [
    goal.id ? `目标 ${goal.id}` : '当前目标',
    goalStatusLabel(goal.status),
  ];
  const reason = goalReasonLabel(goal.reason);
  if (reason) parts.push(reason);
  return `${parts.join(' · ')}；${goalNextStep(goal.status)}`;
}

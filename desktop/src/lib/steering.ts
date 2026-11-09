import { goalNotice, goalStatusLabel, goalNextStep, type GoalState } from './goal';

/** What a steering request actually did. The native command reports the real
 * outcome, so the composer never claims it inserted an instruction when the
 * text was matched as a `/goal` control command (status, pause or cancel). */
export type SteerOutcome = {kind?: string; message?: string};

/** User-facing notice for one steering result. A status request reports the
 * goal the host published instead of a generic placeholder. */
export function steerNotice(
  outcome: SteerOutcome | null | undefined,
  goal: GoalState | null = null,
): string {
  switch (outcome?.kind) {
    case 'paused':
      return '已请求暂停当前任务';
    case 'cancelled':
      return '已请求取消当前任务，停止后生效';
    case 'status':
      // The host re-emits its committed state on a status request; until that
      // lands, say what is already known instead of claiming success.
      return goal && goal.status !== 'none'
        ? goalNotice(goal)
        : '已刷新当前目标状态，等待宿主回传';
    case 'steered':
      return '已向当前任务插入指令';
    default:
      // An older host that returned no body still steered the task.
      return '已向当前任务插入指令';
  }
}


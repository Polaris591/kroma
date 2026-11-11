/** What a steering request actually did. The native command reports the real
 * outcome, so the composer never claims it inserted an instruction when the
 * text was matched as a control command (status, pause or cancel). */
export type SteerOutcome = {kind?: string; message?: string};

/** User-facing notice for one steering result. */
export function steerNotice(outcome: SteerOutcome | null | undefined): string {
  switch (outcome?.kind) {
    case 'paused':
      return '已请求暂停当前任务';
    case 'cancelled':
      return '已请求取消当前任务，停止后生效';
    case 'status':
      return '已刷新当前任务状态';
    case 'steered':
      return '已向当前任务插入指令';
    default:
      // An older host that returned no body still steered the task.
      return '已向当前任务插入指令';
  }
}


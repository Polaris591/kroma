import {describe, expect, it} from 'vitest';
import {
  GOAL_COMMAND, goalAllowsFollowup, goalFromSnapshot, goalActions, goalControlLine, goalFromEvent, goalNextStep,
  goalNotice, goalReasonLabel, goalStatusLabel, type GoalState,
} from './goal';

const running: GoalState = {
  owner: 'kroma-goal', id: 'goal-7', revision: 3,
  status: 'running', reason: '', summary: '把登录页改成深色',
};

describe('Goal projection', () => {
  it('reads the fields the desktop host actually publishes', () => {
    const goal = goalFromEvent({
      kind:'task_control', owner:'kroma-goal', id:'goal-7', revision:3,
      status:'paused', reason:'user_stop', summary:'把登录页改成深色',
    });
    expect(goal).toEqual({
      owner:'kroma-goal', id:'goal-7', revision:3,
      status:'paused', reason:'user_stop', summary:'把登录页改成深色',
    });
  });

  it('ignores other event kinds and unknown statuses', () => {
    expect(goalFromEvent({kind:'text_delta', text:'ok'})).toBeNull();
    expect(goalFromEvent({kind:'task_control', status:'thinking'})).toBeNull();
    expect(goalFromEvent({kind:'task_control', status:'completed'})).toMatchObject({
      id:'', revision:0, status:'completed', reason:'',
    });
  });

  it('renders every status in Chinese', () => {
    expect(goalStatusLabel('running')).toBe('执行中');
    expect(goalStatusLabel('paused')).toBe('已暂停');
    expect(goalStatusLabel('completed')).toBe('已完成');
    expect(goalStatusLabel('cancelled')).toBe('已取消');
    expect(goalStatusLabel('none')).toBe('未开始');
  });

  it('translates internal reason codes instead of showing them', () => {
    expect(goalReasonLabel('user_stop')).toBe('你手动暂停了这个目标');
    expect(goalReasonLabel('provider_error')).toContain('模型服务出错');
    for (const code of ['user_stop', 'interrupted', 'finish_retry_limit', 'scope_changed']) {
      expect(goalReasonLabel(code)).not.toContain(code);
    }
    expect(goalReasonLabel('')).toBe('');
    // A code the frontend does not know still never leaks verbatim.
    expect(goalReasonLabel('some_future_code')).not.toContain('some_future_code');
  });

  it('offers only the actions the host can execute', () => {
    expect(goalActions('running').map(a => a.id)).toEqual(['pause', 'cancel']);
    expect(goalActions('paused').map(a => a.id)).toEqual(['resume', 'cancel']);
    expect(goalActions('completed').map(a => a.id)).toEqual(['resume']);
    expect(goalActions('cancelled').map(a => a.id)).toEqual(['resume']);
    expect(goalControlLine('pause')).toBe(`${GOAL_COMMAND} pause`);
    expect(goalControlLine('resume')).toBe(`${GOAL_COMMAND} resume`);
    expect(goalControlLine('cancel')).toBe(`${GOAL_COMMAND} cancel`);
  });

  it('always states the next step', () => {
    expect(goalNextStep('running')).toContain('暂停');
    expect(goalNextStep('paused')).toContain(`${GOAL_COMMAND} resume`);
    expect(goalNextStep('completed')).toContain('开始下一个目标');
    expect(goalNextStep('cancelled')).toContain('新的目标');
    expect(goalNotice({...running, status:'paused', reason:'user_stop'})).toBe(
      '目标 goal-7 · 已暂停 · 你手动暂停了这个目标；'
      + goalNextStep('paused')
    );
    expect(goalNotice({...running, id:'', reason:''})).toBe(`当前目标 · 执行中；${goalNextStep('running')}`);
  });
});

it('restores the durable goal after reload and session switching', () => {
  const snapshot = {control: {...running, status: 'paused' as const, command: '/work'}};
  const restored = goalFromSnapshot(snapshot as import('./api').SessionSnapshot);
  expect(restored).toMatchObject({status: 'paused', command: '/work'});
  expect(goalControlLine('resume', restored?.command)).toBe('/work resume');
  expect(goalFromSnapshot({} as import('./api').SessionSnapshot)).toBeNull();
  expect(goalFromSnapshot(null)).toBeNull();
});
it('does not continue the queue or mislabel a blocked goal as success', () => {
  expect(goalAllowsFollowup({...running, status: 'paused'})).toBe(false);
  expect(goalAllowsFollowup({...running, status: 'completed'})).toBe(true);
  expect(goalAllowsFollowup(null)).toBe(true);
  expect(goalReasonLabel('BLOCKED')).toContain('阻塞');
  expect(goalReasonLabel('plugin_error')).toContain('插件出错');
});

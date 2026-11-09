import {describe, expect, it} from 'vitest';
import {steerNotice} from './steering';

describe('steering notices', () => {
  it('reports the actual /goal outcome instead of always claiming an insert', () => {
    expect(steerNotice({kind:'status'})).toContain('状态');
    expect(steerNotice({kind:'paused'})).toContain('暂停');
    expect(steerNotice({kind:'cancelled'})).toContain('取消');
    expect(steerNotice({kind:'steered'})).toContain('插入指令');
    expect(new Set([
      steerNotice({kind:'status'}),
      steerNotice({kind:'paused'}),
      steerNotice({kind:'cancelled'}),
    ]).size).toBe(3);
  });

  it('shows the real goal status for /goal status instead of a placeholder', () => {
    const goal = {
      owner:'kroma-goal', id:'goal-7', revision:3,
      status:'paused' as const, reason:'user_stop', summary:'把登录页改成深色',
    };
    const notice = steerNotice({kind:'status'}, goal);
    expect(notice).toContain('目标 goal-7');
    expect(notice).toContain('已暂停');
    expect(notice).toContain('你手动暂停了这个目标');
    expect(notice).toContain('/goal resume');
    expect(notice).not.toContain('user_stop');
    // Without a published state the message must not claim a known status.
    expect(steerNotice({kind:'status'})).toBe('已刷新当前目标状态，等待宿主回传');
    expect(steerNotice({kind:'status'}, {...goal, status:'none', id:''}))
      .toBe('已刷新当前目标状态，等待宿主回传');
  });

  it('keeps the plain insert wording for an unknown or empty result', () => {
    expect(steerNotice(undefined)).toBe('已向当前任务插入指令');
    expect(steerNotice({})).toBe('已向当前任务插入指令');
    expect(steerNotice({kind:'something-new'})).toBe('已向当前任务插入指令');
  });
});

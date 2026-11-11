import {describe, expect, it} from 'vitest';
import {steerNotice} from './steering';

describe('steering notices', () => {
  it('reports the actual control outcome instead of always claiming an insert', () => {
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

  it('keeps the plain insert wording for an unknown or empty result', () => {
    expect(steerNotice(undefined)).toBe('已向当前任务插入指令');
    expect(steerNotice({})).toBe('已向当前任务插入指令');
    expect(steerNotice({kind:'something-new'})).toBe('已向当前任务插入指令');
  });
});

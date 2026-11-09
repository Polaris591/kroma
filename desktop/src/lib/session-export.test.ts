import { describe, expect, it } from 'vitest';
import { exportFileStem } from './session-export';

describe('exportFileStem', () => {
  it('keeps a readable title', () => {
    expect(exportFileStem('修复登录问题')).toBe('修复登录问题.md');
  });

  it('replaces path-hostile characters', () => {
    expect(exportFileStem('a/b\\c:d*e?f"g<h>i|j')).toBe('a_b_c_d_e_f_g_h_i_j.md');
  });

  it('falls back for an empty title and caps the length', () => {
    expect(exportFileStem('')).toBe('Kroma 会话.md');
    expect(exportFileStem('x'.repeat(90))).toBe('x'.repeat(56) + '.md');
  });
});

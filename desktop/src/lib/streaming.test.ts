import {describe,expect,it} from 'vitest';
import {markdownRefreshDelay} from './streaming';

describe('live Markdown refresh rate', () => {
  it('keeps short responses responsive and bounds heavy repeated parsing', () => {
    expect(markdownRefreshDelay(0)).toBe(50);
    expect(markdownRefreshDelay(1000)).toBe(50);
    expect(markdownRefreshDelay(16001)).toBe(100);
    expect(markdownRefreshDelay(64001)).toBe(180);
  });
});

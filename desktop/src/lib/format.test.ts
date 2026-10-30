import { describe, expect, it } from 'vitest';
import { cachePercentage, compactCount } from './format';

describe('desktop status formatting', () => {
  it('does not invent unavailable measurements', () => {
    expect(compactCount(undefined)).toBe('—');
    expect(compactCount(Number.NaN)).toBe('—');
    expect(cachePercentage(null, 100)).toBe('—');
    expect(cachePercentage(0, 0)).toBe('—');
    expect(cachePercentage(20, 21)).toBe('—');
  });
  it('distinguishes actual reported 100% from rounded percentages', () => {
    expect(cachePercentage(1000, 1000)).toBe('100% (reported)');
    expect(cachePercentage(2000, 1999)).toBe('<100%');
    expect(cachePercentage(1000, 100)).toBe('10.0%');
  });
  it('renders compact counts without rounding invalid values', () => {
    expect(compactCount(1200)).toBe('1.2k');
    expect(compactCount(1_200_000)).toBe('1.2m');
    expect(compactCount(999)).toBe('999');
  });
});

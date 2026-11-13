import { describe, expect, it } from 'vitest';
import { contextDisplay } from './context-display';
import type { ContextReport } from './api';

const report = (overrides: Partial<ContextReport> = {}): ContextReport => ({
  window: 10_000, available_input: 8_000, output_reserve: 2_000,
  segments: [{key: 'system', tokens: 200}, {key: 'messages', tokens: 4_000}],
  ...overrides,
});

describe('context display', () => {
  it('does not invent usage from a configured capacity', () => {
    expect(contextDisplay(null, 10_000)).toMatchObject({capacity: 10_000, used: null, free: null, reserve: null, percent: null, fraction: 0});
  });
  it('distinguishes a measured empty request from missing information', () => {
    expect(contextDisplay(report({segments: []}), null)).toMatchObject({used: 0, percent: 0, free: 8_000});
  });
  it('honours an independent input ceiling', () => {
    expect(contextDisplay(report({available_input: 6_000}), null)).toMatchObject({used: 4_200, free: 1_800, percent: 42});
  });
  it('does not spend the reserved output allowance', () => {
    expect(contextDisplay(report({available_input: 10_000}), null).free).toBe(3_800);
  });
  it('keeps an overflow visible while clamping the ring and remaining space', () => {
    expect(contextDisplay(report({segments: [{key: 'messages', tokens: 15_000}]}), null)).toMatchObject({percent: 150, fraction: 1, free: 0});
  });
  it('uses the report capacity and excludes non-positive segments', () => {
    expect(contextDisplay(report({segments: [{key: 'empty', tokens: 0}, {key: 'system', tokens: 100}]}), 500_000))
      .toMatchObject({capacity: 10_000, used: 100, segments: [{key: 'system', tokens: 100}]});
  });
});

import {describe, expect, it} from 'vitest';
import {progressPercent, tableGrid, tableWidth} from './panel-blocks';

describe('panel progress block', () => {
  it('clamps value/max into a usable bar width', () => {
    expect(progressPercent(3, 10)).toBe(30);
    expect(progressPercent(1, 3)).toBe(33);
    expect(progressPercent(15, 10)).toBe(100);
    expect(progressPercent(-4, 10)).toBe(0);
  });

  it('stays at zero when the maximum cannot divide', () => {
    expect(progressPercent(5, 0)).toBe(0);
    expect(progressPercent(5, -5)).toBe(0);
    expect(progressPercent(Number.NaN, 10)).toBe(0);
    expect(progressPercent(5, Number.POSITIVE_INFINITY)).toBe(0);
  });
});

describe('panel table block', () => {
  it('takes the wider of headers and rows as the column count', () => {
    expect(tableWidth(['a', 'b'], [['1', '2'], ['3']])).toBe(2);
    expect(tableWidth(['a'], [['1', '2', '3']])).toBe(3);
    expect(tableWidth([], [])).toBe(0);
  });

  it('pads every row to that width so columns stay aligned', () => {
    const grid = tableGrid(['Name', 'Size'], [['todo.rs'], ['goal.rs', '12', 'extra']]);
    expect(grid.headers).toEqual(['Name', 'Size', '']);
    expect(grid.rows).toEqual([
      ['todo.rs', '', ''],
      ['goal.rs', '12', 'extra'],
    ]);
    expect(grid.rows.every(row => row.length === grid.headers.length)).toBe(true);
  });
});

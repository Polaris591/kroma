import { describe, expect, it } from 'vitest';
import { looksLikeSource, splitTrailingSource } from './citations';

describe('Trailing source detection', () => {
  it('splits a plain parenthetical outlet from the report body', () => {
    expect(splitTrailingSource('路演预计下周启动。（TechCrunch）')).toEqual({
      body: '路演预计下周启动。', source: 'TechCrunch',
    });
    expect(splitTrailingSource('抛售后的谨慎情绪。(HN 汇总)')).toEqual({
      body: '抛售后的谨慎情绪。', source: 'HN 汇总',
    });
    expect(splitTrailingSource('已接入 Workspace Vids。（Google Cloud Blog、Workspace Updates）')).toEqual({
      body: '已接入 Workspace Vids。', source: 'Google Cloud Blog、Workspace Updates',
    });
  });

  it('leaves prose, figures, dates and pointers as plain text', () => {
    expect(splitTrailingSource('漏洞（数据未经独立核实）。')).toBeNull();
    expect(splitTrailingSource('印发通知（1.1 万亿美元）')).toBeNull();
    expect(splitTrailingSource('本周重点（详见附录）')).toBeNull();
    expect(splitTrailingSource('2026-11-07（周六）')).toBeNull();
    expect(splitTrailingSource('定价 $0.07/秒，(比上代便宜 30%)')).toBeNull();
    expect(splitTrailingSource('还没有括号结尾')).toBeNull();
  });

  it('models what an outlet name looks like', () => {
    expect(looksLikeSource('路透')).toBe(true);
    expect(looksLikeSource('WSJ、彭博')).toBe(true);
    expect(looksLikeSource('Google Cloud Blog、Workspace Updates')).toBe(true);
    expect(looksLikeSource('说明：见上')).toBe(false);
    expect(looksLikeSource('周三')).toBe(false);
  });
});

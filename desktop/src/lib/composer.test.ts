import {describe, expect, it} from 'vitest';
import {canCompose, composerAction, composerPlaceholder, composerTip, COMPACTING_TIP, COMPACTING_PLACEHOLDER} from './composer';
import type {ComposerGate} from './composer';

const gate = (over: Partial<ComposerGate> = {}): ComposerGate => ({
  ready: true, busy: false, running: false, compacting: false, hasContent: true, ...over,
});

describe('composer gating', () => {
  it('sends only from an idle, connected, non-compacting composer', () => {
    expect(canCompose(gate())).toBe(true);
    expect(canCompose(gate({compacting: true}))).toBe(false);
    expect(canCompose(gate({busy: true}))).toBe(false);
    expect(canCompose(gate({running: true}))).toBe(false);
    expect(canCompose(gate({ready: false}))).toBe(false);
    expect(canCompose(gate({hasContent: false}))).toBe(false);
  });

  it('never queues a task while the context is being compacted', () => {
    expect(composerAction(gate({compacting: true, hasContent: false}))).toBe('compact-wait');
    expect(composerAction(gate({compacting: true}))).toBe('compact-wait');
    expect(composerPlaceholder(gate({compacting: true}))).toBe(COMPACTING_PLACEHOLDER);
    expect(COMPACTING_TIP).toContain('压缩上下文');
  });

  it('uses a quiet placeholder without changing blocked or running explanations', () => {
    expect(composerPlaceholder(gate())).toBe('想做什么？');
    expect(composerPlaceholder(gate({compacting: true}))).toBe(COMPACTING_PLACEHOLDER);
    expect(composerPlaceholder(gate({running: true}))).toContain('补充指令');
    expect(composerPlaceholder(gate({ready: false}))).toContain('连接模型');
  });

  it('explains the running, connected and empty states', () => {
    expect(composerAction(gate({running: true}))).toBe('steer');
    expect(composerAction(gate({running: true, hasContent: false}))).toBe('blocked');
    expect(composerAction(gate({hasContent: false}))).toBe('blocked');
    expect(composerAction(gate())).toBe('send');
    expect(composerPlaceholder(gate({running: true}))).toContain('补充指令');
    expect(composerPlaceholder(gate({ready: false}))).toContain('连接模型');
  });
});

describe('composerTip', () => {
  it('prefers the file suggestion hint', () => {
    expect(composerTip('send', false, true)).toContain('↑↓ 切换');
  });

  it('explains a blocking compaction', () => {
    expect(composerTip('compact-wait', false, false)).toBe(COMPACTING_TIP);
  });

  it('switches between steering and sending hints', () => {
    expect(composerTip('steer', true, false)).toContain('Ctrl+Enter');
    expect(composerTip('send', false, false)).toContain('Shift+Enter');
  });
});

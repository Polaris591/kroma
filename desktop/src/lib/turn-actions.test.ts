import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DesktopHost } from './desktop-host';

const apiMock = vi.hoisted(() => ({ rewindTurn: vi.fn() }));
vi.mock('./api', () => ({ api: apiMock }));
const { createTurnActions } = await import('./turn-actions');

function host() {
  return {
    rewindTarget: { messageIndex: 2, snapshotTurn: 1, files: 1, added: 1, removed: 1 },
    rewinding: false, busy: false,
    clearError: vi.fn(), showError: vi.fn(), toast: vi.fn(),
    syncHistory: vi.fn(), restoreDraft: vi.fn(), restoreFollowups: vi.fn(),
    scrollDown: vi.fn().mockResolvedValue(undefined),
  } as unknown as DesktopHost;
}
beforeEach(() => { apiMock.rewindTurn.mockReset(); });

describe('rewind confirmation visibility', () => {
  it('keeps the confirmation visible while applying, then closes after success', async () => {
    const state = host();
    let finish!: (value: unknown) => void;
    apiMock.rewindTurn.mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    const work = createTurnActions(state).confirmRewind(true);
    expect(state.rewindTarget).not.toBeNull();
    expect(state.rewinding).toBe(true);
    expect(state.busy).toBe(true);
    finish({ saved: 'branch', view: {} });
    await work;
    expect(apiMock.rewindTurn).toHaveBeenCalledWith(2, 1, true);
    expect(state.rewindTarget).toBeNull();
    expect(state.rewinding).toBe(false);
    expect(state.busy).toBe(false);
  });

  it('keeps a failed confirmation available for correction or retry', async () => {
    const state = host();
    const target = state.rewindTarget;
    const error = new Error('restore failed');
    let fail!: (reason: unknown) => void;
    apiMock.rewindTurn.mockImplementation(() => new Promise((_, reject) => { fail = reject; }));
    const work = createTurnActions(state).confirmRewind(false);
    fail(error);
    await work;
    expect(state.rewindTarget).toBe(target);
    expect(state.showError).toHaveBeenCalledWith(error);
    expect(state.toast).not.toHaveBeenCalled();
    expect(state.busy).toBe(false);
    expect(state.rewinding).toBe(false);
  });

  it('does not submit the same rewind twice while busy', async () => {
    const state = host();
    state.rewinding = true;
    await createTurnActions(state).confirmRewind(true);
    expect(apiMock.rewindTurn).not.toHaveBeenCalled();
  });
});

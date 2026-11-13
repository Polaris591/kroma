import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { modal } from './modal';

function dialog() {
  return Object.assign(new EventTarget(), {
    showModal: vi.fn(), close: vi.fn(), querySelector: vi.fn(),
  });
}

let opener: { isConnected: boolean; getClientRects: ReturnType<typeof vi.fn>; focus: ReturnType<typeof vi.fn> };
beforeEach(() => {
  opener = { isConnected: true, getClientRects: vi.fn(() => [1]), focus: vi.fn() };
  vi.stubGlobal('document', { activeElement: opener });
});
afterEach(() => vi.unstubAllGlobals());

describe('native modal action', () => {
  it('opens a modal and focuses the requested field', () => {
    const node = dialog();
    const field = { focus: vi.fn() };
    node.querySelector.mockReturnValue(field);
    const action = modal(node as unknown as HTMLDialogElement, { onClose: vi.fn(), initialFocus: 'input' });
    expect(node.showModal).toHaveBeenCalledOnce();
    expect(node.querySelector).toHaveBeenCalledWith('input');
    expect(field.focus).toHaveBeenCalledWith({ preventScroll: true });
    action.destroy();
  });

  it('routes Escape cancellation through the owner, not an invisible native close', () => {
    const node = dialog();
    const onClose = vi.fn();
    const action = modal(node as unknown as HTMLDialogElement, { onClose });
    const event = new Event('cancel', { cancelable: true });
    node.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(onClose).toHaveBeenCalledOnce();
    action.destroy();
  });

  it('keeps a busy operation visible and uses the latest busy state', () => {
    const node = dialog();
    const onClose = vi.fn();
    const action = modal(node as unknown as HTMLDialogElement, { onClose });
    action.update({ onClose, busy: true });
    const event = new Event('cancel', { cancelable: true });
    node.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(onClose).not.toHaveBeenCalled();
    action.update({ onClose, busy: false });
    node.dispatchEvent(new Event('cancel', { cancelable: true }));
    expect(onClose).toHaveBeenCalledOnce();
    action.destroy();
  });

  it('cleans up and restores the opener without scrolling', () => {
    const node = dialog();
    const onClose = vi.fn();
    const action = modal(node as unknown as HTMLDialogElement, { onClose });
    action.destroy();
    expect(node.close).toHaveBeenCalledOnce();
    expect(opener.focus).toHaveBeenCalledWith({ preventScroll: true });
    node.dispatchEvent(new Event('cancel', { cancelable: true }));
    expect(onClose).not.toHaveBeenCalled();
  });

  it.each(['detached', 'hidden'])('does not focus a %s opener', state => {
    const node = dialog();
    const action = modal(node as unknown as HTMLDialogElement, { onClose: vi.fn() });
    if (state === 'detached') opener.isConnected = false;
    else opener.getClientRects.mockReturnValue([]);
    action.destroy();
    expect(opener.focus).not.toHaveBeenCalled();
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { popover } from './popover';

let doc: EventTarget & {querySelector: ReturnType<typeof vi.fn>; activeElement: unknown; body: object};
let trigger: {isConnected: boolean; focus: ReturnType<typeof vi.fn>; contains: ReturnType<typeof vi.fn>};
let node: EventTarget & {focus: ReturnType<typeof vi.fn>; contains: ReturnType<typeof vi.fn>};
let onClose: ReturnType<typeof vi.fn>;
beforeEach(() => {
  doc = Object.assign(new EventTarget(), {querySelector: vi.fn(() => null), activeElement: null as unknown, body: {}});
  trigger = {isConnected: true, focus: vi.fn(), contains: vi.fn(target => target === trigger)};
  node = Object.assign(new EventTarget(), {focus: vi.fn(), contains: vi.fn(target => target === node)});
  doc.activeElement = node;
  onClose = vi.fn();
  vi.stubGlobal('document', doc);
});
afterEach(() => vi.unstubAllGlobals());
const open = () => popover(node as unknown as HTMLElement, {onClose, trigger: trigger as unknown as HTMLElement});
function escape(composing = false) {
  const event = Object.assign(new Event('keydown', {cancelable: true}), {key: 'Escape', isComposing: composing});
  node.dispatchEvent(event);
  return event;
}

describe('non-modal popover', () => {
  it('focuses the details and restores the trigger on Escape', () => {
    const action = open();
    expect(node.focus).toHaveBeenCalledWith({preventScroll: true});
    expect(escape().defaultPrevented).toBe(true);
    expect(onClose).toHaveBeenCalledOnce();
    action.destroy();
    expect(trigger.focus).toHaveBeenCalledOnce();
  });
  it('lets an overlaid native dialog and an input method own Escape', () => {
    const action = open();
    escape(true);
    doc.querySelector.mockReturnValue({} as never);
    escape();
    expect(onClose).not.toHaveBeenCalled();
    action.destroy();
    expect(trigger.focus).not.toHaveBeenCalled();
  });
  it('dismisses on outside focus without stealing it back', () => {
    const action = open();
    node.dispatchEvent(Object.assign(new Event('focusout'), {relatedTarget: {}}));
    expect(onClose).toHaveBeenCalledOnce();
    action.destroy();
    expect(trigger.focus).not.toHaveBeenCalled();
  });
  it('keeps focus moving within the popover or back to its trigger', () => {
    const action = open();
    node.dispatchEvent(Object.assign(new Event('focusout'), {relatedTarget: trigger}));
    node.dispatchEvent(Object.assign(new Event('focusout'), {relatedTarget: node}));
    expect(onClose).not.toHaveBeenCalled();
    action.destroy();
  });
  it('removes listeners and never restores a detached trigger', () => {
    const action = open();
    trigger.isConnected = false;
    action.destroy();
    escape();
    doc.dispatchEvent(new Event('pointerdown'));
    expect(onClose).not.toHaveBeenCalled();
    expect(trigger.focus).not.toHaveBeenCalled();
  });
});

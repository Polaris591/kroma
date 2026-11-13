type PopoverOptions = {
  onClose: () => void;
  trigger: HTMLElement;
};

/** Non-modal details keep Tab in the normal document order; Escape returns to
 * the trigger, while a click or focus outside dismisses without stealing focus.
 * A native dialog above the popover owns its own focus and Escape. */
export function popover(node: HTMLElement, options: PopoverOptions) {
  let restoreFocus = true;
  function dismiss(restore: boolean) {
    restoreFocus = restore;
    options.onClose();
  }
  function keydown(event: KeyboardEvent) {
    if (event.key !== 'Escape' || event.isComposing || event.defaultPrevented
      || document.querySelector('dialog:modal')) return;
    event.preventDefault();
    event.stopPropagation();
    dismiss(true);
  }
  function outside(event: PointerEvent) {
    if (document.querySelector('dialog:modal')) return;
    const target = event.target as Node;
    if (!node.contains(target) && !options.trigger.contains(target)) dismiss(false);
  }
  function focusout(event: FocusEvent) {
    if (document.querySelector('dialog:modal')) return;
    const target = event.relatedTarget as Node | null;
    if (target && !node.contains(target) && !options.trigger.contains(target)) dismiss(false);
  }
  node.addEventListener('keydown', keydown);
  node.addEventListener('focusout', focusout);
  document.addEventListener('pointerdown', outside, {capture: true});
  node.focus({preventScroll: true});
  return {
    update(next: PopoverOptions) { options = next; },
    destroy() {
      node.removeEventListener('keydown', keydown);
      node.removeEventListener('focusout', focusout);
      document.removeEventListener('pointerdown', outside, {capture: true});
      if (restoreFocus && options.trigger.isConnected && !document.querySelector('dialog:modal')
        && (!document.activeElement || document.activeElement === document.body || node.contains(document.activeElement))) {
        options.trigger.focus({preventScroll: true});
      }
    },
  };
}

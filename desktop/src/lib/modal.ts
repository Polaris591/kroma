type ModalOptions = {
  onClose: () => void;
  busy?: boolean;
  initialFocus?: string;
};

export function modal(node: HTMLDialogElement, options: ModalOptions) {
  const opener = document.activeElement as HTMLElement | null;
  const cancel = (event: Event) => {
    event.preventDefault();
    if (!options.busy) options.onClose();
  };
  node.addEventListener('cancel', cancel);
  node.showModal();
  if (options.initialFocus) {
    node.querySelector<HTMLElement>(options.initialFocus)?.focus({ preventScroll: true });
  }

  return {
    update(next: ModalOptions) { options = next; },
    destroy() {
      node.removeEventListener('cancel', cancel);
      node.close();
      if (opener?.isConnected && opener.getClientRects().length) {
        opener.focus({ preventScroll: true });
      }
    },
  };
}

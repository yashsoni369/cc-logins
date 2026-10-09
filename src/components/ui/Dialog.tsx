import { useEffect, useRef, type ReactNode } from "react";

interface DialogProps {
  open: boolean;
  onClose: () => void;
  /** Accessible name. */
  label: string;
  /** `"drawer"` slides in from the right edge; `"center"` is a small modal. */
  variant?: "drawer" | "center";
  className?: string;
  children: ReactNode;
}

/**
 * A modal on the native `<dialog>` element.
 *
 * `showModal()` gives the WAI-ARIA dialog behaviour for free — focus moved
 * inside, background made inert, Escape to close, top-layer stacking — and
 * the browser returns focus to whatever opened it. This wrapper only syncs
 * `open` with the element and closes on a backdrop click.
 */
export default function Dialog({ open, onClose, label, variant = "center", className, children }: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      aria-label={label}
      className={`dlg dlg-${variant}${className ? ` ${className}` : ""}`}
      onCancel={(e) => {
        // Escape: let React own the state instead of the element closing itself.
        e.preventDefault();
        onClose();
      }}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      {open && <div className="dlg-body">{children}</div>}
    </dialog>
  );
}

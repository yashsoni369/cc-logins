import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

export interface ToastInput {
  message: string;
  /** One optional action, e.g. Undo. */
  action?: { label: string; run: () => void };
  /** Milliseconds before it leaves on its own. Default 8s. */
  duration?: number;
}

interface ToastApi {
  show: (toast: ToastInput) => void;
  dismiss: () => void;
}

const ToastContext = createContext<ToastApi>({ show: () => {}, dismiss: () => {} });

export function useToast(): ToastApi {
  return useContext(ToastContext);
}

/**
 * One quiet confirmation at a time, bottom-right. It reports; it does not
 * celebrate.
 *
 * The `role="status"` region is mounted once, empty, with the app — screen
 * readers (NVDA, JAWS) do not announce a live region inserted together with
 * its text, so the message is rendered into a region that already exists.
 */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [toast, setToast] = useState<(ToastInput & { id: number }) | null>(null);
  const counter = useRef(0);

  const dismiss = useCallback(() => setToast(null), []);
  const show = useCallback((input: ToastInput) => {
    counter.current += 1;
    setToast({ ...input, id: counter.current });
  }, []);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(dismiss, toast.duration ?? 8_000);
    return () => window.clearTimeout(timer);
  }, [toast, dismiss]);

  const api = useMemo(() => ({ show, dismiss }), [show, dismiss]);

  return (
    <ToastContext.Provider value={api}>
      {children}
      <div className="toast-region" role="status" aria-live="polite">
        {toast && (
          <div className="toast" key={toast.id}>
            <span>{toast.message}</span>
            {toast.action && (
              <button
                type="button"
                className="btn ghost toast-action"
                onClick={() => {
                  const run = toast.action?.run;
                  dismiss();
                  run?.();
                }}
              >
                {toast.action.label}
              </button>
            )}
            <button type="button" className="btn ghost btn-icon" aria-label="Dismiss" onClick={dismiss}>
              ✕
            </button>
          </div>
        )}
      </div>
    </ToastContext.Provider>
  );
}

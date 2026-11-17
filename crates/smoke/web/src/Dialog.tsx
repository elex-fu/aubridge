import { createContext, useContext, useLayoutEffect, useRef, type ReactNode } from "react";

export const DialogErrorContext = createContext({ message: "", clear: () => {} });

interface DialogProps {
  children: ReactNode;
  onClose: () => void;
  busy?: boolean;
  label?: string;
  labelledBy?: string;
  describedBy?: string;
  role?: "dialog" | "alertdialog";
  className?: string;
}

const FOCUSABLE = 'button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex="-1"])';

// Keep modal keyboard behavior local to the dialog, including busy close guards.
export function Dialog({ children, onClose, busy = false, label, labelledBy, describedBy, role = "dialog", className = "" }: DialogProps) {
  const error = useContext(DialogErrorContext);
  const element = useRef<HTMLDivElement>(null);
  const trigger = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  const close = useRef(onClose);
  const blocked = useRef(busy);
  close.current = onClose;
  blocked.current = busy;

  useLayoutEffect(() => {
    const dialog = element.current;
    if (!dialog) return;
    const focusable = () => Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE))
      .filter(node => node.tabIndex >= 0 && node.getClientRects().length > 0);
    if (!dialog.contains(document.activeElement)) (focusable()[0] ?? dialog).focus();
    const keydown = (event: KeyboardEvent) => {
      if (event.defaultPrevented) return;
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        if (!blocked.current) close.current();
      }
      if (event.key !== "Tab") return;
      const items = focusable();
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (!first) {
        event.preventDefault();
        dialog.focus();
      } else if (!dialog.contains(active) || active === dialog || (event.shiftKey ? active === first : active === last)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      }
    };
    document.addEventListener("keydown", keydown);
    return () => {
      document.removeEventListener("keydown", keydown);
      if (trigger.current?.isConnected) trigger.current.focus();
    };
  }, []);

  return (
    <div className="modal-backdrop" onClick={event => {
      if (event.target === event.currentTarget && !busy) onClose();
    }}>
      <div ref={element} className={`modal ${className}`} role={role} aria-modal="true"
        aria-label={label} aria-labelledby={labelledBy} aria-describedby={describedBy} tabIndex={-1}>
        {error.message && (
          <div className="alert error modal-error" role="alert">
            <div className="app-error-message">{error.message}</div>
            <button className="ghost" aria-label="关闭错误提示" onClick={error.clear}>关闭</button>
          </div>
        )}
        {children}
      </div>
    </div>
  );
}

import type { ReactNode } from "react";

// Presentation only. The owner controls task identity, lifetime and side effects.
export function TaskView({ title, onBack, backLabel = "返回", busy = false, children }: {
  title: string;
  onBack: () => void;
  backLabel?: string;
  busy?: boolean;
  children: ReactNode;
}) {
  return <section className="task-view" aria-label={title} aria-busy={busy || undefined}>
    <header className="task-view-header">
      <button type="button" className="ghost" disabled={busy} onClick={onBack}>{backLabel}</button>
      <h3>{title}</h3>
    </header>
    <div className="task-view-body">{children}</div>
  </section>;
}

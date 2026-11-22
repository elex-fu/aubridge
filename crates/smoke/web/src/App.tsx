import { useRef, useState } from "react";
import appIcon from "../../icons/app-icon.svg";
import { Agents } from "./Agents";
import { DialogErrorContext } from "./Dialog";
import { Models } from "./Models";
import { Plugins } from "./Plugins";
import { Providers } from "./Providers";
import { Proxy } from "./Proxy";
import { Settings } from "./Settings";
import type { AgentPrerequisite } from "./agent-flow";
import type { AnalyticsRange } from "./analytics-types";

// Keep navigation shared by Agent actions and the six existing pages.
export type AppTab = "agents" | "providers" | "models" | "proxy" | "plugins" | "settings";

const TABS: { key: AppTab; label: string }[] = [
  { key: "agents", label: "Agents" },
  { key: "providers", label: "模型源" },
  { key: "models", label: "模型" },
  { key: "proxy", label: "代理" },
  { key: "plugins", label: "插件" },
  { key: "settings", label: "设置" },
];

export function App() {
  const [tab, setTab] = useState<AppTab>("agents");
  const [err, setErr] = useState("");
  const main = useRef<HTMLElement>(null);
  const originScroll = useRef(0);
  const reportFocus = useRef<HTMLElement | null>(null);
  const sequence = useRef(0);
  const [returnIntent, setReturnIntent] = useState<{ kind: "source"; agent: AgentPrerequisite } | { kind: "analytics" } | null>(null);
  const [refreshRevision, setRefreshRevision] = useState(0);
  const [sourceBusy, setSourceBusy] = useState(false);
  const [modelsBusy, setModelsBusy] = useState(false);
  const [pluginsBusy, setPluginsBusy] = useState(false);
  const navigationBusy = sourceBusy || modelsBusy || pluginsBusy;
  const [historyEntry, setHistoryEntry] = useState<{ range: AnalyticsRange; sequence: number } | null>(null);
  const resumeOrigin = () => {
    if (!returnIntent || navigationBusy) return;
    const returningToReport = returnIntent.kind === "analytics";
    setTab("agents"); setReturnIntent(null); setRefreshRevision(current => current + 1);
    window.requestAnimationFrame(() => {
      if (main.current) main.current.scrollTop = originScroll.current;
      if (returningToReport && reportFocus.current?.isConnected) reportFocus.current.focus({ preventScroll: true });
    });
  };
  const navigate = (next: AppTab) => {
    if (navigationBusy || next === tab) return;
    if (next === "agents" && returnIntent) { resumeOrigin(); return; }
    setReturnIntent(null); setHistoryEntry(null); setTab(next);
    window.requestAnimationFrame(() => { if (main.current) main.current.scrollTop = 0; });
  };
  const prepareSources = (agent: AgentPrerequisite) => {
    originScroll.current = main.current?.scrollTop || 0;
    setReturnIntent({ kind: "source", agent }); setTab("providers");
    window.requestAnimationFrame(() => { if (main.current) main.current.scrollTop = 0; });
  };
  const inspectRange = (range: AnalyticsRange) => {
    originScroll.current = main.current?.scrollTop || 0;
    reportFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setReturnIntent({ kind: "analytics" }); setHistoryEntry({ range, sequence: ++sequence.current }); setTab("proxy");
    window.requestAnimationFrame(() => { if (main.current) main.current.scrollTop = 0; });
  };
  return (
    <DialogErrorContext.Provider value={{ message: err, clear: () => setErr("") }}>
      <div className="app-shell">
        <header className="app-header">
          <h1 className="brand"><img src={appIcon} alt="" width="24" height="24" />aubridge</h1>
          <nav className="app-nav" aria-label="主导航">
            {TABS.map(t => (
              <button key={t.key} className={tab === t.key ? "on" : ""} aria-current={tab === t.key ? "page" : undefined} disabled={navigationBusy && t.key !== tab} onClick={() => navigate(t.key)}>
                {t.label}
              </button>
            ))}
          </nav>
        </header>
        {err && (
          <div className="alert error app-error" role="alert">
            <div className="app-error-message">{err}</div>
            <button className="ghost" aria-label="关闭错误提示" onClick={() => setErr("")}>关闭</button>
          </div>
        )}
        <main ref={main} className={`app-main ${tab}-view`} aria-label={TABS.find(item => item.key === tab)?.label}>
          {(tab === "agents" || returnIntent !== null) && <div className="task-workspace-pane" hidden={tab !== "agents"}><Agents setErr={setErr} go={navigate} active={tab === "agents"} refreshRevision={refreshRevision} onPrerequisites={prepareSources} onOpenRequests={inspectRange} /></div>}
          {tab === "providers" && <Providers setErr={setErr} onBusyChange={setSourceBusy} onReturn={returnIntent?.kind === "source" ? resumeOrigin : undefined} returnLabel={returnIntent?.kind === "source" ? `返回 ${returnIntent.agent.agentName} 验证` : undefined} />}
          {tab === "models" && <Models setErr={setErr} onBusyChange={setModelsBusy} />}
          {tab === "proxy" && <Proxy setErr={setErr} go={navigate} initialHistoryRange={historyEntry?.range} historyEntryKey={historyEntry?.sequence} onBackToAnalytics={returnIntent?.kind === "analytics" ? resumeOrigin : undefined} />}
          {tab === "plugins" && <Plugins setErr={setErr} onBusyChange={setPluginsBusy} />}
          {tab === "settings" && <Settings setErr={setErr} onOpenSources={() => navigate("providers")} onOpenAgents={() => navigate("agents")} />}
        </main>
      </div>
    </DialogErrorContext.Provider>
  );
}

import { useEffect, useRef, useState } from "react";
import { AgentRow, AgentsPage, Dashboard, DisconnectPreview, GatewayPage, ProvidersPage, SettingsPage, fetchJson, postJson, supportsWiring } from "./api";
import { CopyBtn } from "./CopyButton";
import { Dialog } from "./Dialog";
import { AgentConfigModal } from "./AgentConfig";
import { TaskView } from "./TaskView";
import { FirstTask } from "./FirstTask";
import { Analytics, AnalyticsOverview, formatAnalyticsMoney } from "./Analytics";
import type { AnalyticsRange } from "./analytics-types";
import { agentStep, cliInstallations, cliPreparation, type AgentEntry, type AgentPrerequisite, type AgentTarget } from "./agent-flow";

function successSource(provider: string, attempts: unknown): "free_tier" | "subscription" | "unknown" {
  if (!Array.isArray(attempts)) return "unknown";
  let selected: Record<string, unknown> | undefined;
  for (const attempt of attempts) {
    if (attempt == null || typeof attempt !== "object" || Array.isArray(attempt)) return "unknown";
    const entry: Record<string, unknown> = attempt;
    if (typeof entry.chosen !== "boolean") return "unknown";
    if (entry.chosen) {
      if (selected) return "unknown";
      selected = entry;
    }
  }
  if (typeof selected?.id !== "string" || !selected.id || selected.id !== provider) return "unknown";
  return selected.kind === "free_tier" || selected.kind === "subscription" ? selected.kind : "unknown";
}

function AgentStatus({ agent, unknown = false }: { agent: AgentRow; unknown?: boolean }) {
  const step = agentStep(agent, !unknown);
  return <span className={`badge ${step.tone}`}>{step.status}</span>;
}

function AgentEditor({ agent, models, providers, gateway, settings, configured, failed, loading, connectionBusy, preview, disconnectBusy, onCancelDisconnect, onConfirmDisconnect, onClose, onReload, onToggle, go, setErr, active, entry, refreshRevision, onPrerequisites }: {
  agent: AgentRow; models: string[]; providers: ProvidersPage | null; gateway: GatewayPage | null;
  settings: SettingsPage | null; configured: boolean; failed: boolean; loading: boolean; connectionBusy: boolean;
  preview: DisconnectPreview | null; disconnectBusy: boolean; onCancelDisconnect: () => void; onConfirmDisconnect: () => void;
  onClose: () => void; onReload: () => void; onToggle: (on: boolean) => void;
  go: (tab: "providers" | "proxy") => void; setErr: (message: string) => void;
  active: boolean; entry?: AgentEntry; refreshRevision: number; onPrerequisites?: (intent: AgentPrerequisite) => void;
}) {
  const [model, setModel] = useState(agent.model || "");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [showToken, setShowToken] = useState(false);
  const [view, setView] = useState<"connection" | "validation" | "manual" | "config">(entry && entry.target !== "connection" ? "validation" : "connection");
  const [visited, setVisited] = useState({ validation: Boolean(entry && entry.target !== "connection"), config: false });
  const [configBusy, setConfigBusy] = useState(false);
  const [validationBusy, setValidationBusy] = useState(false);
  const [checkingCli, setCheckingCli] = useState(false);
  const workspace = useRef<HTMLDivElement>(null);
  const resumeFocus = useRef<HTMLElement | null>(null);
  const resumePending = useRef(false);
  const pending = useRef(false);
  const manualTrigger = useRef<HTMLButtonElement>(null);
  const configTrigger = useRef<HTMLButtonElement>(null);
  const busy = saving || connectionBusy || configBusy || validationBusy || disconnectBusy || checkingCli;
  const cli = cliPreparation(agent);
  const simpleModel = agent.id === "opencode" || agent.id === "gemini";
  const canEditModel = simpleModel && configured && !failed && agent.wired && agent.drift === "clean";
  useEffect(() => { setModel(agent.model || ""); setSaved(false); }, [agent.model]);
  const enter = (next: typeof view) => {
    if (busy) return;
    setShowToken(false);
    if (next === "validation" || next === "config") setVisited(current => ({ ...current, [next]: true }));
    setView(next);
  };
  useEffect(() => {
    if (entry && entry.target !== "connection") { setVisited(current => ({ ...current, validation: true })); setView("validation"); }
  }, [entry?.sequence]);
  useEffect(() => {
    if (!active || loading || failed || !resumePending.current) return;
    const previous = resumeFocus.current;
    const fallback = Array.from(workspace.current?.querySelectorAll<HTMLElement>("[data-validation-primary], button:not(:disabled)") || []).find(node => node.getClientRects().length > 0 && !node.matches(":disabled"));
    (previous?.isConnected && previous.getClientRects().length && !previous.matches(":disabled") ? previous : fallback)?.focus();
    resumePending.current = false;
  }, [active, loading, failed, refreshRevision]);
  const navigate = (tab: "providers" | "proxy") => {
    if (busy) return;
    if (tab === "providers" && onPrerequisites) {
      resumeFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      resumePending.current = true;
      onPrerequisites({ agentId: agent.id, agentName: agent.name, task: "validation" });
    } else go(tab);
  };
  const returnToConnection = (trigger: "manual" | "config") => {
    enter("connection");
    window.requestAnimationFrame(() => (trigger === "manual" ? manualTrigger : configTrigger).current?.focus());
  };
  const saveModel = async () => {
    if (pending.current || !canEditModel || !model) return;
    pending.current = true; setSaving(true); setSaved(false);
    try {
      await postJson(`/admin/agents/${agent.id}/model`, { model });
      setSaved(true); onReload();
    } catch (error) { setErr(`设置 ${agent.name} 模型失败：${error}`); }
    finally { pending.current = false; setSaving(false); }
  };
  const checkCli = async () => {
    if (pending.current || busy || failed) return;
    pending.current = true; setCheckingCli(true);
    try { await postJson(`/admin/agents/${agent.id}/discovery`, {}); onReload(); }
    catch { setErr(`暂时无法检测 ${agent.name} CLI 与版本，请重新读取本机状态后再试。`); }
    finally { pending.current = false; setCheckingCli(false); }
  };
  return <Dialog label={preview ? `预览断开 ${agent.name}` : `编辑 ${agent.name}`} busy={busy} active={active} onClose={preview ? onCancelDisconnect : onClose} className="modal-wide agent-editor">
    <div className="modal-header"><h2>{preview ? `断开 ${agent.name}` : `编辑 ${agent.name}`}</h2><AgentStatus agent={agent} unknown={failed} /></div>
    <div className="modal-body" ref={workspace}>
      {preview && <DisconnectView agent={agent} preview={preview} busy={disconnectBusy} onClose={onCancelDisconnect} onConfirm={onConfirmDisconnect} />}
      <div className="task-workspace-nav task-workspace-pane" aria-label={`${agent.name} 编辑工作区`} hidden={Boolean(preview)}>
        <button className="ghost" aria-pressed={view !== "validation"} disabled={busy} onClick={() => enter("connection")}>连接与模型</button>
        <button className="ghost" aria-pressed={view === "validation"} disabled={busy} onClick={() => enter("validation")}>验证</button>
      </div>
      <section className="task-workspace-pane agent-editor-section" hidden={Boolean(preview) || view !== "connection"} aria-label={`${agent.name} 连接与模型`}>
        <p role="status">{cli.message}{agent.discovery?.version ? ` · ${agent.discovery.version}` : ""}</p>
        <div className="actions"><button className="ghost" disabled={busy || loading || failed} onClick={() => void checkCli()}>{checkingCli ? "本地检测中…" : "重新检测 CLI 与版本"}</button>{cliInstallations[agent.id] && <a href={cliInstallations[agent.id]} target="_blank" rel="noreferrer">{agent.name} 官方安装说明</a>}</div>
        <p className="muted">检测只在本机执行 --version，不调用模型、不改写接线。CLI 未就绪时仍可手动预配或预览断开。</p>
        <div className="section-head"><h3>连接与模型</h3>
          <button className={agent.wired ? "ghost" : "button-primary"} disabled={busy || loading || failed || !configured || !agent.wired && agent.drift === "replaced"} onClick={() => onToggle(!agent.wired)}>{connectionBusy ? "处理中…" : agent.wired ? "预览断开" : "连接到 aubridge"}</button>
        </div>
        <p className="muted">连接会将此 Agent 的请求指向本机网关；连接成功不代表文件工具任务已经通过。</p>
        {failed && <p className="alert error" role="alert">连接状态尚未确认，请重新读取后再操作。<button className="ghost" disabled={loading} onClick={onReload}>重新读取</button></p>}
        {agent.drift === "replaced" && <p className="alert">接线字段已被改写，请核对当前配置。断开前会预览受管字段的恢复操作。</p>}
        {!agent.wired && agent.drift && agent.drift !== "unwired" && <button className="ghost" disabled={busy || failed || !configured} onClick={() => onToggle(false)}>预览清理受管连接</button>}
        {agent.drift === "port-changed" && <button className="ghost" disabled={busy || failed || !configured} onClick={() => onToggle(true)}>按当前网关重新连接</button>}
        {simpleModel ? <>
          <div className="field"><label htmlFor={`agent-model-${agent.id}`}>使用模型</label>
            <select id={`agent-model-${agent.id}`} aria-label={`${agent.name} 使用模型`} value={model} disabled={busy || loading || !canEditModel} onChange={event => { setModel(event.target.value); setSaved(false); }}>
              {!agent.model && <option value="" disabled>{agent.id === "opencode" ? "沿用 OpenCode 原默认" : "选择模型"}</option>}
              {agent.model && !models.includes(agent.model) && <option value={agent.model}>{agent.model}</option>}
              {models.map(value => <option key={value} value={value}>{value === "aubridge" ? "自动(aubridge)" : value}</option>)}
            </select>
          </div>
          <div className="row"><button className="ghost" disabled={busy || loading || !canEditModel || !model} onClick={() => void saveModel()}>{saving ? "保存中…" : "保存模型"}</button>{saved && <span role="status" className="muted">模型已保存，实际任务尚需验证</span>}</div>
          {agent.id === "gemini" && <p className="muted">保存后请重启已打开的 Gemini CLI 会话。</p>}
          {agent.id === "opencode" && agent.wired && agent.drift !== "replaced" && <button className="ghost" disabled={busy || failed || !configured} onClick={() => onToggle(true)}>同步模型目录</button>}
        </> : <>
          <p className="muted">当前模型：{failed ? "尚未确认" : agent.model === "aubridge" ? "自动(aubridge)" : agent.model || "尚未选择"}</p>
          <button ref={configTrigger} className="ghost" disabled={busy || failed || !configured} onClick={() => enter("config")}>编辑模型与参数</button>
        </>}
        {agent.launch_command && <p className="task-inline-command">启动命令：<code>{agent.launch_command}</code> <CopyBtn text={agent.launch_command} /></p>}
        {agent.hint && <p className="muted">{agent.hint}</p>}
        {agent.token && <button ref={manualTrigger} className="ghost" disabled={busy} onClick={() => enter("manual")}>手动接入信息</button>}
      </section>
      {visited.validation && <section className="task-workspace-pane" hidden={Boolean(preview) || view !== "validation"}>
        <FirstTask agent={agent} configured={configured} settings={settings} providers={providers} gateway={gateway} failed={failed} loading={loading} onReload={onReload} go={navigate} entry={entry} refreshRevision={refreshRevision} onBusyChange={setValidationBusy} />
      </section>}
      {visited.config && <section className="task-workspace-pane" hidden={Boolean(preview) || view !== "config"}>
        <AgentConfigModal agent={agent} models={models} embedded onBusyChange={setConfigBusy} setErr={setErr} onClose={() => returnToConnection("config")} onSaved={onReload} />
      </section>}
      {view === "manual" && !preview && <TaskView title={`${agent.name} 手动接入信息`} onBack={() => returnToConnection("manual")} busy={busy}>
        <p className="muted">只读查看此 Agent 的本地连接标识；此处不会改写客户端配置。</p>
        {agent.token && <div className="row"><code>{showToken ? agent.token : "••••••••"}</code><button className="ghost" aria-pressed={showToken} onClick={() => setShowToken(value => !value)}>{showToken ? "隐藏" : "显示"}</button><CopyBtn key={`${agent.id}:${agent.token}`} text={agent.token} id={`复制 ${agent.name} Agent token`} /></div>}
      </TaskView>}
    </div>
    {!preview && <div className="modal-actions"><button className="ghost" disabled={busy} onClick={onClose}>关闭</button></div>}
  </Dialog>;
}

function DisconnectView({ agent, preview, busy, onClose, onConfirm }: {
  agent: AgentRow; preview: DisconnectPreview; busy: boolean; onClose: () => void; onConfirm: () => void;
}) {
  return <TaskView title={`预览断开 ${agent.name}`} busy={busy} onBack={onClose}>
    <p className="muted">确认前，核对以下配置恢复操作。</p>
    {preview.changes.length ? <ul>{preview.changes.map(change => <li key={change.field}><code>{change.field}</code>{" "}{change.action === "restore" ? "恢复备份值" : change.action === "remove" ? "移除接线时添加的值" : "保留现值"}</li>)}</ul> : <p>没有受 aubridge 管理的字段。</p>}
    {preview.blocked && <p className="alert error">受管配置已被改写。请先核对当前设置，再回来预览。</p>}
    <div className="actions"><button className="ghost" disabled={busy} onClick={onClose}>取消</button><button className="button-danger" disabled={busy || preview.blocked} onClick={onConfirm}>{busy ? "断开中…" : "确认断开"}</button></div>
  </TaskView>;
}

// D57: cards are the entry point; selections and actions stay inside one Agent.
// Reads refresh status only. Opening an editor never applies settings or runs a test.
export function Agents({
  setErr,
  go,
  active = true,
  refreshRevision = 0,
  onPrerequisites,
  onOpenRequests,
}: {
  setErr: (s: string) => void;
  go: (t: "agents" | "providers" | "models" | "proxy" | "plugins") => void;
  active?: boolean; refreshRevision?: number; onPrerequisites?: (intent: AgentPrerequisite) => void;
  onOpenRequests?: (range: AnalyticsRange) => void;
}) {
  const [data, setData] = useState<AgentsPage | null>(null);
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [dashboardReadAt, setDashboardReadAt] = useState<string | null>(null);
  const [dashboardFailed, setDashboardFailed] = useState(false);
  const [provs, setProvs] = useState<ProvidersPage | null>(null);
  const [costSettings, setCostSettings] = useState<SettingsPage | null>(null);
  const [gateway, setGateway] = useState<GatewayPage | null>(null);
  const [connectionFailed, setConnectionFailed] = useState(false);
  const [loading, setLoading] = useState(true);
  const loadGeneration = useRef(0);
  const pageLoaded = useRef(false);
  const [readRevision, setReadRevision] = useState<number | null>(null);
  const [entry, setEntry] = useState<AgentEntry>({ target: "connection", sequence: 0 });
  const [editingId, setEditingId] = useState<string | null>(null);
  const [usageView, setUsageView] = useState(false);
  const [usageKind, setUsageKind] = useState<"report" | "snapshot">("report");
  const [usageVisited, setUsageVisited] = useState(false);
  const [usageRange, setUsageRange] = useState<AnalyticsRange | undefined>();
  const [usageEntry, setUsageEntry] = useState(0);
  const usageTrigger = useRef<HTMLButtonElement>(null);
  const reportTrigger = useRef<HTMLElement | null>(null);
  const [connectionBusy, setConnectionBusy] = useState(false);
  const connectionPending = useRef(false);
  const [disconnect, setDisconnect] = useState<{ agent: AgentRow; preview: DisconnectPreview } | null>(null);
  const [disconnectBusy, setDisconnectBusy] = useState(false);
  const openUsage = (range?: AnalyticsRange) => {
    reportTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : usageTrigger.current;
    setUsageRange(range); setUsageEntry(current => current + 1); setUsageVisited(true); setUsageKind("report"); setUsageView(true);
  };
  const closeUsage = () => {
    setUsageView(false);
    window.requestAnimationFrame(() => (reportTrigger.current?.isConnected ? reportTrigger.current : usageTrigger.current)?.focus({ preventScroll: true }));
  };

  const loadPage = (sampleDashboard: boolean) => {
    if (!active) return;
    const generation = ++loadGeneration.current;
    setLoading(true);
    Promise.all([
      fetchJson<AgentsPage>("/admin/agents"),
      fetchJson<ProvidersPage>("/admin/providers?sample=0"),
      fetchJson<GatewayPage>("/admin/gateway?limit=1"),
      fetchJson<SettingsPage>("/admin/settings"),
    ]).then(([agents, providers, gateway, settings]) => {
      if (generation !== loadGeneration.current) return;
      setReadRevision(refreshRevision);
      setData(agents); setProvs(providers); setGateway(gateway); setCostSettings(settings);
      setConnectionFailed(false);
    }).catch(() => {
      if (generation === loadGeneration.current) setConnectionFailed(true);
    }).finally(() => {
      if (generation === loadGeneration.current) setLoading(false);
    });
    if (sampleDashboard) fetchJson<Dashboard>("/admin/dashboard")
      .then(next => {
        if (generation !== loadGeneration.current) return;
        setDash(next); setDashboardReadAt(new Date().toLocaleString()); setDashboardFailed(false);
      })
      .catch(() => { if (generation === loadGeneration.current) setDashboardFailed(true); });
  };
  // Explicit refreshes and returns update local prerequisites, preserving the KPI snapshot.
  const load = () => loadPage(false);
  useEffect(() => {
    if (!active || usageView) return;
    const firstRead = !pageLoaded.current;
    pageLoaded.current = true;
    loadPage(firstRead);
    // The server recomputes validation identity/TTL and pause facts on reads.
    // Resume the ordinary timer without immediately resampling upstream quotas.
    const timer = window.setInterval(() => loadPage(true), 20_000);
    return () => { window.clearInterval(timer); loadGeneration.current++; };
  }, [setErr, active, refreshRevision, usageView]);

  const agents: AgentRow[] = [
    ["claude-code", "Claude Code"], ["codex", "Codex"], ["opencode", "OpenCode"], ["gemini", "Gemini CLI"],
  ].map(([id, name]) => data?.agents?.find(agent => agent.id === id) || {
    id, name, wired: false, detected: false, drift: null, hint: null, model: null, token: null,
  });
  const editing = agents.find(agent => agent.id === editingId) || null;
  const confirmed = (id: string) => active && readRevision === refreshRevision && !connectionFailed && data?.agents?.some(agent => agent.id === id) === true;
  const models = data?.models || ["aubridge"];

  const toggle = async (a: AgentRow, on: boolean) => {
    if (connectionPending.current) return;
    connectionPending.current = true; setConnectionBusy(true);
    try {
      if (supportsWiring(a.id) && !on) {
        const preview = await fetchJson<DisconnectPreview>(`/admin/agents/${a.id}/disconnect-preview`);
        setDisconnect({ agent: a, preview });
        return;
      }
      await postJson(`/admin/agents/${a.id}/enable`, { on });
      load();
    } catch (e) {
      setErr(`切换 ${a.name} 失败:${e}`);
    } finally { connectionPending.current = false; setConnectionBusy(false); }
  };

  const confirmDisconnect = async () => {
    if (!disconnect || disconnect.preview.blocked) return;
    setDisconnectBusy(true);
    try {
      await postJson(`/admin/agents/${disconnect.agent.id}/enable`, { on: false, preview: disconnect.preview });
      setDisconnect(null);
      load();
    } catch {
      setDisconnect(null);
      setErr("断开未完成，请重新预览当前配置后再试。");
      load();
    } finally {
      setDisconnectBusy(false);
    }
  };

  // 汇总卡:分享卡(复制)+ 源健康 + 最近请求·成功率。
  const dashProvs = dash?.providers || [];
  const healthAvailable = !dashboardFailed && Array.isArray(dash?.providers);
  const healthy = dashProvs.filter(p => p.ready && p.healthy && p.enabled && !p.paused && !p.resting && !p.breaker_open).length;
  const recentAvailable = !dashboardFailed && Array.isArray(dash?.recent_decisions);
  const recentUnavailable = dashboardFailed || (dash !== null && !recentAvailable);
  const decisions = dash?.recent_decisions || [];
  const completed = decisions.filter(d => d.outcome !== "cancelled");
  const successful = completed.filter(d => d.outcome === "ok");
  const rate = completed.length ? Math.round((successful.length / completed.length) * 100) : null;
  const sources = { free_tier: 0, subscription: 0, unknown: 0 };
  for (const decision of successful) sources[successSource(decision.provider, decision.attempts)]++;
  const showSources = recentAvailable && (successful.length > 0 || decisions.length === 0);

  const statsUnavailable = dashboardFailed || dash?.monthly_savings_status === "unavailable";
  const savings = statsUnavailable ? null : dash?.monthly_savings_usd;
  const shareCard = savings == null ? null : dash?.share_card ?? null;
  const value=statsUnavailable ? null : dash?.value_summary;

  return (
    <div className="agents-page">
      <section className="task-workspace-pane" hidden={usageView}>
      <div className="section-head"><h2>Agents</h2><span className="muted">选择工具，编辑连接与验证</span></div>
      <dl className="usage-key-metrics" aria-label="使用统计关键值">
        <div>
          <dt>API 等值估算 · 本月至今快照</dt>
          <dd><strong>{savings == null ? value?.unpriced_records ? "待定价" : value?.records ? "待核算" : dash || dashboardFailed ? "暂不可用" : "读取中" : formatAnalyticsMoney(savings)}</strong></dd>
        </div>
        <div>
          <dt>本机就绪来源 · 最近快照</dt>
          <dd><strong>{healthAvailable ? `${healthy}/${dashProvs.length}` : dash || dashboardFailed ? "暂不可用" : "读取中"}</strong></dd>
        </div>
        <div>
          <dt>请求 · 最近返回样本</dt>
          <dd>
            <strong>{recentUnavailable ? "暂不可用" : !recentAvailable ? "读取中" : `${decisions.length} 条`}</strong>
            <span className="usage-key-note">{recentAvailable ? rate != null ? `成功率 ${rate}%` : decisions.length ? "取消请求不计成功率" : "暂无请求记录" : "请求统计尚未确认"}</span>
          </dd>
        </div>
      </dl>
      {connectionFailed && <p className="alert error" role="alert">暂时无法读取连接状态。<button className="ghost" disabled={loading} onClick={load}>重新读取</button></p>}
      <div className="agent-grid">
        {agents.map(agent => {
          const step = agentStep(agent, confirmed(agent.id));
          const open = (target: AgentTarget) => { setEntry(current => ({ target, sequence: current.sequence + 1 })); setEditingId(agent.id); };
          return <article className="agent-card" key={agent.id}>
            <h3>{agent.name}</h3>
            <div className="agent-card-status"><AgentStatus agent={agent} unknown={!confirmed(agent.id)} /></div>
            <p className="agent-card-model muted">{!confirmed(agent.id) ? "模型尚未确认" : agent.model ? agent.model === "aubridge" ? "自动(aubridge)" : agent.model : agent.id === "opencode" ? "沿用原默认模型" : "尚未选择模型"}</p>
            <div className="agent-actions"><button className="button-primary" aria-label={`${agent.name}：${step.action}`} onClick={() => step.refresh ? load() : open(step.target)}>{step.action}</button></div>
          </article>;
        })}
      </div>
      <button ref={usageTrigger} className="ghost" onClick={() => openUsage()}>查看完整使用统计</button>
      <button className="ghost" onClick={() => { reportTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null; setUsageKind("snapshot"); setUsageView(true); }}>本月快照与分享</button>
      <AnalyticsOverview onOpenAnalytics={openUsage} />
      </section>
      {usageVisited && <section className="task-workspace-pane" hidden={!usageView || usageKind !== "report"}>
        <Analytics initialRange={usageRange} entryKey={usageEntry} backLabel="返回工具概览" onOpenRequests={onOpenRequests} onBack={closeUsage} />
      </section>}
      {usageView && usageKind === "snapshot" && <TaskView title="本月快照与分享" backLabel="返回工具概览" onBack={closeUsage}>
        <p className="muted">金额与计价覆盖为本月至今快照；成功来源分布仅对应最近返回的请求样本。本机就绪按凭据可读取、已启用、未暂停、未休息及熔断关闭等状态计算，不等于上游实测成功，也不代表费用范围已授权。{dashboardReadAt ? `本页上次读取：${dashboardReadAt}。` : "尚未取得快照。"}此处保留已有快照，不随所选日期报表变化。</p>
        {dashboardFailed && <p className="alert error" role="alert">最近一次首页快照读取失败；当前不能确认统计值。</p>}
        <div className="usage-detail-actions"><p>{savings == null ? "本月 API 等值暂不可确认" : `本月 API 等值估算 ${formatAnalyticsMoney(savings)}`}{value ? ` · 已计价 ${value.priced_records}/${value.records} 条用量记录` : ""}</p><CopyBtn key={shareCard} text={shareCard} label="复制分享卡" /></div>
        {showSources && <p>最近样本的成功请求来源：免费 {sources.free_tier} · 订阅 {sources.subscription} · 未知 {sources.unknown}</p>}
        {!recentAvailable && <p>最近样本的来源分布尚未确认。</p>}
        {recentAvailable && !showSources && <p>最近样本没有成功请求，不据此计算成功来源分布。</p>}
        <button className="ghost" onClick={() => go("proxy")}>查看最近请求</button>
        {value && <section className="panel">
          <h3>本月计价与缓存覆盖</h3>
          <p>计价覆盖 {value.price_coverage == null ? "暂无用量" : `${(value.price_coverage * 100).toFixed(0)}%（${value.priced_records}/${value.records}）`} · 缓存命中 {value.cache_hit_rate == null ? "尚无可计算的报告" : `${(value.cache_hit_rate * 100).toFixed(1)}%`}</p>
          <p>已报告缓存读取 {value.cache_read_tokens.toLocaleString()} token · 创建 {value.cache_write_tokens.toLocaleString()} token；{value.cache_unknown_records} 条缓存未报告，命中率分母仅包含可计算记录。</p>
          <p>目录价覆盖 {value.official_price_coverage == null ? "暂无用量" : `${(value.official_price_coverage * 100).toFixed(0)}%`} · 用户对标覆盖 {value.benchmark_coverage == null ? "暂无用量" : `${(value.benchmark_coverage * 100).toFixed(0)}%`}（{value.benchmark_priced_records}/{value.records}）</p>
          <p>提供方目录 API 等值：{formatAnalyticsMoney(value.official_usd)} · 可估值 {value.official_valued_records} 条；目录未提供可计算价格或用量时保持未知。</p>
          <p>用户对标 API 等值：{formatAnalyticsMoney(value.benchmark_usd)} · 可估值 {value.benchmark_valued_records} 条；与目录、声明、历史固定估算分列，不表示现金节省。</p>
          <p>未定价 {value.unpriced_records} 条 · 可估值 {value.valued_records} 条 · 部分用量 {value.partial_usage_records} 条 · 内置对标 {value.builtin_priced_records} 条。</p>
          <p className="muted">按已知输入、缓存、输出 token 和当次价格折算 API 等值，未扣订阅费；{value.cache_rate_fallback_records} 条缓存计价数据不完整。无法区分缓存或缺少缓存单价时按普通输入计价。</p>
          {value.cache_value_delta_usd != null && <p>可计算的缓存计价差额 ${value.cache_value_delta_usd.toLocaleString(undefined, { maximumFractionDigits: 8 })}，覆盖 {value.cache_value_known_records} 条；已包含在 API 等值口径中，差额可为负。</p>}
        </section>}
      </TaskView>}

      {editing && <AgentEditor key={editing.id}
        agent={editing} models={models} providers={provs} gateway={gateway} settings={costSettings}
        configured={data?.configured === true && data.agents?.some(agent => agent.id === editing.id) === true}
        failed={!confirmed(editing.id)} loading={loading} connectionBusy={connectionBusy}
        preview={disconnect?.agent.id === editing.id ? disconnect.preview : null} disconnectBusy={disconnectBusy}
        onCancelDisconnect={() => setDisconnect(null)} onConfirmDisconnect={confirmDisconnect}
        onClose={() => setEditingId(null)} onReload={load} onToggle={on => void toggle(editing, on)}
        go={go} setErr={setErr} active={active} entry={entry} refreshRevision={refreshRevision} onPrerequisites={onPrerequisites} />}
    </div>
  );
}


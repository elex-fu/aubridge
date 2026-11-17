import { useEffect, useState } from "react";
import { AgentRow, AgentsPage, Dashboard, DisconnectPreview, ProvidersPage, fetchJson, postJson, supportsWiring } from "./api";
import { CopyBtn } from "./CopyButton";
import { Dialog } from "./Dialog";
import { AgentConfigModal } from "./AgentConfig";

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

// 编辑弹层(ui-design §6.1):使用模型下拉 + 本 agent token 复制。
// 保存即写该 agent 配置文件;模型提供方等细节由接线器落到目标文件,
// 弹层不暴露配置路径(§7:小白全程不需要知道配置文件路径)。
function EditModal({
  a,
  models,
  onClose,
  onSave,
  setErr,
}: {
  a: AgentRow;
  models: string[];
  onClose: () => void;
  onSave: () => void;
  setErr: (s: string) => void;
}) {
  const [model, setModel] = useState(a.model || (a.id === "opencode" ? "" : models[0]) || "");
  const [saving, setSaving] = useState(false);
  const [showToken, setShowToken] = useState(false);

  const save = async () => {
    if (saving || !model) return;
    setSaving(true);
    try {
      await postJson(`/admin/agents/${a.id}/model`, { model });
      onSave();
    } catch (e) {
      setErr(`设置 ${a.name} 模型失败:${e}`);
      setSaving(false);
    }
  };

  return (
    <Dialog label={`编辑 ${a.name} 模型`} busy={saving} onClose={onClose}>
        <div className="modal-header"><h2>{a.name} 使用模型</h2></div>
        <div className="modal-body">
        <div className="field">
          <label htmlFor="agent-model">使用模型</label>
          <select id="agent-model" aria-label={`${a.name} 使用模型`} value={model} disabled={saving} onChange={e => setModel(e.target.value)}>
            {a.id === "opencode" && !a.model && <option value="" disabled>沿用 OpenCode 原默认</option>}
            {a.model && !models.includes(a.model) && <option value={a.model}>{a.model}</option>}
            {models.map(m => (
              <option key={m} value={m}>
                {m === "aubridge" ? "自动(aubridge)" : m}
              </option>
            ))}
          </select>
        </div>
        {a.token && (
          <div className="field">
            <span className="section-label">本 Agent token</span>
            <div className="row">
              <code>{showToken ? a.token : "••••••••"}</code>
              <button className="ghost" aria-pressed={showToken} onClick={() => setShowToken(!showToken)}>{showToken ? "隐藏" : "显示"}</button>
              <CopyBtn key={`${a.id}:${a.token}`} text={a.token} id={`复制 ${a.name} Agent token`} />
            </div>
          </div>
        )}
        <p className="muted">保存后立即更新此 Agent 的模型，无需重启。</p>
        </div>
        <div className="modal-actions">
          <button className="ghost" disabled={saving} onClick={onClose}>取消</button>
          <button className="button-primary" onClick={save} disabled={saving || !model}>
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
    </Dialog>
  );
}

function DisconnectModal({ agent, preview, busy, onClose, onConfirm }: {
  agent: AgentRow;
  preview: DisconnectPreview;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Dialog label={`预览断开 ${agent.name}`} busy={busy} onClose={onClose}>
        <div className="modal-header"><h2>断开 {agent.name}</h2></div>
        <div className="modal-body">
          <p className="muted">确认前，核对以下配置恢复操作。</p>
          {preview.changes.length ? (
            <ul>
              {preview.changes.map(change => (
                <li key={change.field}>
                  <code>{change.field}</code>{" "}
                  {change.action === "restore" ? "恢复备份值" : change.action === "remove" ? "移除接线时添加的值" : "保留现值"}
                </li>
              ))}
            </ul>
          ) : <p>没有受 aubridge 管理的字段。</p>}
          {preview.blocked && <p className="alert error">受管配置已被改写。请先核对当前设置，再回来预览。</p>}
        </div>
        <div className="modal-actions">
          <button className="ghost" disabled={busy} onClick={onClose}>取消</button>
          <button className="button-danger" disabled={busy || preview.blocked} onClick={onConfirm}>{busy ? "断开中…" : "确认断开"}</button>
        </div>
    </Dialog>
  );
}

// Agents 页(ui-design §6.1,一屏约束):顶部汇总卡,下部生效 agent
// 清单。汇总 = 原仪表盘压缩(分享卡/源健康/请求计数);请求明细
// 移代理页。切换失败 setErr 不回滚——数据重载揭示真实状态。
export function Agents({
  setErr,
  go,
}: {
  setErr: (s: string) => void;
  go: (t: "agents" | "providers" | "models" | "proxy" | "plugins") => void;
}) {
  const [data, setData] = useState<AgentsPage | null>(null);
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [dashboardFailed, setDashboardFailed] = useState(false);
  const [provs, setProvs] = useState<ProvidersPage | null>(null);
  const [editing, setEditing] = useState<AgentRow | null>(null);
  const [configuring, setConfiguring] = useState<{ agent: AgentRow; tab: "form" | "test" } | null>(null);
  const [disconnect, setDisconnect] = useState<{ agent: AgentRow; preview: DisconnectPreview } | null>(null);
  const [disconnectBusy, setDisconnectBusy] = useState(false);

  const load = () => {
    fetchJson<AgentsPage>("/admin/agents")
      .then(setData)
      .catch(e => setErr(String(e)));
    fetchJson<Dashboard>("/admin/dashboard")
      .then(next => {
        setDash(next);
        setDashboardFailed(false);
      })
      .catch(e => {
        setDashboardFailed(true);
        setErr(String(e));
      });
    fetchJson<ProvidersPage>("/admin/providers")
      .then(setProvs)
      .catch(e => setErr(String(e)));
  };
  useEffect(load, [setErr]);

  if (!data) return <p className="empty-state">正在读取 Agents…</p>;

  const agents = data.agents || [];
  const models = data.models || ["aubridge"];

  const toggle = async (a: AgentRow, on: boolean) => {
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
    }
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
  const allProvs = provs?.providers || [];
  const recentAvailable = !dashboardFailed && Array.isArray(dash?.recent_decisions);
  const recentUnavailable = dashboardFailed || (dash !== null && !recentAvailable);
  const decisions = dash?.recent_decisions || [];
  const completed = decisions.filter(d => d.outcome !== "cancelled");
  const successful = completed.filter(d => d.outcome === "ok");
  const rate = completed.length ? Math.round((successful.length / completed.length) * 100) : null;
  const sources = { free_tier: 0, subscription: 0, unknown: 0 };
  for (const d of successful) sources[successSource(d.provider, d.attempts)]++;
  const showSources = !recentUnavailable && Array.isArray(dash?.recent_decisions) &&
    (successful.length > 0 || decisions.length === 0);

  // 引导卡(§7 状态驱动,就绪后消失):①有源但全无 key → 去
  // Provider 粘贴;②有 key 但无接线 agent → 回本页接线。
  const providersAvailable = Array.isArray(provs?.providers);
  const keyless = providersAvailable && (allProvs.length === 0 || allProvs.every(p => !p.ready));
  const pausedOnly = allProvs.some(p => p.ready) && allProvs.every(p => !p.ready || p.paused);
  const needWire = !agents.some(a => supportsWiring(a.id) && a.wired);
  const showGuide = data.configured && providersAvailable && (keyless || pausedOnly || needWire);

  const statsUnavailable = dashboardFailed || dash?.monthly_savings_status === "unavailable";
  const savings = statsUnavailable ? null : dash?.monthly_savings_usd;
  const shareCard = savings == null ? null : dash?.share_card ?? null;

  return (
    <div className="agents-page">
      <div className="summary-grid agents-summary">
        <div className="summary-card">
          <span className="summary-label metric-label">本月对标节省</span>
          {savings == null ? (
            <span className="summary-value metric-value">{dash || dashboardFailed ? "暂不可用" : "读取中"}</span>
          ) : (
            <>
              <span className="summary-value metric-value">${savings.toFixed(1)}</span>
              <span className="summary-note metric-note">按已报告用量，部分仅计已知用量</span>
            </>
          )}
          <span className="metric-action"><CopyBtn key={shareCard} text={shareCard} label="复制分享卡" /></span>
        </div>
        <div className="summary-card">
          <span className="summary-label metric-label">源状态</span>
          <span className="summary-value metric-value">{healthAvailable ? `${healthy}/${dashProvs.length}` : dash || dashboardFailed ? "暂不可用" : "读取中"}</span>
          <span className="summary-note metric-note">{healthAvailable ? "正常 / 全部模型源" : "源健康尚未确认"}</span>
        </div>
        <div className="summary-card">
          <span className="summary-label metric-label">最近请求</span>
          <span className="summary-value metric-value">{recentUnavailable ? "暂不可用" : !recentAvailable ? "读取中" : decisions.length}</span>
          <span className="summary-note metric-note">{recentAvailable ? rate != null ? `成功率 ${rate}%` : decisions.length ? "取消请求不计成功率" : "暂无请求记录" : "请求统计尚未确认"}</span>
          {showSources && (
            <span className="summary-note metric-note">免费 {sources.free_tier} · 订阅 {sources.subscription} · 未知 {sources.unknown}</span>
          )}
          <button className="ghost metric-action" onClick={() => go("proxy")}>查看请求</button>
        </div>
      </div>

      {showGuide && (
        <div className="guide setup-guide">
          <div className="guide-copy">{keyless ? "先设置一个模型源，保存凭据后即可接线。" : pausedOnly ? "已就绪的源均已暂停，恢复一个模型源后继续。" : "模型源已就绪，在下方接线一个 Agent 即可开始。"}</div>
          {(keyless || pausedOnly) && <button className="button-primary" onClick={() => go("providers")}>{keyless ? "设置模型源" : "恢复模型源"}</button>}
        </div>
      )}

      {!data.configured ? (
        <p className="empty-state">Agent 接线未配置</p>
      ) : (
        <>
          <div className="section-head"><h2>Agents</h2><span className="muted">连接到本机网关</span></div>
          <div className="agent-list data-list">
              {agents.map(a => (
                <div className="agent-item agent-row" key={a.id}>
                  <div className="agent-identity">
                    <strong>{a.name}</strong>
                    <div>
                      {!supportsWiring(a.id) ? (
                        <span className="muted">协议待接通</span>
                      ) : a.drift === "replaced" ? (
                        <span className="badge bad">接线字段被第三方改写</span>
                      ) : a.drift === "port-changed" ? (
                        <span className="badge warn">网关已换端口，可重新接线</span>
                      ) : !a.detected ? (
                        <span className="muted">{a.id === "opencode" ? "未发现配置" : "未安装"}</span>
                      ) : a.wired ? (
                        <span className="badge success">已接线</span>
                      ) : (
                        <span className="muted">未接线</span>
                      )}
                    </div>
                  </div>
                  <div className="agent-model">
                    {a.model ? (
                      a.model === "aubridge" ? (
                        <span>自动(aubridge)</span>
                      ) : (
                        a.model
                      )
                    ) : (
                      <span className="muted">{a.id === "opencode" ? "沿用 OpenCode 原默认" : "在 Agent 内选择模型"}</span>
                    )}
                  </div>
                  <div className="agent-actions actions">
                    {(a.id === "claude-code" || (a.id === "codex" && a.wired) ||
                      (a.id === "opencode" && a.wired && a.drift === "clean")) && (
                      <button
                        className="ghost"
                        onClick={() => {
                          if (a.id === "opencode") setEditing(a);
                          else setConfiguring({ agent: a, tab: "form" });
                        }}
                      >
                        编辑
                      </button>
                    )}
                    {a.wired && (a.id === "claude-code" || a.id === "codex") && (
                      <button className="ghost" onClick={() => setConfiguring({ agent: a, tab: "test" })}>测试</button>
                    )}
                    {a.id === "opencode" && a.wired && a.drift !== "replaced" && (
                      <button className="ghost" onClick={() => toggle(a, true)}>同步模型</button>
                    )}
                    {supportsWiring(a.id) && !a.wired && a.drift && a.drift !== "unwired" && (
                      <button className="ghost" onClick={() => toggle(a, false)}>预览断开</button>
                    )}
                    <label className="agent-wiring row">
                      <span>{a.wired ? "断开" : "接线"}</span>
                      <span className="switch">
                        <input
                          type="checkbox"
                          aria-label={`${a.name} 接线`}
                          checked={a.wired}
                          disabled={!a.wired && (!supportsWiring(a.id) || a.drift === "replaced")}
                          onChange={e => toggle(a, e.target.checked)}
                        />
                        <i></i>
                      </span>
                    </label>
                  </div>
                  {a.wired && a.hint ? <div className="agent-hint"><code className="hint">{a.hint}</code></div> : null}
                </div>
              ))}
              {!agents.length && (
                <p className="empty-state">暂无可接线的 Agent</p>
              )}
          </div>
          <p className="muted agent-footnote">
            接线会将 Agent 的请求指向本机网关。断开前可预览受管字段的恢复操作。
          </p>
        </>
      )}

      {disconnect && (
        <DisconnectModal agent={disconnect.agent} preview={disconnect.preview} busy={disconnectBusy} onClose={() => setDisconnect(null)} onConfirm={confirmDisconnect} />
      )}

      {editing && (
        <EditModal
          a={editing}
          models={models}
          setErr={setErr}
          onClose={() => setEditing(null)}
          onSave={() => {
            setEditing(null);
            load();
          }}
        />
      )}

      {configuring && (
        <AgentConfigModal
          agent={configuring.agent}
          models={models}
          initialTab={configuring.tab}
          setErr={setErr}
          onClose={() => setConfiguring(null)}
          onSaved={load}
        />
      )}
    </div>
  );
}


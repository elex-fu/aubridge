import { useEffect, useRef, useState } from "react";
import { DesktopUpdateStatus, checkDesktopUpdate, desktopUpdateStatus, installDesktopUpdate, prepareDesktopUpdate, prepareDevelopmentUpdate, readableActionError } from "./api";
import { Dialog } from "./Dialog";
import { TaskView } from "./TaskView";

const states: Record<string, string> = {
  not_configured: "尚未配置可信更新源，无法判断是否为最新版本",
  checking: "正在检查可信更新源", current: "可信更新源未发现更新版本", available: "发现可准备的新版本",
  preparing: "正在下载并校验更新包", ready: "更新包已校验并准备，尚未安装",
  busy: "仍有任务运行，请在任务完成后重试安装", installing: "正在安装，App 可能退出；下次启动后查看最终结果",
  installed: "安装记录显示已完成，请同时核对上方当前运行版本",
  rolled_back: "更新未完成，安装记录显示已恢复旧包", failed: "更新未完成，请核对原因后重新准备",
};
const reasons: Record<string, string> = {
  "trusted-channel-not-configured": "此包尚未配置可信发行源，暂时无法通过可信来源检查新版本。",
  "offer-expired-or-unbound": "更新候选已过期，请重新检查。",
  "offer-unavailable": "当前没有可用候选，请重新检查更新。",
  "receipt-hash-mismatch": "发行回执校验不一致，请停止使用此候选并重新检查。",
  "feed-receipt-mismatch": "候选与发行回执不一致，请重新检查。",
  "feed-platform-channel-mismatch": "更新包不适用于当前平台或发行通道。",
  "insufficient-update-space": "安装空间不足，请释放空间后重新准备。",
  "independent-archive-hash-mismatch": "独立确认的 ZIP 校验值与发行回执不一致。",
  "archive-hash-size-mismatch": "本地 ZIP 的大小或校验值不一致，请重新取得正确归档。",
  "explicit-development-confirmation-required": "本地开发包需要独立 ZIP 校验值与明确的开发验证确认。",
  "prepared-update-unavailable": "已准备的更新不可用，请重新准备。",
  "prepared-handle-unbound": "准备回执已变化，请重新读取状态并准备更新。",
  "installed-app-changed-since-prepare": "安装目录自准备后被修改，请核对当前 App 后重新准备。",
  "prepared-journal-changed": "安装记录已经改变，请重新读取状态。",
  "trusted-helper-not-ready": "可信安装助手未就绪，App 继续运行，请稍后重新准备或重试。",
  "interrupted-update-needs-recovery": "上次更新尚待恢复，请重新打开 App 并读取恢复结果。",
};
const explain = (error: unknown) => {
  const code = error instanceof Error ? error.message : String(error);
  return reasons[code] || readableActionError(error, "更新操作未完成，请重新读取状态；详细原因可在下方查看。");
};

export function UpdatePanel({ onBusyChange, onErrorChange, onTaskActiveChange }: { onBusyChange?: (busy: boolean) => void; onErrorChange?: (message: string) => void; onTaskActiveChange?: (active: boolean) => void }) {
  const [status, setStatus] = useState<DesktopUpdateStatus | null>(null);
  const [reading, setReading] = useState(false);
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [errorCode, setErrorCode] = useState("");
  const [confirmInstall, setConfirmInstall] = useState(false);
  const [archive, setArchive] = useState("");
  const [receipt, setReceipt] = useState("");
  const [sha, setSha] = useState("");
  const [developmentConfirmed, setDevelopmentConfirmed] = useState(false);
  const developmentTrigger = useRef<HTMLButtonElement | null>(null);
  const [developmentTask, setDevelopmentTask] = useState(false);
  const [developmentVisited, setDevelopmentVisited] = useState(false);
  const diagnosticsTrigger = useRef<HTMLButtonElement | null>(null);
  const [diagnosticsTask, setDiagnosticsTask] = useState(false);
  const [diagnosticsVisited, setDiagnosticsVisited] = useState(false);
  const [checked, setChecked] = useState(false);
  const generation = useRef(0);
  const pending = useRef(false);
  const alive = useRef(true);
  const closeDevelopment = () => { setDevelopmentTask(false); window.requestAnimationFrame(() => developmentTrigger.current?.focus()); };
  const read = async () => {
    const current = ++generation.current;
    setReading(true);
    try {
      const next = await desktopUpdateStatus();
      if (alive.current && current === generation.current) setStatus(next);
      return next;
    } finally { if (alive.current && current === generation.current) setReading(false); }
  };
  useEffect(() => {
    alive.current = true;
    void read().catch(failure => { if (alive.current) { setError(explain(failure)); setErrorCode(String(failure)); } });
    return () => { alive.current = false; generation.current++; };
  }, []);
  useEffect(() => {
    if (status?.state !== "installing") return;
    const timer = window.setInterval(() => void read().catch(() => {}), 3000);
    return () => window.clearInterval(timer);
  }, [status?.state]);
  const run = async (label: string, action: () => Promise<DesktopUpdateStatus>) => {
    if (pending.current) return;
    pending.current = true; setBusy(label); setError(""); setErrorCode("");
    const timer = window.setInterval(() => void read().catch(() => {}), 2000);
    try {
      const next = await action();
      generation.current++;
      if (alive.current) { setStatus(next); if (next.state === "installing") setConfirmInstall(false); if (developmentTask && next.prepared_handle && (next.state === "ready" || next.state === "busy")) closeDevelopment(); }
    } catch (failure) {
      if (alive.current) {
        setError(explain(failure)); setErrorCode(String(failure));
        await read().catch(() => {});
      }
    } finally {
      window.clearInterval(timer); pending.current = false;
      if (alive.current) { setBusy(""); setReading(false); }
    }
  };
  const phase = status?.state;
  const installing = phase === "installing";
  const locked = Boolean(busy) || installing || phase === "checking" || phase === "preparing";
  useEffect(() => { onBusyChange?.(locked || confirmInstall); }, [locked, confirmInstall, onBusyChange]);
  const visibleFailure = error || (phase === "failed" || phase === "rolled_back" ? explain(status?.reason || states[phase]) : "");
  useEffect(() => { onErrorChange?.(visibleFailure); }, [visibleFailure, onErrorChange]);
  useEffect(() => { onTaskActiveChange?.(developmentTask || diagnosticsTask); }, [developmentTask, diagnosticsTask, onTaskActiveChange]);
  const canInstall = (phase === "ready" || phase === "busy") && Boolean(status?.prepared_handle);
  const validDevelopment = archive.trim().startsWith("/") && receipt.trim().startsWith("/") && /^[a-fA-F0-9]{64}$/.test(sha.trim()) && developmentConfirmed;
  const refresh = () => { setError(""); setErrorCode(""); void read().catch(failure => { setError(explain(failure)); setErrorCode(String(failure)); }); };
  return <section className="panel" aria-labelledby="update-title">
    <div className="panel-heading"><h3 id="update-title" tabIndex={-1}>检查与安装更新</h3><button className="ghost" disabled={Boolean(busy) || reading} onClick={refresh}>重新读取状态</button></div>
    <p role="status">{status ? status.state === "current" && !checked ? "尚未取得本窗口的更新检查结果，可按需检查可信更新源。" : states[status.state] || "更新状态尚未确认" : error ? "更新状态不可读取，请从已安装的 macOS 桌面 App 使用此功能。" : "正在读取安装状态…"}</p>
    {status?.version && <p>候选版本：<code>{status.version}</code></p>}
    {status?.reason && <p className="muted">{explain(status.reason)}</p>}
    {error && <p className="alert error" role="alert">{error}</p>}
    <div className="task-workspace-pane" hidden={developmentTask || diagnosticsTask}>
    <div className="button-row">
      <button className={phase === "available" || canInstall ? "ghost" : "button-primary"} disabled={locked || canInstall || !status || reading} onClick={() => void run("检查更新中…", async () => { const next = await checkDesktopUpdate(); if (alive.current) setChecked(true); return next; })}>检查更新</button>
      {phase === "available" && status?.offer_handle && <button className="button-primary" disabled={locked} onClick={() => void run("校验更新包中…", () => prepareDesktopUpdate(status.offer_handle!))}>下载并校验更新包</button>}
      {canInstall && <button className="button-primary" disabled={locked} onClick={() => setConfirmInstall(true)}>{phase === "busy" ? "任务结束后重试安装" : "确认安装已准备的版本"}</button>}
      {busy && <span className="muted" role="status">{busy}</span>}
    </div>
    <p className="muted">仅在任务结束后手动安装；运行中的请求与验证任务不会被强停。App 将退出并重新打开，客户端可能需要重新连接。下次打开 App 后将显示最终结果，可同时核对当前运行版本。</p>
    </div>
    <div className="task-workspace-pane" hidden={developmentTask || diagnosticsTask}><button className="ghost" disabled={locked} onClick={event => { developmentTrigger.current = event.currentTarget; setDevelopmentVisited(true); setDevelopmentTask(true); }}>校验本地开发包（不是公众更新）</button></div>
    {developmentVisited && <div className="task-workspace-pane" hidden={!developmentTask}><TaskView title="校验本地开发包" onBack={closeDevelopment} busy={locked || confirmInstall}>
      <p className="muted">此入口不代表 Developer ID 签名、公证或独立设备验收通过。必须提供本地 ZIP、schema2 发行回执，以及从独立渠道确认的 ZIP SHA-256；不会从回执自动填入校验值。</p>
      <label className="field">本地 ZIP 绝对路径<input value={archive} disabled={locked || canInstall || !status} onChange={event => { setArchive(event.target.value); setDevelopmentConfirmed(false); }} /></label>
      <label className="field">发行回执绝对路径<input value={receipt} disabled={locked || canInstall || !status} onChange={event => { setReceipt(event.target.value); setDevelopmentConfirmed(false); }} /></label>
      <label className="field">独立确认的 ZIP SHA-256<input value={sha} autoComplete="off" spellCheck={false} disabled={locked || canInstall || !status} onChange={event => { setSha(event.target.value); setDevelopmentConfirmed(false); }} /></label>
      <label className="row"><input type="checkbox" checked={developmentConfirmed} disabled={locked || canInstall || !status} onChange={event => setDevelopmentConfirmed(event.target.checked)} />我已独立核对本地归档及其校验值，明确仅进行开发验证</label>
      <button disabled={locked || canInstall || !status || !validDevelopment} onClick={() => void run("校验本地开发包中…", () => prepareDevelopmentUpdate(archive.trim(), receipt.trim(), sha.trim().toLowerCase()))}>校验并准备本地开发包</button>
    </TaskView></div>}

    {(errorCode || status?.reason) && <div className="task-workspace-pane" hidden={developmentTask || diagnosticsTask}><button className="ghost" disabled={locked || confirmInstall} onClick={event => { diagnosticsTrigger.current = event.currentTarget; setDiagnosticsVisited(true); setDiagnosticsTask(true); }}>查看更新原因标识</button></div>}
    {diagnosticsVisited && <div className="task-workspace-pane" hidden={!diagnosticsTask}><TaskView title="更新原因与诊断标识" busy={locked || confirmInstall} onBack={() => { setDiagnosticsTask(false); window.requestAnimationFrame(() => { if (diagnosticsTrigger.current?.isConnected) diagnosticsTrigger.current.focus(); else document.getElementById("update-title")?.focus(); }); }}>
      <p>{visibleFailure || (status?.reason ? explain(status.reason) : "当前没有更新失败原因。")}</p>
      <p className="muted">更新原因标识：<code>{errorCode || status?.reason || "无"}</code></p>
    </TaskView></div>}
    {confirmInstall && <Dialog label="确认退出并安装更新" role="alertdialog" busy={Boolean(busy)} onClose={() => setConfirmInstall(false)}>
      <div className="modal-header"><h2>安装候选版本 {status?.version || "未知"}？</h2></div>
      <div className="modal-body"><p>将退出当前 App，安装已校验的候选，再打开新 App。请先结束当前任务；若仍有活动，服务端会拒绝安装并保留正在运行的任务。</p><p className="muted">安装失败时会尝试恢复旧包。重新打开后，请核对当前运行版本与更新结果。</p>{error && <p className="alert error" role="alert">{error}</p>}</div>
      <div className="modal-actions"><button disabled={Boolean(busy)} onClick={() => setConfirmInstall(false)}>暂不安装</button><button className="button-primary" disabled={Boolean(busy) || !canInstall} onClick={() => { if (status?.prepared_handle) void run("正在交接安装…", () => installDesktopUpdate(status.prepared_handle!)); }}>确认退出并安装</button></div>
    </Dialog>}
  </section>;
}

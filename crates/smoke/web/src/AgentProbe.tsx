import { useEffect, useRef, useState } from "react";
import { AgentRow, AgentTestResult, postJson } from "./api";
import { Dialog } from "./Dialog";
import { TaskView } from "./TaskView";

export function AgentProbe({ agent, onClose, embedded = false, onBusyChange, disabled = false }: {
  agent: AgentRow; onClose: () => void; embedded?: boolean; onBusyChange?: (busy: boolean) => void; disabled?: boolean;
}) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<AgentTestResult | null>(null);
  const [error, setError] = useState("");
  const [logs, setLogs] = useState(false);
  const [testedModel, setTestedModel] = useState("");
  const pending = useRef(false);
  const alive = useRef(true);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  useEffect(() => { onBusyChange?.(busy); return () => onBusyChange?.(false); }, [busy, onBusyChange]);
  const run = async () => {
    if (pending.current || disabled || !agent.wired) return;
    pending.current = true; setBusy(true); setError(""); setLogs(false); setResult(null);
    setTestedModel(agent.model || "自动(aubridge)");
    try {
      const next = await postJson<AgentTestResult>(`/admin/agents/${agent.id}/test`, {});
      if (alive.current) setResult(next);
    } catch (failure) { if (alive.current) { setResult(null); setError(String(failure)); } }
    finally { pending.current = false; if (alive.current) setBusy(false); }
  };
  const hasLogs = Boolean(result?.stdout || result?.stderr);
  const body = logs && hasLogs ? <TaskView title={`${agent.name} 短文本测试日志`} onBack={() => setLogs(false)} busy={busy}>
    {error && <p className="alert error" role="alert">{error}</p>}
    <p><span className={`badge ${result?.ok ? "success" : "bad"}`}>{result?.ok ? "短文本通过" : "短文本失败"}</span> {result?.message}</p>
    <p className="muted">上次测试启动时的配置模型：{testedModel}；不代表当前连接或文件工具已经验证。</p>
    {result?.stdout && <><h4>标准输出</h4><pre className="task-log">{result.stdout}</pre></>}
    {result?.stderr && <><h4>客户端日志</h4><pre className="task-log">{result.stderr}</pre></>}
  </TaskView> : <>
    <p>向 {agent.model || "自动(aubridge)"} 发送一次短文本测试，会消耗所选模型的额度；不验证文件工具或原项目。</p>
    <p className="muted">使用此 Agent 当前已保存的受管配置。只有点击开始测试后才调用 CLI，最多等待 150 秒。</p>
    {!agent.wired && <p className="alert">请先连接当前 Agent，再运行短文本测试。</p>}
    {error && <p className="alert error" role="alert">{error}</p>}
    {result && <div role="status"><p><span className={`badge ${result.ok ? "success" : "bad"}`}>{result.ok ? "短文本通过" : "短文本失败"}</span> {result.message} · {(result.duration_ms / 1000).toFixed(1)} 秒 · 退出码 {result.exit_code ?? "未知"}{result.timed_out ? " · 已超时" : ""}</p><p className="muted">上次测试启动时的配置模型：{testedModel}。{testedModel !== (agent.model || "自动(aubridge)") ? "当前模型已改变，请重新测试。" : "不代表文件工具或原项目验证通过。"}</p></div>}
    {hasLogs && <button className="ghost" disabled={busy} onClick={() => setLogs(true)}>查看本次测试日志</button>}
    <button className="button-primary" disabled={busy || disabled || !agent.wired} onClick={() => void run()}>{busy ? "测试中…" : "开始测试"}</button>
  </>;
  if (embedded) return logs && hasLogs ? body : <TaskView title={`${agent.name} 短文本连接测试`} onBack={onClose} busy={busy}>{body}</TaskView>;
  return <Dialog label={`${agent.name} 短文本连接测试`} busy={busy} onClose={onClose}>
    <div className="modal-header"><h2>{agent.name} 短文本连接测试</h2></div>
    <div className="modal-body">{body}</div>
    <div className="modal-actions"><button className="ghost" disabled={busy} onClick={onClose}>关闭</button></div>
  </Dialog>;
}

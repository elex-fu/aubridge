import { useEffect, useRef, useState } from "react";
import { fetchJson, postJson } from "./api";
import { fmtTime } from "./format";

type NativeSession = { handle: string; agent: string; project_alias: string; updated_at: number; state: "ready" | "read_only"; reason?: string | null };
type NativeSessionsPage = { sessions: NativeSession[]; agents: { agent: string; state: "ready" | "read_only"; reason?: string | null }[] };
const names: Record<string, string> = { "claude-code": "Claude Code", codex: "Codex", opencode: "OpenCode", gemini: "Gemini CLI" };
const reasons: Record<string, string> = {
  platform_unsupported: "当前平台不支持从 App 启动原生终端恢复",
  not_wired: "请先到 Agents 连接此 CLI，再刷新此列表",
  cli_check_required: "请先到 Agents 检查 CLI 与版本，再刷新此列表",
  native_adapter_pending: "当前 CLI 原生记录适配待验证",
  opaque_unverified: "无法证明加密会话的账号归属，请在原 CLI 核对账号",
  workspace_missing: "原项目目录缺失，请恢复原目录后刷新",
  identity_mismatch: "连接或账号条件已经改变，请核对当前 CLI 与登录",
  project_context_conflict: "原项目配置可能遮蔽当前受管连接，请先到 Agents 检查项目启动环境",
  project_context_unreadable: "项目环境无法读取，请先到 Agents 核对原项目目录",
  profile_mismatch: "原会话的配置不属于当前受管连接，请核对原 CLI 配置",
  child_session: "这是子会话，请从原主会话继续",
  scan_limit: "已达到本轮记录扫描上限，部分历史尚未读取",
  record_unreadable: "原生会话记录不可读，请在原 CLI 中检查",
  record_changed: "原生记录已变化，请刷新列表后重新选择",
  unsafe_path: "原项目路径无法安全确认，请在本机核对目录",
  unsupported_schema: "当前原生记录格式尚不支持恢复",
  invalid_session_id: "原生会话标识无效，请在原 CLI 中检查",
  gemini_custom_headers_conflict: "自定义请求头需核对，请核对 Gemini 配置与终端环境后刷新",
};
const reasonText = (reason?: string | null) => reason ? reasons[reason] || "暂时无法确认恢复条件，请核对原 CLI 后刷新" : "恢复条件尚未确认";

export function NativeSessions({ refreshKey }: { refreshKey: unknown }) {
  const [page, setPage] = useState<NativeSessionsPage | null>(null);
  const [error, setError] = useState("");
  const [receipt, setReceipt] = useState("");
  const [loading, setLoading] = useState(false);
  const [pending, setPending] = useState("");
  const [revision, setRevision] = useState(0);
  const submitting = useRef(false);
  const alive = useRef(true);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError("");
    fetchJson<NativeSessionsPage>("/admin/native-sessions", { signal: controller.signal, cache: "no-store" })
      .then(value => { if (!controller.signal.aborted) setPage(value); })
      .catch(() => { if (!controller.signal.aborted) setError("原生会话列表暂不可读，请刷新后重试。"); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [revision, refreshKey]);
  const resume = async (session: NativeSession) => {
    if (submitting.current || session.state !== "ready") return;
    submitting.current = true; setPending(session.handle); setError(""); setReceipt("");
    try {
      const next = await postJson<{ state: string; agent: string; project_alias: string }>("/admin/native-sessions/resume", { handle: session.handle });
      if (next.state !== "launched" || next.agent !== session.agent) throw new Error("启动状态未确认");
      if (alive.current) {
        setReceipt(`已请求打开终端：${names[next.agent] || next.agent} · ${next.project_alias}。请在终端确认原对话并完成新的文件工具任务；启动回执不代表恢复验证通过。${next.agent === "gemini" ? "如终端提示自定义请求头需核对，请核对后刷新重试。" : ""}`);
        setRevision(value => value + 1);
      }
    } catch (error) {
      if (alive.current) setError(error instanceof Error && error.message === "gemini_custom_headers_conflict" ? reasons.gemini_custom_headers_conflict : "未确认终端启动。请核对是否已有终端打开，再刷新列表；会话或连接变化时须重新选择。");
    } finally {
      submitting.current = false;
      if (alive.current) setPending("");
    }
  };
  return <section className="panel" aria-labelledby="native-sessions-title">
    <div className="panel-heading"><h2 id="native-sessions-title">继续原对话</h2><button className="ghost" disabled={loading || Boolean(pending)} onClick={() => setRevision(value => value + 1)}>{loading ? "读取中…" : "刷新原生会话"}</button></div>
    <p className="muted">从本机官方 CLI 的原生记录返回原项目。恢复会打开系统终端，沿用当前受管连接；下方诊断会话 ID 只用于查找。</p>
    {receipt && <p role="status">{receipt}</p>}
    {error && <p className="alert error" role="alert">{error}</p>}
    {!page && !error && <p role="status">正在读取原生会话…</p>}
    {page?.sessions.map(session => <div className="native-session-row row" key={session.handle}>
      <div><strong>{names[session.agent] || session.agent}</strong> · {session.project_alias}<p className="muted">{fmtTime(session.updated_at)}{session.state !== "ready" ? ` · ${reasonText(session.reason)}` : " · 原项目与原生会话记录已找到，点击时会重新核对"}</p></div>
      {session.state === "ready" && <button disabled={Boolean(pending) || loading} onClick={() => void resume(session)}>{pending === session.handle ? "启动中…" : "在原项目继续对话"}</button>}
    </div>)}
    {page && !page.sessions.length && <p className="muted">尚未找到可列出的原生会话。可以先从 Agents 正确启动 CLI，在原项目完成一次对话后刷新。</p>}
    {page && <section className="section-details" aria-label="各 CLI 的恢复条件"><h3>各 CLI 的恢复条件</h3>{page.agents.map(agent => <p key={agent.agent}>{names[agent.agent] || agent.agent} · {agent.state === "ready" ? "CLI 检查已就绪" : "只读"}{agent.reason ? ` · ${reasonText(agent.reason)}` : ""}</p>)}</section>}
  </section>;
}

import { useEffect, useRef, useState } from "react";
import { AgentRow, AgentValidationJob, AgentValidationSummary, deleteJson, fetchJson, postJson, readableActionError } from "./api";
import { CopyBtn } from "./CopyButton";
import { TaskView } from "./TaskView";

type ProjectCheck = {
  project_handle: string; project_alias: string; status: "matched" | "could_shadow" | "unknown" | "unreadable";
  reason?: string | null; checked_at: number; verified: false;
  env?: { layer: string; trust: string; base_key_present?: boolean | null; api_key_present?: boolean | null } | null;
  actions: string[];
};
type ProjectEnvPreview = {
  preview_handle: string; project_alias: string; layer: string; operation: "add" | "undo";
  changes: { key: string; action: "add" | "keep" | "remove_owned" }[]; expires_at: number;
};
type ProjectSummary = AgentValidationSummary;
type ProjectJob = AgentValidationJob;
const states: Record<string, string> = { matched: "配置静态检查一致，尚未验证项目任务", could_shadow: "项目配置可能遮蔽受管连接", unknown: "项目配置作用域尚未确认", unreadable: "项目环境不可读取" };
const jobs: Record<string, string> = { queued: "项目任务排队中", running: "项目文件工具任务运行中", passed: "项目任务已完成", failed: "项目任务未通过", cancelled: "项目任务已取消", timed_out: "项目任务超时" };
const layers: Record<string, string> = { none: "未命中环境文件", unknown: "未知", project_gemini: "项目 .gemini/.env", project_plain: "项目 .env", ancestor_gemini: "父级 .gemini/.env", ancestor_plain: "父级 .env", home_gemini: "用户主目录 .gemini/.env", home_plain: "用户主目录 .env" };
const trusts: Record<string, string> = { trusted: "已信任", untrusted: "未信任", unknown: "未知" };
const checkReasons: Record<string, string> = {
  project_environment_shadow: "优先命中了其他环境文件；即使其中只有无关变量，也可能遮蔽受管连接。请在原项目核对连接配置后重新检查。",
  project_untrusted: "项目未被 CLI 信任，请先在原 CLI 确认项目的信任范围。",
  project_trust_unknown: "无法确定 CLI 对此项目的信任范围，请先在原 CLI 核对。",
  project_configuration_unreadable: "项目配置无法读取或格式不受支持，请检查权限与配置格式。",
  project_auth_or_model_override: "项目设置另有登录或模型选择，请核对是否要沿用当前受管连接。",
  project_connection_override: "项目设置另有接入地址或凭据字段，请核对当前连接。",
  project_profile_override: "项目配置可能覆盖受管 Codex profile，请核对项目配置。",
  project_provider_override: "项目配置另有模型或 aubridge 来源设置，请核对当前连接。",
};
const taskReasons: Record<string, string> = {
  project_external_read_unverified: "未确认 CLI 实际读取项目外的探针。请重新检查项目，并明确同意项目内临时文件后再验证。",
  project_environment_override: "继承的环境可能覆盖受管连接，请核对 CLI 启动环境后重查项目。",
  project_execution_extensions: "检测到外部 MCP、hooks 或扩展，当前安全文件验证拒绝启动。请先在原 CLI 核对这些配置。",
  project_handle_expired: "项目选择已过期，请重新检查原目录。",
  project_changed: "项目目录或配置已经改变，请重新检查。",
  project_context_conflict: "项目配置可能遮蔽当前受管连接，请核对启动环境后重查。",
  "update-maintenance-active": "正在准备更新，请稍后再试。",
  timed_out: "任务已超时，请核对 CLI 和来源状态后重查项目。",
  cancelled: "本次项目任务已取消。",
};
const taskFailure = (failure: unknown) => {
  const message = readableActionError(failure);
  return Object.entries(taskReasons).find(([code]) => message.includes(code))?.[1] || message;
};
const presence = (value?: boolean | null) => value == null ? "未知" : value ? "存在" : "不存在";

export function ProjectTask({ agent, canRun, loading, onReload, onBack, onBusyChange }: { agent: AgentRow; canRun: boolean; loading: boolean; onReload: () => void; onBack: () => void; onBusyChange?: (busy: boolean) => void }) {
  const summary = agent.validation as ProjectSummary | undefined;
  const [historical, setHistorical] = useState(true);
  const [path, setPath] = useState("");
  const [check, setCheck] = useState<ProjectCheck | null>(null);
  const [handle, setHandle] = useState(summary?.scope === "project" ? summary.project_handle || "" : "");
  const [alias, setAlias] = useState(summary?.scope === "project" ? summary.project_alias || "" : "");
  const [preview, setPreview] = useState<ProjectEnvPreview | null>(null);
  const [envNotice, setEnvNotice] = useState("");
  const [consent, setConsent] = useState(false);
  const [jobId, setJobId] = useState(summary?.scope === "project" ? summary.last_job_id || "" : "");
  const [jobHandle, setJobHandle] = useState(summary?.scope === "project" ? summary.project_handle || "" : "");
  const [job, setJob] = useState<ProjectJob | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [uncertain, setUncertain] = useState(false);
  const [readState, setReadState] = useState<"idle" | "reading" | "ready" | "failed">(jobId ? "reading" : "idle");
  const [newPrepared, setNewPrepared] = useState(false);
  const [retry, setRetry] = useState(0);
  const [view, setView] = useState<"project" | "evidence">("project");
  const pendingStart = useRef<{ handle: string; priorJobId: string } | null>(null);
  const summaryWasTracked = useRef(Boolean(jobId && jobHandle && summary?.scope === "project" && summary.last_job_id === jobId && summary.project_handle === jobHandle));
  const pending = useRef(false);
  const alive = useRef(true);
  const reload = useRef(onReload);
  reload.current = onReload;
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  useEffect(() => {
    const intent = pendingStart.current;
    if (intent && summary?.scope === "project" && summary.project_handle === intent.handle && summary.last_job_id && summary.last_job_id !== intent.priorJobId) {
      setJobHandle(intent.handle); setJobId(summary.last_job_id); setReadState("reading"); setNewPrepared(false); setUncertain(false); setError(""); pendingStart.current = null;
    }
  }, [handle, summary?.scope, summary?.project_handle, summary?.last_job_id]);
  useEffect(() => {
    if (historical && !pendingStart.current && summary?.scope === "project" && summary.last_job_id) { setJobHandle(summary.project_handle || ""); setJobId(summary.last_job_id); }
  }, [summary?.scope, summary?.last_job_id, historical]);
  useEffect(() => {
    const lostRecord = summaryWasTracked.current && summary?.status === "not_run" && !summary.last_job_id;
    summaryWasTracked.current = Boolean(jobId && jobHandle && summary?.scope === "project" && summary.last_job_id === jobId && summary.project_handle === jobHandle);
    if (lostRecord && jobId) { setReadState("reading"); setRetry(value => value + 1); }
  }, [jobId, jobHandle, summary?.scope, summary?.last_job_id, summary?.project_handle, summary?.status]);
  useEffect(() => {
    if (!jobId) return;
    let active = true;
    let timer: number | undefined;
    setJob(null); setReadState("reading");
    const read = async () => {
      try {
        const next = await fetchJson<ProjectJob>(`/admin/agent-validation-jobs/${encodeURIComponent(jobId)}`);
        if (!active) return;
        if (next.agent_id !== agent.id || next.job_id !== jobId || next.scope !== "project" || !jobHandle || next.project_handle !== jobHandle) throw new Error("任务身份不匹配");
        setJob(next); setReadState("ready"); setError("");
        if (next.status === "queued" || next.status === "running") setNewPrepared(false);
        if (next.status === "queued" || next.status === "running") timer = window.setTimeout(read, 1500);
        else reload.current();
      } catch { if (active) { setReadState("failed"); setError("无法核实前次原项目任务是否已执行或结束。可按原任务 ID 与项目范围重新读取；不会自动再次调用模型。"); } }
    };
    void read();
    return () => { active = false; window.clearTimeout(timer); };
  }, [agent.id, jobId, jobHandle, retry]);
  const summaryMatches = Boolean(jobId && jobHandle && summary?.scope === "project" && summary.last_job_id === jobId && summary.project_handle === jobHandle);
  const readable = readState === "ready" && job?.agent_id === agent.id && job.job_id === jobId && job.scope === "project" && Boolean(jobHandle) && job.project_handle === jobHandle && !pendingStart.current;
  const status = readable && job ? job.status : summaryMatches ? summary!.status : "not_run";
  const running = Boolean(jobId) && readState !== "failed" && !pendingStart.current && (status === "queued" || status === "running");
  const restoring = Boolean(jobId && readState === "reading");
  const unresolved = Boolean(pendingStart.current) || readState === "failed" || !jobId && historical && summary?.scope === "project" && summary.status !== "not_run";
  useEffect(() => { onBusyChange?.(busy || running || restoring); return () => onBusyChange?.(false); }, [busy, running, restoring, onBusyChange]);
  const currentPass = readable && summaryMatches && status === "passed" && summary?.valid === true && summary.scope === "project" &&
    Boolean(handle) && summary.project_handle === handle && job?.project_handle === handle &&
    job.evidence?.tool_result_matched === true && job.evidence.final_exact === true && job.evidence.requests.length >= 2;
  const observedAt = summaryMatches ? summary?.observed_at : job?.finished_at;
  const needsRecheck = !running && !currentPass && status !== "not_run" && Boolean(handle) &&
    summary?.scope === "project" && summary.project_handle === handle;
  const mutate = async (action: () => Promise<void>) => {
    if (pending.current) return;
    pending.current = true; setBusy(true); setError("");
    try { await action(); }
    catch (failure) {
      if (alive.current) { setUncertain(true); setError(`操作结果未确认：${taskFailure(failure)}`); onReload(); }
    } finally { pending.current = false; if (alive.current) setBusy(false); }
  };
  const inspect = () => mutate(async () => {
    if (unresolved && !newPrepared) return;
    setHistorical(false); setPreview(null); setEnvNotice(""); setConsent(false);
    const next = await postJson<ProjectCheck>(`/admin/agents/${agent.id}/project-check`, { path: path.trim() });
    if (alive.current) {
      setCheck(next); setHandle(next.project_handle); setAlias(next.project_alias); setConsent(false);
      if (!unresolved) { setJobId(""); setJobHandle(""); setJob(null); setReadState("idle"); setUncertain(false); setNewPrepared(false); }
    }
  });
  const projectEnv = (operation: "add" | "undo") => mutate(async () => {
    setHistorical(false); setPreview(null); setConsent(false); setEnvNotice(""); setNewPrepared(false);
    if (!unresolved) { pendingStart.current = null; setJobId(""); setJobHandle(""); setJob(null); setReadState("idle"); }
    try {
      const next = await postJson<ProjectEnvPreview>("/admin/agents/gemini/project-env/preview", { project_handle: handle, operation });
      if (alive.current) setPreview(next);
    } catch {
      if (alive.current) { setCheck(null); setHandle(""); setError("连接方案已失效或条件有变化，请重新检查项目。请核对当前项目状态。"); }
    }
  });
  const applyProjectEnv = () => mutate(async () => {
    if (!preview) return;
    const operation = preview.operation;
    const previewHandle = preview.preview_handle;
    setHistorical(false); setPreview(null); setConsent(false); setCheck(null); setHandle(""); setNewPrepared(false);
    if (!unresolved) { pendingStart.current = null; setJobId(""); setJobHandle(""); setJob(null); setReadState("idle"); }
    try {
      const next = await postJson<ProjectCheck>("/admin/agents/gemini/project-env/apply", { preview_handle: previewHandle, confirmed: true });
      if (!alive.current) return;
      setCheck(next); setHandle(next.project_handle); setAlias(next.project_alias); setUncertain(false);
      setEnvNotice(`${operation === "undo" ? "已撤销" : "已补齐"}本项目连接；该项目尚未验证。`);
      onReload();
    } catch {
      if (alive.current) { setError("操作未确认或方案已失效，请重新检查项目；不会自动重试。"); onReload(); }
    }
  });
  const start = () => mutate(async () => {
    if (!canRun || !handle || !consent || !check?.actions.includes("run_project_task") || running || restoring || unresolved && !newPrepared) return;
    setNewPrepared(false);
    pendingStart.current = { handle, priorJobId: summary?.last_job_id || jobId };
    const next = await postJson<ProjectJob>(`/admin/agents/${agent.id}/validation-jobs`, { scope: "project", project_handle: handle, allow_project_temp_file: true });
    if (!alive.current) return;
    if (next.agent_id !== agent.id || next.scope !== "project" || next.project_handle !== handle || !next.job_id) throw new Error("项目任务归属未确认");
    pendingStart.current = null; setUncertain(false); setReadState("reading"); setJobHandle(handle); setJob(next); setJobId(next.job_id); onReload();
  });
  const cancel = () => mutate(async () => {
    await deleteJson(`/admin/agent-validation-jobs/${encodeURIComponent(jobId)}`);
    if (alive.current) setRetry(value => value + 1);
  });
  const readAgain = () => { setError(""); if (!pendingStart.current) setUncertain(false); onReload(); if (jobId) setRetry(value => value + 1); };
  const label = running ? "取消项目任务" : restoring ? "正在读取项目任务…" : unresolved && !newPrepared || uncertain && !newPrepared || error && jobId && !newPrepared ? "重新读取任务状态" : currentPass ? "复制正确启动命令" : check?.actions.includes("run_project_task") && handle && (!needsRecheck || newPrepared) ? "开始新的原项目验证" : "只读检查项目";
  if (view === "evidence" && job?.evidence) return <TaskView title={`${agent.name} 原项目任务证据`} onBack={() => setView("project")} busy={busy || running || restoring}>
    <p>{jobs[job.status]} · {job.project_alias || "项目未知"} · {observedAt ? new Date(observedAt * 1000).toLocaleString() : "时间未知"}</p>
    {error && <p className="alert error" role="alert">{error}</p>}
    {job.reason && <p>任务结果：{taskReasons[job.reason] || "任务未完成，请核对项目、CLI 与来源状态"}（<code>{job.reason}</code>）</p>}
    {job.evidence.requests.map(request => <p key={request.request_id}>实际来源 {request.provider} · 模型 {request.served_model} · 请求 <code>{request.request_id}</code></p>)}
  </TaskView>;
  return <TaskView title={`${agent.name} 原项目检查与任务`} onBack={onBack} busy={busy || running || restoring}>
    <p role="status">{unresolved ? "无法核实前次原项目验证是否已执行或结束；准备新验证不表示旧任务已取消。" : currentPass ? `${agent.name} 已在 ${alias || job?.project_alias || "所选项目"} 完成文件工具任务` : running ? jobs[status] : status === "passed" ? "这是历史项目结果，当前目录或连接条件需重新验证" : status !== "not_run" ? `${jobs[status] || "项目任务状态尚未确认"}，请核对项目配置后再决定是否新运行。` : check ? states[check.status] : "选择原项目，先只读检查配置作用域；检查一致仍不代表项目已经可用。"}</p>
    {!jobId && !pendingStart.current && <p className="muted">当前没有可恢复的验证记录；这不表示此前从未执行。</p>}
    {newPrepared && <p role="status">已准备一次新的原项目验证，尚未调用模型。点击开始将独立使用额度，可能产生费用；不表示前次任务已取消或未执行。</p>}
    <label className="field" htmlFor={`project-path-${agent.id}`}>项目目录的绝对路径
      <input id={`project-path-${agent.id}`} placeholder="输入原项目目录的绝对路径" value={path} disabled={busy || running || unresolved && !newPrepared} onChange={event => {
        setHistorical(false); setPath(event.target.value); setCheck(null); setHandle(""); setConsent(false); setPreview(null); setEnvNotice("");
        if (!unresolved) { setJobId(""); setJobHandle(""); setJob(null); setReadState("idle"); setNewPrepared(false); }
      }} />
    </label>
    <p className="muted">在访达中选中原项目文件夹，按 Option + Command + C 复制路径后粘贴。</p>
    {check && <div className="project-check-facts">
      <p>{check.project_alias} · {new Date(check.checked_at * 1000).toLocaleString()}</p>
      {check.env && <p>实际命中层级：{layers[check.env.layer] || "未知"} · 信任作用域：{trusts[check.env.trust] || "未知"} · 接入地址：{presence(check.env.base_key_present)} · 本地连接标识：{presence(check.env.api_key_present)}</p>}
      {check.reason && <p className="muted">{checkReasons[check.reason] || "项目配置条件尚未确认，请核对配置后重查。"} 只读检查不会改写环境文件。</p>}
      {check.actions.includes("restart_cli") && <p className="muted">请关闭旧 CLI 会话，使用下方正确命令从所选原项目启动新会话。</p>}
      {check.actions.includes("run_project_task") && !running && !currentPass && canRun && (!needsRecheck || newPrepared) && !preview && <label className="row"><input type="checkbox" disabled={busy || loading} checked={consent} onChange={event => setConsent(event.target.checked)} />同意在此原项目创建临时文件并运行真实 CLI 工具任务，结束后清理；可能使用额度或产生费用。</label>}
    </div>}
    {agent.id === "gemini" && check && !running && !preview && <div className="row">
      {check.actions.includes("preview_project_env_add") && <button disabled={busy || loading} onClick={() => void projectEnv("add")}>预览补齐本项目连接</button>}
      {check.actions.includes("preview_project_env_undo") && <button disabled={busy || loading} onClick={() => void projectEnv("undo")}>撤销本项目连接</button>}
    </div>}
    {preview && <section aria-label="本项目连接方案">
      <p>{preview.project_alias} · {layers[preview.layer] || "未知层级"}</p>
      <p>{preview.operation === "undo" ? "移除本应用为该项目新增的连接行。" : "Gemini 会从这个项目环境文件连接本机。"}仅处理下列连接键，保留其余内容。</p>
      {preview.changes.filter(change => ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"].includes(change.key)).map(change => <p key={change.key}>{change.key === "GOOGLE_GEMINI_BASE_URL" ? "接入地址" : "本地连接标识"}：{change.action === "add" ? "补齐" : change.action === "keep" ? "保持" : "移除本应用新增行"}</p>)}
      <div className="row"><button className="button-primary" disabled={busy || loading} onClick={() => void applyProjectEnv()}>确认{preview.operation === "undo" ? "撤销" : "合并补齐"}</button><button disabled={busy} onClick={() => { setPreview(null); setEnvNotice("保持不变；该项目尚未验证。"); }}>保持不变</button></div>
    </section>}
    {!canRun && <p className="muted">可检查或撤销项目连接。运行文件任务前，请返回 CLI 连接，确认连接、CLI、网关、来源和费用范围。</p>}
    {envNotice && <p role="status">{envNotice}</p>}
    {error && <p className="alert error" role="alert">{error}</p>}
    {currentPass && <p className="muted">实际来源与模型：{Array.from(new Set((job?.evidence?.requests || []).map(request => `${request.provider} / ${request.served_model}`))).join(" · ")} · {observedAt ? new Date(observedAt * 1000).toLocaleString() : "观测时间未知"}</p>}
    <div className="first-task-action">
      {label === "复制正确启动命令" ? <div className="first-task-copy"><CopyBtn text={agent.launch_command || null} label={label} /></div> : <button className="button-primary" disabled={busy || loading && !running || Boolean(preview) || restoring && !running || (label === "开始新的原项目验证" ? !canRun || !consent : label === "只读检查项目" ? !path.trim().startsWith("/") : false)} onClick={() => {
        if (running) void cancel(); else if (label === "重新读取任务状态") readAgain(); else if (label === "开始新的原项目验证") void start(); else if (label === "只读检查项目") void inspect();
      }}>{busy ? "处理中…" : label}</button>}
      {unresolved && !newPrepared && !running && <button className="ghost" disabled={busy || restoring} onClick={() => { setNewPrepared(true); }}>准备一次新的验证</button>}
      {unresolved && newPrepared && <button className="ghost" disabled={busy || restoring} onClick={() => { setNewPrepared(false); readAgain(); }}>重新读取原任务记录</button>}
      {check && !running && <button className="ghost" disabled={busy || loading || unresolved && !newPrepared || !path.trim().startsWith("/")} onClick={() => void inspect()}>重新检查项目</button>}
    </div>
    {agent.launch_command && <p className="muted">请先在选定项目目录打开终端，再运行：<code>{agent.launch_command}</code> <CopyBtn text={agent.launch_command} />。复制命令不会切换工作目录，也不会启动工具。</p>}
    {job?.reason && <p className="alert">任务结果：{taskReasons[job.reason] || "任务未完成，请核对项目、CLI 与来源状态"}（<code>{job.reason}</code>）</p>}
    {job?.evidence && <button className="ghost" disabled={busy || running || restoring} onClick={() => setView("evidence")}>查看本次项目任务证据</button>}
  </TaskView>;
}

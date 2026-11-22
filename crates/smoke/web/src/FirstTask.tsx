import { useEffect, useRef, useState } from "react";
import { AgentRow, AgentValidationJob, CostSettings, GatewayPage, ProvidersPage, deleteJson, fetchJson, postJson, readableActionError, resetGatewayAccess } from "./api";
import { CopyBtn } from "./CopyButton";
import { AgentProbe } from "./AgentProbe";
import { TaskView } from "./TaskView";
import { ProjectTask } from "./ProjectTask";
import { cliInstallations, cliPreparation, validationFailureGuide, type AgentEntry } from "./agent-flow";

const discoveryLabels: Record<string, string> = {
  not_checked: "尚未检查可执行文件与版本", not_found: "未找到 CLI 可执行文件",
  not_executable: "CLI 文件不可执行", version_unknown: "无法确认 CLI 版本",
  version_unsupported: "此版本尚不支持文件验证", ready: "可执行文件与版本已确认",
};
const jobLabels: Record<string, string> = {
  queued: "文件测试排队中", running: "文件测试运行中", passed: "文件测试已完成",
  failed: "文件测试未通过", cancelled: "文件测试已取消", timed_out: "文件测试已超时",
};
const failureLabels: Record<string, string> = {
  not_wired: "请先连接此 CLI", version_unknown: "无法确认 CLI 版本，请重新检查",
  version_unsupported: "此版本尚不支持文件验证", cancelled: "任务已取消", timed_out: "任务超过等待时间",
  file_task_unverified: "未确认文件工具已完成任务", session_unverified: "未能确认本次 CLI 会话归属",
  identity_changed: "连接条件已经改变，请重新验证", cli_failed: "CLI 执行未完成，请检查安装与连接设置",
  validation_busy: "已有验证任务运行，请等待或取消", output_limit: "CLI 输出超过限制，任务已停止",
};

type Go = (tab: "providers" | "proxy") => void;

function taskFacts(agent: AgentRow, providers: ProvidersPage, gateway: GatewayPage, settings: CostSettings) {
  const rows = providers.providers || [];
  const ready = rows.filter(p => p.source_health ? p.source_health.credential.present || p.source_health.credential.required === false : p.ready);
  const usable = rows.filter(p => p.source_health ? p.source_health.credential.usable : p.ready);
  const paused = rows.length > 0 && rows.every(p => p.paused) || ready.length > 0 && ready.every(p => p.paused);
  const costFactsComplete = rows.length > 0 && rows.every(p => p.source_health);
  const hasCostCandidate = usable.some(p => !p.paused && !p.routing_disabled &&
    (p.source_health?.cost.basis === "confirmed_free" || settings.cost_intent === "free_then_subscription" &&
      p.source_health?.cost.subscription_eligible && p.source_health.cost.authorized));
  const canAttempt = gateway.serving && !paused && usable.some(p => !p.paused) &&
    Boolean(settings.cost_intent) && !settings.cost_policy_issue && (!costFactsComplete || hasCostCandidate) && agent.discovery?.status === "ready" &&
    agent.wired && (!agent.drift || agent.drift === "clean");
  return { ready, usable, paused, costFactsComplete, hasCostCandidate, canAttempt };
}

// The owner stays mounted after first entry; replies remain scoped to this Agent.
function FileTask({ agent, providers, gateway, settings, loading, onReload, go, onProject, onBack, onBusyChange, blocked, refreshRevision }: {
  agent: AgentRow; providers: ProvidersPage; gateway: GatewayPage; settings: CostSettings; loading: boolean; onReload: () => void; go: Go;
  onProject: () => void; onBack: () => void; onBusyChange: (busy: boolean) => void; blocked: boolean; refreshRevision: number;
}) {
  const [jobId, setJobId] = useState(agent.validation?.scope === "private_probe" ? agent.validation.last_job_id || "" : "");
  const [job, setJob] = useState<AgentValidationJob | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [mutationFailed, setMutationFailed] = useState(false);
  const [readState, setReadState] = useState<"idle" | "reading" | "ready" | "failed">(jobId ? "reading" : "idle");
  const [startUnknown, setStartUnknown] = useState(false);
  const [newPrepared, setNewPrepared] = useState(false);
  const [retry, setRetry] = useState(0);
  const [view, setView] = useState<"task" | "conditions" | "evidence">("task");
  const alive = useRef(true);
  const pending = useRef(false);
  const pendingStart = useRef<{ priorJobId: string } | null>(null);
  const summaryIdSeen = useRef(jobId);
  const summaryWasTracked = useRef(Boolean(jobId && agent.validation?.scope === "private_probe" && agent.validation.last_job_id === jobId));
  const reloadRef = useRef(onReload);
  reloadRef.current = onReload;
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  useEffect(() => {
    const saved = agent.validation?.scope === "private_probe" ? agent.validation.last_job_id || "" : "";
    if (!saved) return; // A restarted gateway's not_run must not erase our old ID.
    const submitted = pendingStart.current;
    if (submitted && saved !== submitted.priorJobId) {
      pendingStart.current = null; setStartUnknown(false); setMutationFailed(false); setNewPrepared(false);
      summaryIdSeen.current = saved; setJobId(saved); setRetry(value => value + 1);
    } else if (!submitted && saved !== summaryIdSeen.current) {
      summaryIdSeen.current = saved; setJobId(saved); setNewPrepared(false);
    }
  }, [agent.validation]);
  useEffect(() => {
    const summary = agent.validation;
    const lostRecord = summaryWasTracked.current && summary?.status === "not_run" && !summary.last_job_id;
    summaryWasTracked.current = Boolean(jobId && summary?.scope === "private_probe" && summary.last_job_id === jobId);
    if (lostRecord && jobId) { setReadState("reading"); setRetry(value => value + 1); }
  }, [jobId, agent.validation?.scope, agent.validation?.last_job_id, agent.validation?.status]);
  useEffect(() => {
    if (!jobId) return;
    setJob(null); setReadState("reading");
    let current = true;
    let timer: number | undefined;
    const observe = async () => {
      try {
        const next = await fetchJson<AgentValidationJob>(`/admin/agent-validation-jobs/${encodeURIComponent(jobId)}`);
        if (!current) return;
        if (next.agent_id !== agent.id || next.job_id !== jobId || next.scope !== "private_probe") throw new Error("任务身份不匹配，请重新读取。");
        setJob(next); setReadState("ready");
        if (next.status === "queued" || next.status === "running") setNewPrepared(false);
        setError("");
        if (next.status === "queued" || next.status === "running") timer = window.setTimeout(observe, 1500);
        else reloadRef.current();
      } catch {
        if (current) { setReadState("failed"); setError("无法核实前次示例目录任务是否已执行或结束。可按原任务 ID 重新读取；不会自动再次发起调用。"); }
      }
    };
    void observe();
    return () => { current = false; window.clearTimeout(timer); };
  }, [agent.id, jobId, retry, refreshRevision]);

  const mutate = async (action: () => Promise<void>) => {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    setError("");
    try { await action(); }
    catch (failure) { if (alive.current) { setMutationFailed(true); if (pendingStart.current) { setStartUnknown(true); onReload(); } setError(readableActionError(failure, "操作结果尚未确认，请重新读取当前状态后继续。")); } }
    finally { pending.current = false; if (alive.current) setBusy(false); }
  };
  const { ready, usable, paused, costFactsComplete, hasCostCandidate, canAttempt: allowed } = taskFacts(agent, providers, gateway, settings);
  const canAttempt = allowed && !blocked;
  const discovery = agent.discovery;
  const cli = cliPreparation(agent);
  const summaryMatches = Boolean(jobId && agent.validation?.scope === "private_probe" && agent.validation.last_job_id === jobId);
  const readable = readState === "ready" && job?.agent_id === agent.id && job.job_id === jobId && job.scope === "private_probe" && !startUnknown && !pendingStart.current;
  const status = readable && job ? job.status : summaryMatches ? agent.validation!.status : "not_run";
  const running = Boolean(jobId) && !startUnknown && readState !== "failed" && (status === "queued" || status === "running");
  const unresolved = startUnknown || readState === "failed" || !jobId && agent.validation?.scope === "private_probe" && agent.validation.status !== "not_run";
  const passed = readable && summaryMatches && !blocked && !loading && status === "passed" && agent.validation?.valid === true &&
    agent.validation.scope === "private_probe" && job?.scope === "private_probe" &&
    job.evidence?.tool_result_matched === true && job.evidence.final_exact === true &&
    job.evidence.requests.length >= 2;
  const stale = status === "passed" && (!summaryMatches || agent.validation?.valid !== true);
  const observedAt = summaryMatches ? agent.validation?.observed_at : job?.finished_at;
  const restoring = Boolean(jobId && readState === "reading");
  useEffect(() => { onBusyChange(busy || running || restoring); return () => onBusyChange(false); }, [busy, running, restoring, onBusyChange]);
  const check = () => mutate(async () => {
    if (blocked) return;
    await postJson(`/admin/agents/${agent.id}/discovery`, {});
    if (alive.current) onReload();
  });
  let message = "已连接，尚未完成文件测试";
  let label = "开始新的验证";
  const startPrivate = () => void mutate(async () => {
    if (!canAttempt || running || restoring || unresolved && !newPrepared || mutationFailed && !newPrepared) return;
    pendingStart.current = { priorJobId: agent.validation?.last_job_id || jobId };
    setNewPrepared(false); setStartUnknown(false);
    const next = await postJson<Pick<AgentValidationJob, "job_id" | "agent_id" | "scope" | "status">>(`/admin/agents/${agent.id}/validation-jobs`, {});
    if (!alive.current) return;
    if (next.agent_id !== agent.id || next.scope !== "private_probe" || !next.job_id) throw new Error("任务身份不匹配");
    pendingStart.current = null; setStartUnknown(false); setMutationFailed(false);
    setJob(null); setReadState("reading");
    setJobId(next.job_id);
    onReload();
  });
  let action: () => void = startPrivate;
  if (running) {
    message = jobLabels[status]; label = "取消文件测试";
    action = () => void mutate(async () => {
      const id = jobId || agent.validation?.last_job_id;
      if (!id) throw new Error("任务编号缺失");
      await deleteJson(`/admin/agent-validation-jobs/${encodeURIComponent(id)}`);
      if (alive.current) setRetry(value => value + 1);
    });
  } else if (!gateway.serving) {
    message = "网关已暂停接收新请求。恢复后继续设置。"; label = "恢复接收请求";
    action = () => void mutate(async () => { await postJson("/admin/gateway", { serving: true }); if (alive.current) onReload(); });
  } else if (!cli.ready) {
    message = cli.message; label = "重新检测 CLI 与版本"; action = () => void check();
  } else if (readable && (status === "failed" || status === "timed_out") && !newPrepared) {
    const guide = validationFailureGuide(job?.reason || (status === "timed_out" ? status : null));
    message = guide.message; label = guide.action;
    action = () => { if (guide.target === "conditions") onReload(); setView("conditions"); };
  } else if (paused) {
    message = "已有凭据的模型源均已暂停。"; label = "恢复模型源"; action = () => go("providers");
  } else if (!ready.length) {
    message = "先添加模型源并设置凭据或登录。凭据存在不代表模型已经可用。"; label = "设置模型源"; action = () => go("providers");
  } else if (!usable.some(p => !p.paused)) {
    message = "已发现来源凭据，但目前不可用于路由。请查看来源状态与可执行的处理说明。";
    label = "查看模型源状态"; action = () => go("providers");
  } else if (settings.cost_intent && costFactsComplete && !hasCostCandidate) {
    message = "当前费用范围内尚无已确认的来源。请核对可信免费目录或专属订阅授权，不必重新填写已有凭据。";
    label = "检查费用范围与模型源"; action = () => go("providers");
  } else if (!settings.cost_intent || settings.cost_policy_issue) {
    message = "请先在模型源中确认全局费用范围，再运行验证。"; label = "去模型源"; action = () => go("providers");
  } else if (!agent.wired || (agent.drift && agent.drift !== "clean")) {
    message = agent.drift === "replaced" ? "接线字段被其他工具改写，请先核对当前 Agent 配置。" : "连接后将请求发往本机网关，断开时可预览恢复受管字段。";
    label = "连接到 aubridge";
    action = () => void mutate(async () => { await postJson(`/admin/agents/${agent.id}/enable`, { on: true }); if (alive.current) onReload(); });
  } else if (passed) {
    message = `${agent.name} 已在示例目录完成文件测试 · 原项目尚未验证`;
    label = "检查原项目";
    action = () => onProject();
  } else if (agent.validation?.scope === "project" && !newPrepared) {
    message = "最近的验证来自原项目，请查看对应项目与实际结果。";
    label = "查看原项目任务"; action = () => onProject();
  } else if (stale) {
    message = "连接条件已改变，历史结果需重新验证。";
  } else if (readable && status === "cancelled") {
    message = "本次示例目录任务已取消，尚未验证通过。新的验证会独立调用模型。";
  }
  // A failed observation must never turn a running/unknown job into another POST.
  if (unresolved && !newPrepared && !running) {
    message = "无法核实前次示例目录验证是否已执行或结束；新验证不会取消前次任务。";
    label = jobId ? "重新读取原任务" : "重新读取验证记录";
    action = () => { onReload(); if (jobId) setRetry(value => value + 1); };
  }
  if (mutationFailed && !startUnknown && !newPrepared && !running) {
    label = "重新读取当前状态";
    action = () => { setError(""); setMutationFailed(false); onReload(); if (jobId) setRetry(value => value + 1); };
  }
  if (restoring && !running) {
    message = "正在读取此前的文件任务…";
    label = "正在读取任务…";
  }
  const locked = busy || running || restoring;
  const cliAction = label === "重新检测 CLI 与版本";
  const primaryStartAvailable = action === startPrivate && canAttempt && !locked && !loading && (!unresolved || newPrepared) && (!mutationFailed || newPrepared);
  const sideStartAvailable = agent.validation?.scope === "project" && !(locked || loading || mutationFailed || unresolved && !newPrepared || !canAttempt);
  const startAvailable = primaryStartAvailable || sideStartAvailable;
  const failure = job?.reason && <p className="alert">{readable ? "本次" : "此前可读"}任务结果：{failureLabels[job.reason] || "未通过文件验证，请核对本任务条件后再决定是否新运行"}（<code>{job.reason}</code>）</p>;
  if (view === "conditions") return <TaskView title={`${agent.name} 文件任务条件`} backLabel="返回文件任务准备" onBack={() => setView("task")} busy={locked}>
    {error && <p className="alert error" role="alert">{error}</p>}{failure}
    <dl className="task-facts">
      <div><dt>CLI 与版本</dt><dd>{discoveryLabels[discovery?.status || "not_checked"]}{discovery?.version ? ` · ${discovery.version}` : ""} · <button className="ghost" disabled={busy || running} onClick={() => void check()}>重新检查</button></dd></div>
      <div><dt>来源凭据</dt><dd>{ready.length ? `${ready.length} 个来源已具备凭据或无需凭据，尚不代表模型可用` : "尚未发现可用凭据或登录"}。自动模式由实际请求决定来源。</dd></div>
      <div><dt>接线</dt><dd>{agent.wired ? "配置已连接到 aubridge" : "尚未连接"}；不代表文件工具任务通过。</dd></div>
      <div><dt>文件工具任务</dt><dd>{jobLabels[status] || "尚未测试"} · 只验证私有示例目录，不验证原项目。</dd></div>
    </dl>
    {discovery?.status === "version_unsupported" && <p className="muted">{cli.message}。</p>}
    {cliInstallations[agent.id] && <p><a href={cliInstallations[agent.id]} target="_blank" rel="noreferrer">{agent.name} 官方安装说明与系统前提</a></p>}
    <p className="muted">重新检测只在本机执行 --version，不调用模型、不改写接线。</p>
    <p className="muted">连接设置会改写受管配置，开始验证才调用模型。可返回“连接与模型”手动预配或预览断开。</p>
    <p className="muted">当前费用许可：{settings.cost_intent === "confirmed_free_only" ? "只允许符合确认免费依据的模型" : settings.cost_intent === "free_then_subscription" ? "确认免费优先，并可使用你已明确授权的专属订阅来源" : "尚未确认"}。具体可用性由网关按当次条件核定，实际回答来源与模型在本次证据存在后显示。{agent.id === "gemini" && "Gemini CLI 是所选客户端，不要求为它另填 Google API Key；后端来源与费用许可另行确认。"}</p>
    <p className="muted">只在私有示例目录验证文件工具，会使用所选来源额度，可能产生费用；原项目仍需单独验证。</p>
  </TaskView>;
  if (view === "evidence" && job?.evidence) return <TaskView title={`${agent.name} 示例目录任务证据`} backLabel="返回文件任务准备" onBack={() => setView("task")} busy={locked}>
    {error && <p className="alert error" role="alert">{error}</p>}{failure}
    <p>{passed ? "本次验证" : "历史任务"} · {observedAt ? new Date(observedAt * 1000).toLocaleString() : "观测时间未知"}</p>
    {job.evidence.requests.map(request => <p key={request.request_id}>实际来源 <code>{request.provider}</code> · 模型 <code>{request.served_model}</code> · 请求 <code>{request.request_id}</code></p>)}
  </TaskView>;
  return <TaskView title={`${agent.name} 示例目录文件任务准备`} backLabel={`返回 ${agent.name} 验证`} onBack={onBack} busy={locked}>
    <div className="first-task-action">
      <p role="status" aria-live="polite">{message}{cliAction && discovery?.version ? ` · 当前检测版本 ${discovery.version}` : ""}</p>
      <div className="actions">
      {label === "复制正确启动命令" && !error ? <div className="first-task-copy"><CopyBtn text={agent.launch_command || null} label="复制正确启动命令" id={`复制 ${agent.name} 正确启动命令`} /></div> :
        <button className="button-primary" disabled={busy || loading && !running || restoring && !running || agent.drift === "replaced" && label === "连接到 aubridge"} onClick={action}>{busy ? "处理中…" : label}</button>}
      {cliAction && cliInstallations[agent.id] && <a href={cliInstallations[agent.id]} target="_blank" rel="noreferrer">官方安装说明</a>}
      </div>
    </div>
    {cliAction && <p className="muted">本次仅在本机执行 --version，不改连接、不调用模型。</p>}
    {startAvailable && !jobId && !startUnknown && !pendingStart.current && <p className="muted">当前没有可恢复的验证记录；这不表示此前从未执行。</p>}
    {newPrepared && <p role="status">已准备一次新的示例目录验证，尚未发起调用。点击开始将独立使用模型额度，可能产生费用；不会取消或确认前次任务。</p>}
    {startAvailable && <p className="muted">开始验证会调用模型，使用所选来源额度并可能产生费用；只验证私有示例目录，原项目仍需单独验证。</p>}
    {error && <p className="alert error" role="alert">{error}</p>}{failure}
    {passed && <p className="muted">实际来源与模型：{Array.from(new Set((job?.evidence?.requests || []).map(request => `${request.provider} / ${request.served_model}`))).join(" · ") || "未提供"}</p>}
    <div className="actions"><button className="ghost" disabled={locked} onClick={() => setView("conditions")}>查看文件任务条件</button>
      {(unresolved || readable && (status === "failed" || status === "timed_out")) && !newPrepared && !running && <button className="ghost" disabled={busy || restoring} onClick={() => { setNewPrepared(true); setMutationFailed(false); }}>准备一次新的验证</button>}
      {unresolved && newPrepared && <button className="ghost" disabled={busy || restoring} onClick={() => { setNewPrepared(false); onReload(); if (jobId) setRetry(value => value + 1); }}>{jobId ? "重新读取原任务" : "重新读取验证记录"}</button>}
      {job?.evidence && <button className="ghost" disabled={locked} onClick={() => setView("evidence")}>查看本次任务证据</button>}
      {agent.validation?.scope === "project" && <button className="ghost" disabled={locked || loading || mutationFailed || unresolved && !newPrepared || !canAttempt} onClick={startPrivate}>在示例目录重新验证文件工具</button>}
    </div>
  </TaskView>;
}


export function FirstTask({ agent, configured, providers, gateway, settings, failed, loading, onReload, go, onBusyChange, entry, refreshRevision = 0 }: {
  agent: AgentRow; configured: boolean; providers: ProvidersPage | null; gateway: GatewayPage | null;
  settings: CostSettings | null; failed: boolean; loading: boolean; onReload: () => void; go: Go;
  onBusyChange?: (busy: boolean) => void;
  entry?: AgentEntry; refreshRevision?: number;
}) {
  const [view, setView] = useState<"menu" | "short" | "private" | "project">(entry?.target === "private" || entry?.target === "project" ? entry.target : "menu");
  const [visited, setVisited] = useState({ short: false, private: entry?.target === "private", project: entry?.target === "project" });
  const [shortBusy, setShortBusy] = useState(false);
  const [privateBusy, setPrivateBusy] = useState(false);
  const [projectBusy, setProjectBusy] = useState(false);
  const menu = useRef<HTMLDivElement>(null);
  const busy = shortBusy || privateBusy || projectBusy;
  useEffect(() => { onBusyChange?.(busy); return () => onBusyChange?.(false); }, [busy, onBusyChange]);
  const enter = (next: "short" | "private" | "project") => {
    if (busy) return;
    setVisited(current => ({ ...current, [next]: true })); setView(next);
  };
  useEffect(() => {
    const target = entry?.target;
    if (target === "private" || target === "project") {
      setVisited(current => ({ ...current, [target]: true })); setView(target);
    }
  }, [entry?.sequence]);
  const back = (origin: "short" | "private" | "project") => {
    setView("menu");
    window.requestAnimationFrame(() => menu.current?.querySelector<HTMLButtonElement>(`[data-task-entry="${origin}"]`)?.focus());
  };
  if (!providers || !gateway || !settings) return failed ? <div className="first-task-action"><p className="alert error" role="alert">无法读取连接状态；尚未确认任何验证条件。</p><button className="ghost" disabled={loading} onClick={() => { resetGatewayAccess(); onReload(); }}>{loading ? "重新连接中…" : "重新连接"}</button></div> : <p role="status">正在读取连接状态…</p>;
  const blocked = failed || !configured;
  const canAttempt = !blocked && taskFacts(agent, providers, gateway, settings).canAttempt;
  const observedAgent = (blocked || loading) && agent.validation ? { ...agent, validation: { ...agent.validation, valid: false } } : agent;
  const canInspectProject = agent.id === "gemini" || agent.validation?.scope === "project" ||
    agent.validation?.scope === "private_probe" && agent.validation.valid && agent.validation.status === "passed";
  return <div className="first-task">
    {failed && <p className="alert error" role="alert">无法读取当前连接条件；已进入的任务状态仍保留。<button className="ghost" disabled={loading} onClick={() => { resetGatewayAccess(); onReload(); }}>重新连接</button></p>}
    {!configured && <p className="alert">当前尚未提供可接线的 CLI 配置。</p>}
    <section className="task-workspace-pane" hidden={view !== "menu"}>
      <h3>{agent.name} 验证</h3>
      <p className="muted">首次使用建议先准备示例目录文件验证；进入只读取条件，实际运行仍需明确点击。短文本、示例目录和原项目结果互不替代。</p>
      {(!settings.cost_intent || settings.cost_policy_issue) && <p className="muted">运行任务前请在模型源确认全局费用范围。<button className="ghost" disabled={busy || loading} onClick={() => go("providers")}>去模型源</button></p>}
      {agent.validation && agent.validation.status !== "not_run" && <p role="status">最近{agent.validation.scope === "project" ? "原项目" : "示例目录"}任务：{jobLabels[agent.validation.status] || "状态尚未确认"}{!blocked && !loading && agent.validation.valid ? "" : "，当前条件需重新核对"}</p>}
      <div ref={menu} className="task-workspace-nav" aria-label="验证范围">
        <button data-task-entry="private" data-validation-primary className="button-primary" disabled={busy || blocked} onClick={() => enter("private")}>准备文件工具验证（首次推荐）</button>
        <button data-task-entry="short" className="ghost" disabled={busy || blocked || !agent.wired} onClick={() => enter("short")}>仅测试文本连接（排障）</button>
        <button data-task-entry="project" className="ghost" disabled={busy || blocked || !canInspectProject} onClick={() => enter("project")}>{agent.id === "gemini" ? "检查或撤销项目连接" : "原项目检查与任务"}</button>
      </div>
      {!canInspectProject && <p className="muted">完成示例目录验证后，可进入原项目检查与任务。</p>}
    </section>
    {visited.short && <section className="task-workspace-pane" hidden={view !== "short"}><AgentProbe agent={agent} embedded disabled={blocked} onBusyChange={setShortBusy} onClose={() => back("short")} /></section>}
    {visited.private && <section className="task-workspace-pane" hidden={view !== "private"}><FileTask agent={agent} providers={providers} gateway={gateway} settings={settings} loading={loading || blocked} blocked={blocked} onReload={onReload} go={go} refreshRevision={refreshRevision} onBusyChange={setPrivateBusy} onBack={() => back("private")} onProject={() => enter("project")} /></section>}
    {visited.project && <section className="task-workspace-pane" hidden={view !== "project"}><ProjectTask agent={observedAgent} canRun={canAttempt} loading={loading || blocked} onReload={onReload} onBack={() => back("project")} onBusyChange={setProjectBusy} /></section>}
  </div>;
}

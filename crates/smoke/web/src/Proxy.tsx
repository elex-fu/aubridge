import { useCallback, useEffect, useRef, useState } from "react";
import { GatewayPage, DecisionAttempt, DecisionRow, SkippedCandidate, deleteJson, fetchJson, postJson, listenServingChanged } from "./api";
import { fmtDuration, fmtRouteReason, fmtTime } from "./format";
import { SessionDiagnostics } from "./SessionDiagnostics";
import { Dialog } from "./Dialog";
import { ProxyModelTest } from "./ProxyModelTest";
import { TaskView } from "./TaskView";
import { Analytics } from "./Analytics";
import { SourceInspection } from "./Providers";
import { AnalyticsRange, analyticsQuery, validRange } from "./analytics-types";

function historicalKind(d: DecisionRow): "free_tier" | "subscription" | "unknown" {
  if (!Array.isArray(d.attempts)) return "unknown";
  let selected: DecisionAttempt | undefined;
  for (const attempt of d.attempts) {
    if (attempt == null || typeof attempt !== "object" || typeof attempt.chosen !== "boolean") return "unknown";
    if (attempt.chosen) {
      if (selected) return "unknown";
      selected = attempt;
    }
  }
  if (!selected?.id || selected.id !== d.provider) return "unknown";
  return selected.kind === "free_tier" || selected.kind === "subscription" ? selected.kind : "unknown";
}

function DecisionTrace({ trace }: { trace: ReturnType<typeof projectTrace> }) {
  const { attempts, skipped, legacy, unavailable } = trace;
  return (
    <>
      {legacy ?? (!!attempts?.length && (
        <ol className="trace-list">
          {attempts.map((attempt, index) => (
            <li className="trace-attempt" key={index}>
              <strong>{attempt.id || "源未知"}</strong>{" "}
              {attempt.chosen && <span className="badge">网关记录选中</span>}
              {attempt.kind === "subscription" && <span className="badge subscription">订阅</span>}
              {attempt.kind === "free_tier" && <span className="badge off">免费档</span>}
              {(attempt.requested_model || attempt.model) && (
                <div className="muted">
                  {attempt.requested_model || attempt.model}
                  {attempt.requested_model && attempt.model && attempt.requested_model !== attempt.model ? ` → ${attempt.model}` : ""}
                </div>
              )}
              <div className="muted">
                {[
                  attempt.status ? `HTTP ${attempt.status}` : "",
                  attempt.reason ? fmtRouteReason(attempt.reason) : "",
                  attempt.duration_ms != null ? fmtDuration(attempt.duration_ms) : "",
                ].filter(Boolean).join(" · ") || "此尝试未提供状态或原因"}
              </div>
            </li>
          ))}
        </ol>
      ))}
      {!!skipped?.length && (
        <section className="trace-skipped">
          <h4>跳过 {skipped.length} 个候选</h4>
          <ul>
            {skipped.map((entry, index) => (
              <li key={index}>
                {entry.id}{entry.id && entry.model && " / "}{entry.model}
                {entry.reason && ` · ${fmtRouteReason(entry.reason)}`}
                {entry.retry_at ? ` · ${fmtTime(entry.retry_at)} 可重试` : ""}
                {entry.estimated_input_tokens != null && ` · 估算输入 ${entry.estimated_input_tokens.toLocaleString()} token`}
                {entry.context_window != null && ` · 上下文窗口 ${entry.context_window.toLocaleString()} token`}
              </li>
            ))}
          </ul>
        </section>
      )}
      {unavailable && <div>轨迹暂不可用</div>}
      {!legacy && !skipped?.length && (!attempts?.length || attempts.every(a => !a.reason || a.reason === "ok")) && !unavailable && (
        <div className="muted">此记录未提供完整的选源原因。</div>
      )}
    </>
  );
}

function diagnosticEntries(entries: unknown, fields: Record<string, "string" | "number" | "boolean">) {
  if (!Array.isArray(entries)) return null;
  return entries.flatMap(entry => {
    if (entry == null || typeof entry !== "object" || Array.isArray(entry)) return [];
    const result: Record<string, string | number | boolean> = {};
    for (const [field, type] of Object.entries(fields)) {
      const value: unknown = entry[field];
      if (typeof value === type && (typeof value === "string" || typeof value === "boolean" ||
        (typeof value === "number" && Number.isFinite(value)))) {
        result[field] = value;
      }
    }
    return Object.keys(result).length ? [result] : [];
  });
}

function projectTrace(d: DecisionRow) {
  const legacy = typeof d.attempts === "string" ? d.attempts : null;
  const attempts = diagnosticEntries(d.attempts, {
    id: "string", model: "string", requested_model: "string", kind: "string", status: "number",
    reason: "string", chosen: "boolean", duration_ms: "number",
    cache_read_tokens: "number", cache_write_tokens: "number", upstream_request_id: "string",
  }) as Partial<DecisionAttempt>[] | null;
  const skipped = diagnosticEntries(d.skipped, {
    id: "string", model: "string", kind: "string", reason: "string", retry_at: "number",
    context_window: "number", estimated_input_tokens: "number",
  }) as Partial<SkippedCandidate>[] | null;
  const unavailable = (legacy == null && invalidTrace(d.attempts, attempts)) || invalidTrace(d.skipped, skipped);
  return { attempts, skipped, legacy, unavailable };
}

function invalidTrace(raw: unknown, entries: unknown[] | null): boolean {
  if (raw == null) return false;
  return entries == null || (Array.isArray(raw) && raw.length > 0 && entries.length === 0);
}

function requestSources(d: DecisionRow, trace: ReturnType<typeof projectTrace>) {
  const entries = new Map<string, "chosen" | "attempted" | "record">();
  const chosen = (trace.attempts || []).filter(attempt => attempt.chosen === true && attempt.id);
  for (const attempt of trace.attempts || []) {
    if (attempt.id) entries.set(attempt.id, chosen.length === 1 && chosen[0].id === attempt.id && d.provider === attempt.id ? "chosen" : "attempted");
  }
  if (d.provider && !entries.has(d.provider)) entries.set(d.provider, "record");
  return Array.from(entries, ([id, kind]) => ({ id, kind }));
}

function routeDiagnostic(d: DecisionRow, { attempts, skipped }: ReturnType<typeof projectTrace>): string {
  return JSON.stringify({
    ts: d.ts,
    agent: d.agent,
    model: d.model,
    served_model: d.served_model ?? null,
    provider: d.provider || null,
    outcome: d.outcome,
    input_tokens: d.input_tokens ?? null,
    output_tokens: d.output_tokens ?? null,
    first_content_ms: d.first_content_ms ?? null,
    duration_ms: d.duration_ms ?? null,
    session_id:d.session_id ?? null,parent_session_id:d.parent_session_id ?? null,purpose:d.purpose ?? null,
    request_id:d.request_id ?? null,request_path:d.request_path ?? null,request_bytes:d.request_bytes ?? null,
    cache_read_tokens:d.cache_read_tokens ?? null,cache_write_tokens:d.cache_write_tokens ?? null,
    input_includes_cache:d.input_includes_cache ?? null,price_snapshot:d.price_snapshot ?? null,estimate:d.estimate ?? null,
    attempts,
    skipped,
  }, null, 2);
}

function CopyBtn({ text, id, label = "复制" }: { text: string | (() => string); id: string; label?: string }) {
  const [feedback, setFeedback] = useState("");
  useEffect(() => {
    if (!feedback) return;
    const timer = window.setTimeout(() => setFeedback(""), 1500);
    return () => window.clearTimeout(timer);
  }, [feedback]);
  return (
    <button
      className="copy"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(typeof text === "function" ? text() : text);
          setFeedback("已复制");
        } catch {
          setFeedback("复制失败");
        }
      }}
      aria-label={id}
    >
      {feedback || label}
    </button>
  );
}

// 代理页(ui-design §6.4):运行状态 + 接入方式 + 本地历史请求。
// 停止 = 软开关:推理路径 503,进程不退,面板可用;重启回默认开。
// endpoint 只展示回环地址,不提供监听地址修改入口(§8 红线)。
export function Proxy({
  setErr,
  go,
  initialHistoryRange,
  historyEntryKey,
  onBackToAnalytics,
}: {
  setErr: (s: string) => void;
  go: (t: "agents" | "providers" | "models" | "proxy" | "plugins") => void;
  initialHistoryRange?: AnalyticsRange;
  historyEntryKey?: number;
  onBackToAnalytics?: () => void;
}) {
  const [task, setTask] = useState<"history" | "filter" | "test" | "access" | "sessions" | "request" | "analytics" | "source">("history");
  const [visited, setVisited] = useState({ filter: false, test: false, access: false, sessions: false, analytics: false });
  const [testRunning, setTestRunning] = useState(false);
  const [selectedRequest, setSelectedRequest] = useState<DecisionRow | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const openTask = (next: typeof task, target: HTMLElement) => {
    if (historyDialog || testRunning) return;
    opener.current = target;
    if (next !== "history" && next !== "request" && next !== "source") setVisited(current => ({ ...current, [next]: true }));
    if (next !== "access") setShownAdminToken(null);
    setTask(next);
  };
  const back = () => { if (testRunning) return; setShownAdminToken(null); setTask("history"); requestAnimationFrame(() => opener.current?.focus()); };
  const [sourceContext, setSourceContext] = useState<{ provider: string; kind: string; focusId: string; scroll: number } | null>(null);
  const [fromLocalAnalytics, setFromLocalAnalytics] = useState(false);
  const analyticsOrigin = useRef<{ scroll: number; trigger: HTMLElement | null } | null>(null);
  const proxyRoot = useRef<HTMLDivElement>(null);
  const initialQuery = useRef(initialHistoryRange && validRange(initialHistoryRange) ? analyticsQuery(initialHistoryRange) : "");
  const entrySeen = useRef(historyEntryKey);
  const ready = useRef(false);
  const [data, setData] = useState<GatewayPage | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | { error: string; before?: string }>("loading");
  const [historyDialog, setHistoryDialog] = useState<"confirm" | "clearing" | { error: string } | null>(null);
  const [shownAdminToken, setShownAdminToken] = useState<string | null>(null);
  const [appliedFilters, setAppliedFilters] = useState("");
  const [syncError, setSyncError] = useState(false);
  const latestServing = useRef<boolean | null>(null);
  const servingVersion = useRef(0);
  const servingEventsActive = useRef(false);
  const activeFilters = useRef("");
  const activeRequest = useRef<AbortController | null>(null);

  const load = useCallback(async (filters = activeFilters.current, before?: string) => {
    activeRequest.current?.abort();
    const request = new AbortController();
    activeRequest.current = request;
    activeFilters.current = filters;
    setAppliedFilters(filters);
    setStatus("loading");
    if (!before) {
      setData(current => current && { ...current, requests: [], next: null });
    }

    const query = new URLSearchParams(filters);
    if (before) query.set("before", before);
    const version = servingVersion.current;
    try {
      const page = await fetchJson<GatewayPage>(`/admin/gateway?${query}`, { signal: request.signal, cache: "no-store" });
      if (request.signal.aborted) return;
      if (servingEventsActive.current || servingVersion.current !== version) {
        page.serving = latestServing.current ?? page.serving;
      }
      if (page.history_status === "unavailable") {
        setData(current => before && current
          ? { ...page, requests: current.requests, next: current.next }
          : page);
        setStatus({ error: "", before });
        return;
      }
      setData(current => before && current
        ? { ...page, requests: [...(current.requests ?? []), ...(page.requests ?? [])] }
        : page);
      setStatus("ready");
    } catch (e) {
      if (request.signal.aborted) return;
      setStatus({ error: String(e), before });
    }
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void listenServingChanged(serving => {
      if (!active) return;
      latestServing.current = serving;
      servingVersion.current++;
      setData(current => current && { ...current, serving });
    }).then(stop => {
      if (active) {
        unlisten = stop;
        servingEventsActive.current = true;
      }
      else stop();
    }).catch(() => {
      if (active) setSyncError(true);
    }).finally(() => {
      if (active) { ready.current = true; void load(initialQuery.current); }
    });
    return () => {
      active = false;
      ready.current = false;
      servingEventsActive.current = false;
      unlisten?.();
      activeRequest.current?.abort();
      activeRequest.current = null;
    };
  }, [load]);

  useEffect(() => {
    if (entrySeen.current === historyEntryKey) return;
    entrySeen.current = historyEntryKey;
    if (!initialHistoryRange || !validRange(initialHistoryRange)) return;
    initialQuery.current = analyticsQuery(initialHistoryRange);
    setTask("history"); setFromLocalAnalytics(false); setSelectedRequest(null); setSourceContext(null);
    if (ready.current) void load(initialQuery.current);
  }, [historyEntryKey, initialHistoryRange?.start, initialHistoryRange?.end, load]);
  const openRangeRequests = (range: AnalyticsRange) => {
    if (!validRange(range)) return;
    analyticsOrigin.current = {
      scroll: proxyRoot.current?.closest(".app-main")?.scrollTop || 0,
      trigger: document.activeElement instanceof HTMLElement ? document.activeElement : null,
    };
    setFromLocalAnalytics(true); setTask("history"); void load(analyticsQuery(range));
    requestAnimationFrame(() => { const scroll = proxyRoot.current?.closest(".app-main"); if (scroll) scroll.scrollTop = 0; });
  };
  const backToAnalytics = () => {
    if (!fromLocalAnalytics) { onBackToAnalytics?.(); return; }
    setTask("analytics");
    requestAnimationFrame(() => {
      const scroll = proxyRoot.current?.closest(".app-main");
      if (scroll) scroll.scrollTop = analyticsOrigin.current?.scroll || 0;
      analyticsOrigin.current?.trigger?.focus({ preventScroll: true });
    });
  };
  const backToRequest = () => {
    setTask("request");
    requestAnimationFrame(() => {
      const scroll = proxyRoot.current?.closest(".app-main");
      if (scroll) scroll.scrollTop = sourceContext?.scroll || 0;
      if (sourceContext) document.getElementById(sourceContext.focusId)?.focus({ preventScroll: true });
    });
  };

  const setServing = async (on: boolean) => {
    const version = servingVersion.current;
    try {
      const receipt = await postJson<{ serving: boolean }>("/admin/gateway", { serving: on });
      if (activeRequest.current && servingVersion.current === version) {
        latestServing.current = receipt.serving;
        servingVersion.current++;
        setData(current => current && { ...current, serving: receipt.serving });
      }
    } catch (e) {
      setErr(on ? `启动网关失败:${e}` : `停止网关失败:${e}`);
    }
  };

  const clearHistory = async () => {
    setHistoryDialog("clearing");
    try {
      await deleteJson("/admin/gateway/history");
      setHistoryDialog(null);
      if (activeRequest.current) void load();
    } catch (e) {
      setHistoryDialog({ error: String(e) });
    }
  };

  const reqs = data?.requests ?? [];
  const next = data?.next;
  const clearing = historyDialog === "clearing";
  const adminTokenVisible = data?.admin_token === shownAdminToken;
  const filterLabels: Record<string, string> = {
    agent: "客户端", provider: "选中源", related_provider: "涉及源", model: "模型", kind: "类别", outcome: "结果", request_id: "请求 ID", session_id: "会话 ID", start: "本地开始日（含）", end: "本地结束日（不含）",
  };
  const filterValues: Record<string, string> = {
    free_tier: "免费档", subscription: "订阅", ok: "成功", failed: "失败", cancelled: "取消",
  };
  const filterEntries = Array.from(new URLSearchParams(appliedFilters).entries());

  return (
    <div ref={proxyRoot} className="proxy-page">
      {syncError && <p className="alert error" role="alert">网关状态实时同步失败，请重新打开代理页以重试。当前状态仅在读取或操作后更新。</p>}
      {data && (
        <>
          <div className="status-line gateway-strip">
            <div className="gateway-status">
              <span className={`badge ${data.serving ? "success" : "warn"}`}>
                {data.serving ? "网关运行中" : "新请求已暂停"}
              </span>
              <span className="muted">暂停后不再接收新推理，已进入的请求继续完成。</span>
            </div>
            <button disabled={status === "loading"} onClick={() => setServing(!data.serving)}>
              {data.serving ? "暂停新请求" : "恢复新请求"}
            </button>
          </div>
          {data.request_limits && <p className="muted">
            请求体上限 {data.request_limits.decoded_body_bytes / 1024 / 1024} MiB（解压后）
            {" · 支持 "}{data.request_limits.encodings.join(" / ")}
            {" · 在途 "}{data.request_limits.active_requests}/{data.request_limits.max_requests}
            {" · 读取期限 "}{data.request_limits.body_read_timeout_secs} 秒
            {" · 入口忙时请稍后重试"}
          </p>}


        </>
      )}


      <nav className="task-workspace-nav" aria-label="代理任务">
        {([ ["history", "请求记录"], ["analytics", "使用统计"], ["sessions", "会话与恢复"], ["filter", "筛选请求"], ["test", "测试模型"], ["access", "手动接入"] ] as const).map(([key, label]) => <button key={key} aria-pressed={task === key} disabled={Boolean(historyDialog) || testRunning || (!data && (key === "test" || key === "access"))} onClick={event => openTask(key, event.currentTarget)}>{label}</button>)}
      </nav>
      <div className="task-workspace-pane" hidden={task !== "history"}>
      {(fromLocalAnalytics || initialHistoryRange && onBackToAnalytics) && <button className="ghost" onClick={backToAnalytics}>返回使用统计</button>}
      <section className="panel history-panel" aria-labelledby="request-history-title">
        <div className="section-head panel-header">
          <h2 id="request-history-title">请求记录</h2>
          <div className="actions">
            <button className="ghost" onClick={() => void load()}>刷新</button>
            <button className="ghost" disabled={!data} onClick={() => setHistoryDialog("confirm")}>清空请求历史</button>
          </div>
        </div>

        {filterEntries.length > 0 && (
          <p className="filter-summary muted">当前条件：{filterEntries.map(([key, value]) => `${filterLabels[key] ?? key}：${key === "kind" || key === "outcome" ? filterValues[value] ?? value : value}`).join(" · ")}</p>
        )}
        <div className="request-list">
            {reqs.map((d) => {
              const kind = historicalKind(d);
              return (
                <div className="request-row request-item" key={d.request_id ?? JSON.stringify(d)}>
                  <button className="request-summary" onClick={event => { setSelectedRequest(d); openTask("request", event.currentTarget); }}>
                    <span className="request-overview">
                      <strong className="request-model">{d.served_model && d.served_model !== d.model ? `${d.model} → ${d.served_model}` : d.model || "入口拒绝"}</strong>
                      <span className={`badge ${d.outcome === "ok" ? "success" : d.outcome === "cancelled" ? "off" : "bad"}`}>
                        {d.outcome === "ok" ? "成功" : d.outcome === "failed" ? "失败" : d.outcome === "cancelled" ? "已取消" : fmtRouteReason(d.outcome)}
                      </span>
                      <span className="request-source">{d.provider ? `记录来源 ${d.provider}` : "来源未记录"}</span>
                      <span className={`badge ${kind === "subscription" ? "subscription" : "off"}`}>
                        {kind === "subscription" ? "订阅" : kind === "free_tier" ? "免费档" : "来源未知"}
                      </span>
                      <time className="muted">{fmtTime(d.ts)}</time>
                    </span>
                    <span className="request-meta muted">
                      <span>{d.agent || "客户端未知"} · 总计 {fmtDuration(d.duration_ms)}</span>
                      <span className="request-open">{kind === "subscription" ? "查看路由原因" : "详情"}</span>
                    </span>
                  </button>

                </div>
              );
            })}
            {!reqs.length && status === "ready" && (
              <div className="empty-state">{filterEntries.length ? "暂无符合条件的请求。" : "暂无请求。Agent 发起推理后，可在这里查看记录。"}</div>
            )}
        </div>
        <div className="toolbar muted" aria-live="polite">
          {next && typeof status !== "object" && (
            <button className="ghost" disabled={status === "loading"} onClick={() => void load(undefined, next)}>
              加载更早
            </button>
          )}
          {status === "loading" && <span className="status-loading">正在加载请求历史…</span>}
          {typeof status === "object" && (
            <>
              <span role="alert">历史请求读取失败，请重试。{status.error}</span>
              <button className="ghost" onClick={() => void load(undefined, status.before)}>重试</button>
            </>
          )}
        </div>
      </section>
      </div>
      {visited.filter && <div className="task-workspace-pane" hidden={task !== "filter"}><TaskView title="筛选请求" onBack={back}>        <form
          className="field-grid"
          onSubmit={e => {
            e.preventDefault();
            const fields = new FormData(e.currentTarget);
            const filters = new URLSearchParams();
            const scope = new URLSearchParams(activeFilters.current);
            for (const key of ["start", "end"]) { const value = scope.get(key); if (value) filters.set(key, value); }
            for (const name of ["agent", "provider", "related_provider", "kind", "outcome", "model", "request_id", "session_id"]) {
              const value = String(fields.get(name) ?? "").trim();
              if (value) filters.set(name, value);
            }
            void load(filters.toString()); back();
          }}
          onReset={() => { void load(""); back(); }}
        >
          <label className="field">客户端 ID <input name="agent" placeholder="opencode" /></label>
          <label className="field">请求 ID <input name="request_id" placeholder="req_…" /></label>
          <label className="field">会话 ID <input name="session_id" placeholder="客户端明确会话 ID" /></label>
          <label className="field">选中源 ID <input name="provider" placeholder="kimi-code" /></label>
          <label className="field">涉及源 ID <input name="related_provider" placeholder="openrouter" title="匹配选中、尝试或跳过的源" /></label>
          <label className="field">模型 ID（请求或实际） <input name="model" placeholder="vendor/model:free" /></label>
          <label className="field">
            选中源类别{" "}
            <select name="kind">
              <option value="">全部</option>
              <option value="free_tier">免费档</option>
              <option value="subscription">订阅</option>
            </select>
          </label>
          <label className="field">
            结果{" "}
            <select name="outcome">
              <option value="">全部</option>
              <option value="ok">成功</option>
              <option value="failed">失败</option>
              <option value="cancelled">取消</option>
            </select>
          </label>
          <div className="actions filter-actions">
            <button type="submit" className="button-primary">应用筛选</button>
            <button type="reset" className="ghost">清除筛选</button>
          </div>
        </form>
</TaskView></div>}
      {visited.analytics && <div className="task-workspace-pane" hidden={task !== "analytics"}><Analytics onBack={back} onOpenRequests={openRangeRequests} /></div>}
      {visited.test && data && <div className="task-workspace-pane" hidden={task !== "test"}><TaskView title="测试模型" onBack={back} busy={testRunning}><ProxyModelTest onBusyChange={setTestRunning} serving={data.serving} onCheckSources={() => go("providers")} /></TaskView></div>}
      {visited.access && data && <div className="task-workspace-pane" hidden={task !== "access"}><TaskView title="手动接入与管理令牌" onBack={back}>          <div className="gateway-access">
            <div className="panel-body access-grid">
              <div className="row">
                <span className="section-label">接入地址</span>
                <code>{data.endpoint}/v1</code>
                <CopyBtn text={`${data.endpoint}/v1`} id="endpoint" />
              </div>
              <div className="row">
                <span className="section-label">管理令牌</span>
                <code>{adminTokenVisible ? data.admin_token : "••••••••"}</code>
                <button className="ghost" onClick={() => setShownAdminToken(adminTokenVisible ? null : data.admin_token)}>
                  {adminTokenVisible ? "隐藏令牌" : "显示令牌"}
                </button>
                <CopyBtn text={data.admin_token} id="admin-token" />
              </div>
              <p className="muted">网关只监听本机回环地址。暂停时新推理返回 503，管理页仍可使用。Agent 的接入令牌请在 Agents 页编辑中复制。</p>
            </div>
          </div></TaskView></div>}
      {visited.sessions && <div className="task-workspace-pane" hidden={task !== "sessions"}><SessionDiagnostics onBack={back} filters={appliedFilters} refreshKey={data} select={(agent, session) => {
        const filters = new URLSearchParams(); filters.set("agent", agent); filters.set("session_id", session);
        void load(filters.toString()); back();
      }} /></div>}
      {task === "request" && selectedRequest && <TaskView title={`请求诊断 · ${selectedRequest.request_id ?? "旧记录"}`} onBack={back}>{(() => { const d = selectedRequest; const trace = projectTrace(d); return (                  <div className="request-detail detail-panel">
                    <dl className="request-facts">
                      <div className="request-fact"><dt>网关记录的服务模型</dt><dd>{d.served_model || "未记录"}（缺上游型号时可能沿用发出模型）</dd></div>
                      <div className="request-fact"><dt>请求结果</dt><dd>{d.outcome === "ok" ? "成功" : d.outcome === "cancelled" ? "已取消" : "失败"}；选中不等于成功服务，用量缺失不抹去已知选择。</dd></div>
                      <div className="request-fact"><dt>请求时间</dt><dd>{new Date(d.ts * 1000).toLocaleString()}</dd></div>
                      <div className="request-fact"><dt title="从进入推理路由算到有效内容就绪，非流式为完整响应就绪">首段就绪</dt><dd>{fmtDuration(d.first_content_ms)}</dd></div>
                      <div className="request-fact"><dt>总耗时</dt><dd>{fmtDuration(d.duration_ms)}</dd></div>
                      <div className="request-fact"><dt>输入 / 输出 token</dt><dd>{d.input_tokens ?? "—"} / {d.output_tokens ?? "—"}</dd></div>
                      <div className="request-fact"><dt>缓存读取 / 创建 token</dt><dd>{d.cache_read_tokens ?? "未报告"} / {d.cache_write_tokens ?? "未报告"}</dd></div>
                      <div className="request-fact"><dt>会话 / 父会话</dt><dd>{d.session_id ?? "未上报"} / {d.parent_session_id ?? "未上报"}</dd></div>
                      <div className="request-fact"><dt>请求用途</dt><dd>{d.purpose ?? "未知"}</dd></div>
                      <div className="request-fact"><dt>请求 ID</dt><dd><code>{d.request_id ?? "旧记录未保存"}</code></dd></div>
                      <div className="request-fact"><dt>请求体积（解码后）</dt><dd>{d.request_bytes == null ? "未知" : `${d.request_bytes.toLocaleString()} 字节`}</dd></div>
                      <div className="request-fact"><dt>API 等值估算</dt><dd>{d.estimate ? `$${d.estimate.api_equivalent_usd.toLocaleString(undefined,{maximumFractionDigits:8})}${d.estimate.partial_usage ? "（仅已知部分）" : ""}` : "未定价或用量不足"}</dd></div>
                    </dl>
                    {d.benchmark_snapshot && <p className="muted">用户对标：{d.benchmark_snapshot.provider} / {d.benchmark_snapshot.model} → {d.benchmark_snapshot.target_provider} / {d.benchmark_snapshot.target_model}
                      {" · "}{d.benchmark_snapshot.source} · {d.benchmark_snapshot.version} · {d.benchmark_snapshot.observed_at != null && fmtTime(d.benchmark_snapshot.observed_at)}
                      {" · 输入/输出 $"}{d.benchmark_snapshot.price.input_per_million} / ${d.benchmark_snapshot.price.output_per_million} / 百万 token
                      {" · API 等值 "}{d.benchmark_estimate ? `$${d.benchmark_estimate.api_equivalent_usd.toLocaleString(undefined,{maximumFractionDigits:8})}${d.benchmark_estimate.partial_usage ? "（部分用量）" : ""}` : "用量不足或口径未知"}
                      {d.benchmark_estimate?.cache_rate_fallback && " · 缓存缺价或未完整报告，按普通输入估算"}
                    </p>}
                    {d.price_snapshot && <p className="muted">
                      {d.price_snapshot.basis === "provider-directory" ? "提供方目录价格" : d.price_snapshot.basis === "local-declaration" ? "本机声明价格" : "历史固定估算（非官方账单）"}
                      {" · "}{d.price_snapshot.provider} / {d.price_snapshot.model} · {d.price_snapshot.source} · {d.price_snapshot.version ?? "旧版本未记录"}
                      {d.price_snapshot.observed_at != null && ` · ${fmtTime(d.price_snapshot.observed_at)} 观察`}
                      {" · 每百万输入/输出 $"}{d.price_snapshot.price.input_per_million} / ${d.price_snapshot.price.output_per_million}
                      {d.estimate?.cache_rate_fallback && " · 缓存单价或完整报告缺失，按普通输入估算"}
                    </p>}
                    <div className="request-trace">
                      <h3 className="section-label">尝试与跳过</h3>
                      <DecisionTrace trace={trace} />
                    </div>
                    <div className="actions request-detail-actions">
                      <CopyBtn text={() => routeDiagnostic(d, trace)} id={`复制 ${d.model} 的路由诊断`} label="复制诊断" />
                       {requestSources(d, trace).map((source, index) => <button
                         key={source.id}
                         id={`request-source-inspect-${index}`}
                         className="ghost"
                         onClick={event => {
                           setSourceContext({
                             provider: source.id,
                             kind: source.kind,
                             focusId: event.currentTarget.id,
                             scroll: proxyRoot.current?.closest(".app-main")?.scrollTop || 0,
                           });
                           setTask("source");
                           requestAnimationFrame(() => {
                             const scroll = proxyRoot.current?.closest(".app-main");
                             if (scroll) scroll.scrollTop = 0;
                           });
                         }}
                       >{source.kind === "chosen" ? "查看网关选中来源" : source.kind === "attempted" ? "查看尝试来源" : "查看记录来源"} · {source.id}</button>)}
                       {!requestSources(d, trace).length && <p className="muted">此请求未保存可归属的来源 ID，无法确认相关来源，不猜测当前设置。</p>}
                    </div>
                  </div>); })()}</TaskView>}
      {task === "source" && sourceContext && selectedRequest && <SourceInspection providerId={sourceContext.provider} onBack={backToRequest} requestFacts={<div className="source-request-facts">
        <p>原请求：{selectedRequest.request_id || "旧记录未保存请求 ID"} · {fmtTime(selectedRequest.ts)} · {selectedRequest.agent || "工具未记录"}</p>
        <p>结果：{selectedRequest.outcome === "ok" ? "成功" : selectedRequest.outcome === "cancelled" ? "已取消" : "失败"}；{sourceContext.kind === "chosen" ? "这条来源有一致的网关选中标记，仍不等于请求最终成功服务。" : sourceContext.kind === "attempted" ? "只确认请求曾尝试这条来源，最终选中不据此推断。" : "来源仅见于网关记录，选中标记未能确认。"}</p>
        <p>网关记录的服务模型：{selectedRequest.served_model || "未记录"}；上游未报型号时可能沿用发出模型，不当作提供方独立确认。</p>
        <p className="muted">用量是否报告与上述路由事实独立。此处不重试、不调用模型，也不修改费用授权。</p>
      </div>} />}
      {historyDialog && (
        <Dialog labelledBy="clear-history-title" describedBy="clear-history-scope" role="alertdialog" busy={clearing} onClose={() => setHistoryDialog(null)}>
          <div className="modal-header">
            <h2 id="clear-history-title">清空请求历史</h2>
          </div>
          <div className="modal-body">
            <p id="clear-history-scope">将清空全部已完成的请求明细。用量统计继续累计，进行中的请求完成后会出现在新历史中。</p>
            {typeof historyDialog === "object" && <p className="alert error" role="alert">{historyDialog.error}</p>}
          </div>
          <div className="modal-actions">
            <button className="ghost" autoFocus disabled={clearing} onClick={() => setHistoryDialog(null)}>取消</button>
            <button className="button-danger" disabled={clearing} onClick={clearHistory}>{clearing ? "清空中…" : "确认清空"}</button>
          </div>
        </Dialog>
      )}
    </div>
  );
}

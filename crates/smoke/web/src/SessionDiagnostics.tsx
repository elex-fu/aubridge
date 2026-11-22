import { useEffect, useRef, useState } from "react";
import { fetchJson } from "./api";
import { fmtDuration, fmtTime } from "./format";
import { NativeSessions } from "./NativeSessions";
import { TaskView } from "./TaskView";

type Source = {
  provider: string; model: string; kind: string; requests: number; usage_records: number;
  attempt_records: number; unreported_attempts: number;
  input_tokens: number; output_tokens: number; partial_usage_records: number;
  cache_read_tokens: number; cache_write_tokens: number; cache_reported_records: number;
  cache_unknown_records: number; cache_input_tokens: number; cache_hit_read_tokens: number; cache_hit_rate: number | null;
  price_coverage: number | null; api_equivalent_usd: number | null;
  valued_records: number; partial_estimate_records: number; cache_rate_fallback_records: number;
  cache_value_delta_usd: number | null; cache_value_known_records: number;
};
type Session = {
  agent: string; session_id: string; parent_session_ids: string[]; first_ts: number; last_ts: number;
  requests: number; successful_requests: number; duration_ms: number; duration_known_requests: number;
  purposes: Record<string, number>; free_requests: number; subscription_requests: number;
  unknown_source_requests: number; sources: Source[];
};
type Page = { sessions: Session[]; next: string | null };
const money = (v: number | null) => v == null ? "未知" : `$${v.toLocaleString(undefined, { maximumFractionDigits: 8 })}`;
const percent = (v: number | null) => v == null ? "未知" : `${(100 * v).toFixed(1)}%`;

export function SessionDiagnostics({ filters, refreshKey, select, onBack }: {
  filters: string; refreshKey: unknown; onBack: () => void; select: (agent: string, session: string) => void;
}) {
  const [selectedKey, setSelectedKey] = useState<{ agent: string; session_id: string } | null>(null);
  const opener = useRef<HTMLButtonElement | null>(null);
  const back = () => { setSelectedKey(null); requestAnimationFrame(() => opener.current?.focus()); };
  const [page, setPage] = useState<Page | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const generation = useRef(0);
  const request = useRef<AbortController | null>(null);
  async function load(before?: string) {
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    const current = ++generation.current;
    setLoading(true); setError("");
    if (!before) setPage(null);
    const active = new URLSearchParams(filters);
    const query = new URLSearchParams();
    for (const key of ["agent", "session_id"]) {
      const value = active.get(key); if (value) query.set(key, value);
    }
    if (before) query.set("before", before);
    try {
      const result = await fetchJson<Page>(`/admin/gateway/sessions?${query}`, { signal: controller.signal, cache: "no-store" });
      if (current === generation.current) {
        setPage(old => before && old ? { ...result, sessions: [...old.sessions, ...result.sessions] } : result);
        if (!before) setSelectedKey(current => current && result.sessions.some(s => s.agent === current.agent && s.session_id === current.session_id) ? current : null);
      }
    } catch (e) {
      if (!controller.signal.aborted && current === generation.current) setError(String(e));
    } finally {
      if (current === generation.current) setLoading(false);
    }
  }
  useEffect(() => { setSelectedKey(null); void load(); return () => { generation.current++; request.current?.abort(); }; }, [filters, refreshKey]);
  const selected = page?.sessions.find(s => s.agent === selectedKey?.agent && s.session_id === selectedKey?.session_id) ?? null;
  return <><div className="task-workspace-pane" hidden={selected !== null}><TaskView title="会话与恢复" onBack={onBack}><NativeSessions refreshKey={refreshKey} /><section className="panel history-panel" aria-label="会话诊断">
    <div className="section-head panel-header"><h2>会话诊断</h2><button className="ghost" disabled={loading} onClick={() => void load()}>刷新会话</button></div>
    <p className="muted">仅按客户端明确上报的会话 ID 分组；未上报的请求可在历史请求中查看。会话汇总采用客户端与会话筛选，覆盖保留的全部请求。免费与订阅次数可重叠，包含切源尝试。</p>
    {error && <p role="alert">会话数据暂不可用：{error}</p>}
    {page?.sessions.map(s => <div className="request-row" key={`${s.agent}:${s.session_id}`}>
      <button className="request-summary" onClick={event => { opener.current = event.currentTarget; setSelectedKey({ agent: s.agent, session_id: s.session_id }); }}><strong>{s.agent} / {s.session_id}</strong> · {s.requests} 请求 · 免费 {s.free_requests} / 订阅 {s.subscription_requests} / 来源未知 {s.unknown_source_requests}<span className="muted"> · {fmtTime(s.last_ts)}</span></button>

    </div>)}
    {loading && <p className="muted">读取会话…</p>}
    {page && !page.sessions.length && !loading && <p className="muted">暂无明确命名的会话。</p>}
    {page?.next && <button disabled={loading} onClick={() => void load(page.next ?? undefined)}>更多会话</button>}
  </section></TaskView></div>{selected && <TaskView title={`${selected.agent} / ${selected.session_id}`} onBack={back}>{(() => { const s = selected; return (      <div className="request-detail">
        <p>{s.successful_requests} 次成功 · 已知耗时 {fmtDuration(s.duration_ms)}（{s.duration_known_requests}/{s.requests} 请求） · 用途 {Object.entries(s.purposes).map(([k,v]) => `${k}: ${v}`).join(" / ")}</p>
        {!!s.parent_session_ids.length && <p>明确父会话：{s.parent_session_ids.join(" / ")}</p>}
        <button className="ghost" onClick={() => select(s.agent, s.session_id)}>查看此会话时间线</button>
        {s.sources.map(source => <div className="detail-panel" key={`${source.provider}:${source.model}:${source.kind}`}>
          <strong>{source.provider} / {source.model}</strong> · {source.kind === "subscription" ? "订阅" : source.kind === "free_tier" ? "免费" : "来源未知"}
          <p>实际尝试 {source.requests} 请求 / {source.attempt_records} 次；有用量 {source.usage_records} 条 / 未报告 {source.unreported_attempts} 次 · 输入 {source.input_tokens.toLocaleString()} / 输出 {source.output_tokens.toLocaleString()} token · 部分用量 {source.partial_usage_records} 条</p>
          <p>已报告部分：缓存读 {source.cache_read_tokens.toLocaleString()} / 写 {source.cache_write_tokens.toLocaleString()} token · 已报告 {source.cache_reported_records} / 未知 {source.cache_unknown_records} 条 · 命中率 {percent(source.cache_hit_rate)}（可计算读取 {source.cache_hit_read_tokens.toLocaleString()} / 输入 {source.cache_input_tokens.toLocaleString()} token）</p>
          <p>API 等值 {money(source.api_equivalent_usd)}（{source.valued_records}/{source.usage_records} 条可估值；部分估值 {source.partial_estimate_records} 条；缓存价格或报告缺失 {source.cache_rate_fallback_records} 条） · 价格覆盖 {percent(source.price_coverage)} · 缓存价差 {money(source.cache_value_delta_usd)}（{source.cache_value_known_records}/{source.attempt_records} 次可计算；包含创建成本，可为负）</p>
        </div>)}
        {!s.sources.length && <p className="muted">此会话尚无上游报告的用量。</p>}
      </div>); })()}</TaskView>}</>;
}

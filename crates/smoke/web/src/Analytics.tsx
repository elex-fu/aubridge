import { useEffect, useMemo, useRef, useState } from "react";
import { fetchJson } from "./api";
import { TaskView } from "./TaskView";
import {
  AnalyticsChange, AnalyticsDay, AnalyticsGroup, AnalyticsMetrics, AnalyticsPreset, AnalyticsRange, AnalyticsReport,
  analyticsQuery, presetRange, shiftDate, validDate, validRange,
} from "./analytics-types";

function finite(value: unknown): value is number { return typeof value === "number" && Number.isFinite(value); }
function count(value: unknown): string { return finite(value) && value >= 0 ? value.toLocaleString() : "未知"; }
function percent(value: unknown): string { return finite(value) && value >= 0 && value <= 1 ? `${(value * 100).toFixed(1)}%` : "未知"; }
function duration(value: unknown): string { return finite(value) && value >= 0 ? value < 1000 ? `${value.toFixed(0)} ms` : `${(value / 1000).toFixed(2)} 秒` : "未报告"; }
export function formatAnalyticsMoney(value: unknown): string {
  if (!finite(value) || value < 0) return "未知";
  return value > 0 && value < 0.01 ? "<$0.01" : `$${value.toFixed(2)}`;
}
function rangeLabel(range: AnalyticsRange): string { return `${range.start} 至 ${range.end}（结束日不含）`; }
function metricsPresent(value: AnalyticsMetrics): boolean {
  return Boolean(value?.requests?.duration && value?.usage?.input && value.usage.output && value.usage.cache &&
    value.usage.prices && [value.usage.prices.directory, value.usage.prices.declared, value.usage.prices.legacy, value.usage.prices.benchmark].every(price => price != null && typeof price === "object"));
}
function reportMatches(report: AnalyticsReport, range: AnalyticsRange): boolean {
  return report?.window?.start === range.start && report.window.end === range.end &&
    report.window.timezone === "local" && report.window.scope === "retained_ledger" && typeof report.window.in_progress === "boolean" && finite(report.window.observed_at) && metricsPresent(report.summary) &&
    Array.isArray(report.days) && report.days.length <= 31 && report.days.every(day =>
      validDate(day.date) && day.date >= range.start && day.date < range.end && metricsPresent(day.metrics)) &&
    new Set(report.days.map(day => day.date)).size === report.days.length &&
    [report.groups?.agents, report.groups?.providers, report.groups?.models].every(group =>
      Array.isArray(group) && group.length <= 21 && group.every(row => metricsPresent(row) && typeof row.other === "boolean" && (row.key === null || typeof row.key === "string"))) &&
    (report.comparison == null || metricsPresent(report.comparison.summary));
}
function useAnalytics(range: AnalyticsRange, compare: boolean) {
  const query = analyticsQuery(range, compare);
  const [loaded, setLoaded] = useState<{ query: string; report: AnalyticsReport } | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError(""); setLoaded(null);
    if (!validRange(range)) { setError("请选择 1–31 个本机自然日；结束日期不含，且不能晚于明日。"); setLoading(false); return; }
    void fetchJson<AnalyticsReport>(`/admin/analytics?${query}`, { signal: controller.signal, cache: "no-store" })
      .then(report => {
        if (controller.signal.aborted) return;
        if (!reportMatches(report, range)) throw new Error("analytics-response-unconfirmed");
        setLoaded({ query, report });
      }).catch(() => {
        if (!controller.signal.aborted) setError("本地统计读取未完成，不能当作空记录。若范围过大，请缩短日期；也可重新读取。");
      }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [query, retry]);
  return { report: loaded?.query === query && !loading && !error ? loaded.report : null, loading, error, retry: () => setRetry(value => value + 1) };
}

function ReadingState({ loading, error, retry }: { loading: boolean; error: string; retry: () => void }) {
  return loading ? <p className="analytics-reading" role="status">正在读取本地账本统计…</p> : error ?
    <div className="alert error" role="alert"><p>{error}</p><button className="ghost" onClick={retry}>重新读取统计</button></div> : null;
}
function DailyResults({ days, onOpenRequests }: { days: AnalyticsDay[]; onOpenRequests?: (range: AnalyticsRange) => void }) {
  const known = days.every(day => [day.metrics.requests.total, day.metrics.requests.ok, day.metrics.requests.failed, day.metrics.requests.cancelled].every(value => finite(value) && value >= 0));
  if (!days.length || !known) return <p className="muted">按日结果尚未确认。</p>;
  const maximum = Math.max(1, ...days.map(day => day.metrics.requests.total));
  return <div className="analytics-daily" style={{ gridTemplateColumns: `repeat(${days.length}, minmax(0, 1fr))` }} aria-label="所选本地日期范围的请求结果">
    {days.map(day => {
      const r = day.metrics.requests;
      const label = `${day.date}：共 ${r.total}，成功 ${r.ok}，失败 ${r.failed}，取消 ${r.cancelled}`;
      const content = <><span className="analytics-bar-count">{count(r.total)}</span><span className="analytics-bar-track">
        <span className="analytics-bar-stack" style={{ height: `${r.total / maximum * 100}%` }}>
          <span className="analytics-segment success" style={{ height: `${r.total ? r.ok / r.total * 100 : 0}%` }} />
          <span className="analytics-segment failed" style={{ height: `${r.total ? r.failed / r.total * 100 : 0}%` }} />
          <span className="analytics-segment cancelled" style={{ height: `${r.total ? r.cancelled / r.total * 100 : 0}%` }} />
        </span></span><span>{day.date.slice(8)}</span></>;
      return onOpenRequests ? <button key={day.date} type="button" className="analytics-day" title={label} aria-label={`${label}；查看这天的请求`} onClick={() => onOpenRequests({ start: day.date, end: shiftDate(day.date, 1) })}>{content}</button> :
        <div key={day.date} className="analytics-day" title={label} aria-label={label}>{content}</div>;
    })}
  </div>;
}
function ResultLegend() {
  return <div className="analytics-legend"><span><i className="analytics-dot success" />成功</span><span><i className="analytics-dot failed" />失败</span><span><i className="analytics-dot cancelled" />取消</span></div>;
}
function Coverage({ metrics }: { metrics: AnalyticsMetrics }) {
  const u = metrics.usage, price = u.prices.directory;
  const known = [u.records, price.priced, price.valued].every(value => finite(value) && value >= 0) && price.valued <= price.priced && price.priced <= u.records;
  return <>
    <dl className="analytics-coverage-counts"><div><dt>已报告用量记录</dt><dd>{count(u.records)}</dd></div><div><dt>有记录目录价</dt><dd>{count(price.priced)}/{count(u.records)}</dd></div><div><dt>能够估值</dt><dd>{count(price.valued)}/{count(u.records)}</dd></div></dl>
    {known && u.records > 0 ? <>
      <div className="analytics-coverage-bar" role="img" aria-label={`${u.records} 条用量中 ${price.valued} 可估值，${price.priced - price.valued} 有价但不可估，${u.records - price.priced} 缺目录价格`}>
        <span className="analytics-segment success" style={{ width: `${price.valued / u.records * 100}%` }} />
        <span className="analytics-segment partial" style={{ width: `${(price.priced - price.valued) / u.records * 100}%` }} />
        <span className="analytics-segment cancelled" style={{ width: `${(u.records - price.priced) / u.records * 100}%` }} />
      </div>
      <div className="analytics-legend"><span><i className="analytics-dot success" />可估值 {count(price.valued)}</span><span><i className="analytics-dot partial" />有价但不可估 {count(price.priced - price.valued)}</span><span><i className="analytics-dot cancelled" />缺目录价 {count(u.records - price.priced)}</span></div>
    </> : <p className="muted">{known && u.records === 0 ? "此范围没有已报告用量。" : "价格覆盖尚未确认。"}</p>}
    <p className="muted">目录 API 等值已知小计 {formatAnalyticsMoney(price.usd)}；只覆盖可估值记录，未知不填零。</p>
  </>;
}
function ReportCharts({ report, onOpenRequests }: { report: AnalyticsReport; onOpenRequests?: (range: AnalyticsRange) => void }) {
  return <div className="analytics-chart-grid">
    <section className="panel analytics-chart"><div className="section-head"><h3>按日请求结果</h3><span className="muted">请求数（次）</span></div>
      <DailyResults days={report.days} onOpenRequests={onOpenRequests} /><ResultLegend />
      <p className="analytics-footnote">按本机日期统计当前保留的请求记录；历史缺失不会补记。</p>
    </section>
    <section className="panel analytics-chart"><div className="section-head"><h3>目录价覆盖了多少用量？</h3></div><Coverage metrics={report.summary} />
      <p className="analytics-footnote">每次调用上报的用量分别记录，包含重试；一次请求也可能没有用量报告。此分母与请求记录数不同。</p>
    </section>
  </div>;
}

/** Home-only overview: one local GET per mount, no polling or quota sampling. */
export function AnalyticsOverview({ onOpenAnalytics }: { onOpenAnalytics: (range: AnalyticsRange) => void }) {
  const range = useMemo(() => presetRange("7"), []);
  const observed = useAnalytics(range, false);
  return <section className="analytics-overview" aria-label="本地范围统计概览">
    <div className="section-head"><div><h3>最近 7 个本地自然日</h3><p className="muted">{rangeLabel(range)} · 与顶部三项原统计范围分别呈现</p></div>
      <button className="ghost" onClick={() => onOpenAnalytics(observed.report?.window || range)}>查看该范围报告</button></div>
    <ReadingState {...observed} />
    {observed.report && <><p className="muted">{observed.report.window.in_progress ? "包含未结束的今日，结果仍在变化。" : "所选日期已结束。"}仅统计已保存的本地记录，不证明从安装起完整。</p><ReportCharts report={observed.report} /></>}
  </section>;
}

function MetricSummary({ metrics }: { metrics: AnalyticsMetrics }) {
  const r = metrics.requests, u = metrics.usage;
  return <>
    <dl className="analytics-summary">
      <div><dt>请求记录</dt><dd>{count(r.total)}</dd><small>成功 {count(r.ok)} · 失败 {count(r.failed)} · 取消 {count(r.cancelled)}</small></div>
      <div><dt>成功率</dt><dd>{r.total === 0 ? "暂无请求" : finite(r.total) && finite(r.cancelled) && r.total === r.cancelled ? "取消不计成功率" : percent(r.success_rate)}</dd><small>成功 / 非取消请求；入口拒绝仍计入结果</small></div>
      <div><dt>用量报告</dt><dd>{count(u.records)} 条</dd><small>每次调用上报的用量分别记录，包含重试</small></div>
    </dl>
    <div className="analytics-quality-grid">
      <section className="panel"><h3>已知用量与缺失</h3><dl className="task-facts">
        <div><dt>输入 token（含规范化缓存输入）</dt><dd>{count(u.input.tokens)} · 已知 {count(u.input.known)}/{count(u.records)}，缺 {count(u.input.missing)}</dd></div>
        <div><dt>输出 token</dt><dd>{count(u.output.tokens)} · 已知 {count(u.output.known)}/{count(u.records)}，缺 {count(u.output.missing)}</dd></div>
        <div><dt>报告质量</dt><dd>输入与输出已报告 {count(u.reported)} · 部分 {count(u.partial)} · 估算 {count(u.estimated)}；缓存和价格覆盖另计</dd></div>
        <div><dt>缓存命中（有效输入范围）</dt><dd>{percent(u.cache.hit_rate)} · 已知命中读取 {count(u.cache.hit_read_tokens)} / 有效输入 {count(u.cache.hit_input_tokens)}；口径未知 {count(u.cache.unknown_records)} 条</dd></div>
        <div><dt>已报告缓存读取 / 创建</dt><dd>{count(u.cache.read_tokens)} / {count(u.cache.write_tokens)} token</dd></div>
      </dl><p className="analytics-footnote">全缺时显示未知；缓存命中率不使用所有读取 token 作分子，不从请求成功推断用量完整。</p></section>
      <section className="panel"><h3>成功请求的总耗时</h3><dl className="task-facts">
        <div><dt>已知样本 / 成功请求</dt><dd>{count(r.duration.known)} / {count(r.ok)} · 缺 {count(r.duration.missing)}</dd></div>
        <div><dt>P50</dt><dd>{duration(r.duration.p50_ms)}</dd></div><div><dt>P95</dt><dd>{duration(r.duration.p95_ms)}</dd></div>
        <div><dt>可确认调用次数的请求</dt><dd>{count(r.attempt_known_requests)} / {count(r.total)}；其中 {count(r.retried_requests)} 条发生过重试</dd></div>
      </dl><p className="analytics-footnote">仅统计成功且已报告耗时的样本。在这些样本中，至少一半在 P50 所示时间内完成，至少 95% 在 P95 所示时间内完成；不代表全部成功请求，也不是厂商 TTFT、项目工作时长或历史并发峰值。</p></section>
    </div>
  </>;
}
function PriceTable({ metrics }: { metrics: AnalyticsMetrics }) {
  const prices = metrics.usage.prices;
  const bases = [["directory", "提供方目录价"], ["declared", "本机声明价格"], ["legacy", "历史固定估算"], ["benchmark", "用户对标价格"]] as const;
  return <section className="panel analytics-prices"><h3>金额依据与各自覆盖</h3>
    <table><thead><tr><th scope="col">估值依据</th><th scope="col">已知 API 等值小计（USD）</th><th scope="col">有价 / 用量记录</th><th scope="col">可估值 / 用量记录</th><th scope="col">部分估值 / 缓存回退</th></tr></thead>
      <tbody>{bases.map(([id, label]) => <tr key={id}><th scope="row">{label}</th><td>{formatAnalyticsMoney(prices[id].usd)}</td>
        <td>{count(prices[id].priced)} / {count(metrics.usage.records)}</td><td>{count(prices[id].valued)} / {count(metrics.usage.records)}</td>
        <td>{count(prices[id].partial_valued)} / {count(prices[id].fallback_valued)}</td></tr>)}</tbody></table>
    <p className="analytics-footnote">依据分别呈现，不补当前价格、不把未知当免费，也不相加或相减为现金实付、订阅分摊或净节省。目录覆盖与用户对标覆盖可以不同。</p>
  </section>;
}
const changeReasons: Record<string, string> = {
  open_period: "当前范围包含未结束的自然日，暂不比较。",
  retention_unknown: "无法证明两个区间的数据均完整保留，暂不比较。",
  pending_settlement: "仍有待落盘结果，暂不比较。",
  no_baseline: "前期没有非零基线，不能计算增长率。",
  incomplete_pricing: "两期目录价格、用量或缓存估值覆盖不足，不比较金额。",
};
function changeText(change: AnalyticsChange, open: boolean): string {
  if (open) return changeReasons.open_period;
  if (change.reason) return changeReasons[change.reason] || "比较条件尚未确认。";
  return finite(change.rate) ? `${change.rate > 0 ? "+" : ""}${(change.rate * 100).toFixed(1)}%` : "比较结果未知。";
}
function Comparison({ report }: { report: AnalyticsReport }) {
  if (!report.comparison) return null;
  const comparison = report.comparison;
  return <section className="panel"><h3>与紧邻前期比较</h3><p className="muted">前期：{rangeLabel(comparison.window)} · 两期均按设备本地自然日</p>
    <dl className="task-facts"><div><dt>请求记录数量变化</dt><dd>{changeText(comparison.requests_change, report.window.in_progress)}</dd></div>
      <div><dt>目录 API 等值变化</dt><dd>{changeText(comparison.directory_usd_change, report.window.in_progress)}</dd></div></dl>
    <p className="analytics-footnote">只显示服务端已经证明可比的变化；有历史行不等于保留完整。不可比仍可导出当前汇总。</p>
  </section>;
}
type Dimension = "agents" | "providers" | "models";
const dimensions: { id: Dimension; label: string }[] = [{ id: "agents", label: "按工具" }, { id: "providers", label: "按来源" }, { id: "models", label: "按模型" }];
function groupLabel(group: AnalyticsGroup): string { return group.other ? "其余分组" : group.key == null ? "未记录 / 未确认" : group.key; }
function Groups({ report, dimension, onChange, onSelect }: { report: AnalyticsReport; dimension: Dimension; onChange: (dimension: Dimension) => void; onSelect: (index: number) => void }) {
  const groups = report.groups[dimension];
  return <section className="panel analytics-groups"><div className="section-head"><h3>独立分组</h3><nav className="task-workspace-nav" aria-label="统计分组维度">
    {dimensions.map(item => <button key={item.id} className="ghost" aria-pressed={dimension === item.id} onClick={() => onChange(item.id)}>{item.label}</button>)}
  </nav></div><p className="muted">请求按网关记录的选中来源 / 服务模型归组（型号可能回退到发出模型）；用量按每次调用所报告的来源 / 模型独立归组。同名行也不是一一关联。</p>
    {groups.length ? <table><thead><tr><th scope="col">{dimensions.find(item => item.id === dimension)?.label}</th><th scope="col">请求记录</th><th scope="col">独立用量记录</th><th scope="col">说明</th></tr></thead>
      <tbody>{groups.map((group, index) => <tr key={`${group.other}:${group.key ?? "unknown"}:${index}`}><th scope="row">{groupLabel(group)}</th>
        <td>{count(group.requests.total)}</td><td>{count(group.usage.records)}</td><td><button id={`analytics-group-${dimension}-${index}`} className="ghost" onClick={() => onSelect(index)}>查看此组汇总</button></td></tr>)}</tbody></table> : <p className="empty-state">此范围没有可呈现的分组。</p>}
    <p className="analytics-footnote">每维最多 20 组，超出单列“其余分组”；未记录不是“其余”。选择分组不会新增交叉过滤或请求 / 用量 join。</p>
  </section>;
}

const csvHeaders = ["date", "requests", "ok", "failed", "cancelled", "success_rate", "duration_known", "duration_missing", "p50_ms", "p95_ms", "usage_records", "reported", "partial", "estimated", "input_tokens", "output_tokens", "directory_priced", "directory_valued", "directory_usd", "declared_usd", "legacy_usd", "benchmark_priced", "benchmark_valued", "benchmark_usd"];
function csvNumber(value: unknown): string { return finite(value) ? String(value) : ""; }
export function analyticsCsv(report: AnalyticsReport): string {
  const lines = report.days.map(day => {
    if (!validDate(day.date) || day.date < report.window.start || day.date >= report.window.end) throw new Error("导出日期尚未确认");
    const r = day.metrics.requests, u = day.metrics.usage, p = u.prices;
    return [day.date, ...[r.total, r.ok, r.failed, r.cancelled, r.success_rate, r.duration.known, r.duration.missing,
      r.duration.p50_ms, r.duration.p95_ms, u.records, u.reported, u.partial, u.estimated, u.input.tokens, u.output.tokens,
      p.directory.priced, p.directory.valued, p.directory.usd, p.declared.usd, p.legacy.usd, p.benchmark.priced, p.benchmark.valued, p.benchmark.usd].map(csvNumber)].join(",");
  });
  return [csvHeaders.join(","), ...lines].join("\r\n") + "\r\n";
}

export interface AnalyticsProps {
  onBack?: () => void;
  backLabel?: string;
  onOpenRequests?: (range: AnalyticsRange) => void;
  initialRange?: AnalyticsRange;
  entryKey?: number;
}
export function Analytics({ onBack, backLabel = "返回请求记录", onOpenRequests, initialRange, entryKey }: AnalyticsProps) {
  const firstRange = useRef(initialRange || presetRange("7")).current;
  const [range, setRange] = useState<AnalyticsRange>(firstRange);
  const [draft, setDraft] = useState<AnalyticsRange>(firstRange);
  const [preset, setPreset] = useState<AnalyticsPreset | "custom">(initialRange ? "custom" : "7");
  const [compare, setCompare] = useState(false);
  const [rangeError, setRangeError] = useState("");
  const [downloadNote, setDownloadNote] = useState("");
  const [dimension, setDimension] = useState<Dimension>("agents");
  const [selectedGroup, setSelectedGroup] = useState<number | null>(null);
  const observed = useAnalytics(range, compare);
  const query = analyticsQuery(range, compare);
  const reportRoot = useRef<HTMLDivElement>(null);
  const groupScroll = useRef(0);
  useEffect(() => {
    if (!initialRange) return;
    setRange(initialRange); setDraft(initialRange); setPreset("custom"); setCompare(false); setRangeError("");
  }, [entryKey, initialRange?.start, initialRange?.end]);
  useEffect(() => { setSelectedGroup(null); setDownloadNote(""); }, [query]);
  const choose = (next: AnalyticsPreset) => {
    const value = presetRange(next); setRange(value); setDraft(value); setPreset(next); setRangeError("");
  };
  const apply = () => {
    if (!validRange(draft)) { setRangeError("请选择 1–31 个本机自然日，结束日期不含且不晚于明日。"); return; }
    setRange({ ...draft }); setPreset("custom"); setRangeError("");
  };
  const exportCsv = () => {
    if (!observed.report || observed.loading || observed.error || rangeError) return;
    try {
      const blob = new Blob([analyticsCsv(observed.report)], { type: "text/csv;charset=utf-8" });
      const url = URL.createObjectURL(blob), link = document.createElement("a");
      link.href = url; link.download = `aubridge-local-${observed.report.window.start}_${observed.report.window.end}-USD.csv`;
      document.body.appendChild(link); link.click(); link.remove(); window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      setDownloadNote("已请求下载当前范围的日汇总 CSV；请在下载位置核对文件。");
    } catch { setDownloadNote("汇总导出未完成，请重新读取后再试。"); }
  };
  const report = observed.report;
  const group = report && selectedGroup != null ? report.groups[dimension][selectedGroup] : null;
  const backToReport = () => {
    const focusId = `analytics-group-${dimension}-${selectedGroup}`;
    setSelectedGroup(null);
    requestAnimationFrame(() => {
      const scroll = reportRoot.current?.closest(".app-main"); if (scroll) scroll.scrollTop = groupScroll.current;
      document.getElementById(focusId)?.focus({ preventScroll: true });
    });
  };
  if (group) return <TaskView title={`${dimensions.find(item => item.id === dimension)?.label} · ${groupLabel(group)}`} backLabel="返回使用统计" onBack={backToReport}>
    <p className="muted">{rangeLabel(range)} · 两类分母独立，不代表真实项目或 CLI 任务数。</p><MetricSummary metrics={group} /><PriceTable metrics={group} />
  </TaskView>;
  const content = <div ref={reportRoot} className="analytics-report">
    <div className="analytics-range-presets" aria-label="统计本地日期范围">{([
      ["today", "今日"], ["7", "最近 7 天"], ["30", "最近 30 天"], ["complete7", "上一个完整 7 日"],
    ] as const).map(([id, label]) => <button key={id} className="ghost" aria-pressed={preset === id} onClick={() => choose(id)}>{label}</button>)}</div>
    <form className="analytics-range-form" onSubmit={event => { event.preventDefault(); apply(); }}>
      <label className="field">开始日期（包含）<input type="date" value={draft.start} onChange={event => setDraft(value => ({ ...value, start: event.target.value }))} /></label>
      <label className="field">结束日期（不含）<input type="date" value={draft.end} onChange={event => setDraft(value => ({ ...value, end: event.target.value }))} /></label>
      <button type="submit">查看这个范围</button>
    </form>
    {rangeError && <p className="alert error" role="alert">{rangeError}</p>}
    <p className="muted">已请求：{rangeLabel(range)} · 设备本地自然日；单窗最多 31 天，仅统计已保存的本地记录。</p>
    <div className="actions"><label className="row"><input type="checkbox" checked={compare} onChange={event => setCompare(event.target.checked)} />比较紧邻的前期（同自然日数）</label>
      <button className="ghost" disabled={observed.loading} onClick={observed.retry}>重新读取</button>
      <button className="ghost" disabled={!report || observed.loading || Boolean(observed.error) || Boolean(rangeError)} onClick={exportCsv}>导出日汇总 CSV</button>
    </div>
    <ReadingState {...observed} />
    {report && <>
      <p className="analytics-retention" role="status">{report.window.in_progress ? "含未结束的自然日，当前结果仍在变化。" : "所选自然日已结束。"}仅统计已保存的本地记录；清理过的历史和缺失报告不会补记。观测时间：{new Date(report.window.observed_at * 1000).toLocaleString()}。</p>
      <MetricSummary metrics={report.summary} /><ReportCharts report={report} onOpenRequests={onOpenRequests} /><PriceTable metrics={report.summary} /><Comparison report={report} />
      <Groups report={report} dimension={dimension} onChange={value => { setDimension(value); setSelectedGroup(null); }} onSelect={index => { groupScroll.current = reportRoot.current?.closest(".app-main")?.scrollTop || 0; setSelectedGroup(index); }} />
    </>}
    <p className="analytics-footnote">CSV 仅含当前成功读取范围的每日固定汇总列：日期、计数、用量质量、覆盖和金额；未知留空、已知零为 0。success_rate 为 0–1 比例，金额单位 USD；文件结束日期不含。无请求 / 会话 ID、工具 / 来源 / 模型名、路径、账号、正文或凭据。</p>
    {downloadNote && <p role="status">{downloadNote}</p>}
  </div>;
  return onBack ? <TaskView title="使用统计" backLabel={backLabel} onBack={onBack}>{content}</TaskView> : <section aria-label="使用统计">{content}</section>;
}

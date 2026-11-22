/** D59: aggregate-only local ledger facts, never joined request/usage rows. */
export interface AnalyticsRange { start: string; end: string }
export interface AnalyticsWindow extends AnalyticsRange {
  timezone: "local";
  start_ts: number;
  end_ts: number;
  observed_at: number;
  in_progress: boolean;
  scope: "retained_ledger";
}
export interface RequestMetrics {
  total: number; ok: number; failed: number; cancelled: number;
  success_rate: number | null;
  attempt_known_requests: number; retried_requests: number;
  duration: { known: number; missing: number; p50_ms: number | null; p95_ms: number | null };
}
export interface TokenMetrics { tokens: number | null; known: number; missing: number }
export interface PriceMetrics {
  priced: number; valued: number; usd: number | null;
  partial_valued: number; fallback_valued: number;
}
export interface UsageMetrics {
  records: number; reported: number; partial: number; estimated: number;
  input: TokenMetrics; output: TokenMetrics;
  cache: {
    read_tokens: number | null; write_tokens: number | null;
    hit_read_tokens: number; hit_input_tokens: number;
    hit_rate: number | null; unknown_records: number;
  };
  prices: { directory: PriceMetrics; declared: PriceMetrics; legacy: PriceMetrics; benchmark: PriceMetrics };
}
export interface AnalyticsMetrics { requests: RequestMetrics; usage: UsageMetrics }
export interface AnalyticsDay { date: string; metrics: AnalyticsMetrics }
export interface AnalyticsGroup extends AnalyticsMetrics { key: string | null; other: boolean }
export interface AnalyticsChange { rate: number | null; reason: string | null }
export interface AnalyticsReport {
  window: AnalyticsWindow;
  summary: AnalyticsMetrics;
  days: AnalyticsDay[];
  groups: { agents: AnalyticsGroup[]; providers: AnalyticsGroup[]; models: AnalyticsGroup[] };
  comparison: null | {
    window: AnalyticsWindow; summary: AnalyticsMetrics;
    requests_change: AnalyticsChange; directory_usd_change: AnalyticsChange;
  };
}

export function localDate(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}
export function validDate(value: string): boolean {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const [year, month, day] = value.split("-").map(Number);
  return year >= 1970 && localDate(new Date(year, month - 1, day)) === value;
}
export function shiftDate(value: string, days: number): string {
  const [year, month, day] = value.split("-").map(Number);
  return localDate(new Date(year, month - 1, day + days));
}
export type AnalyticsPreset = "today" | "7" | "30" | "complete7";
export function presetRange(preset: AnalyticsPreset): AnalyticsRange {
  const today = localDate(new Date());
  return preset === "complete7" ? { start: shiftDate(today, -7), end: today } : {
    start: shiftDate(today, preset === "today" ? 0 : preset === "30" ? -29 : -6),
    end: shiftDate(today, 1),
  };
}
export function validRange(range: AnalyticsRange): boolean {
  if (!validDate(range.start) || !validDate(range.end)) return false;
  const calendarDay = (value: string) => {
    const [year, month, day] = value.split("-").map(Number);
    return Date.UTC(year, month - 1, day) / 86_400_000;
  };
  const days = calendarDay(range.end) - calendarDay(range.start);
  return days >= 1 && days <= 31 && range.end <= shiftDate(localDate(new Date()), 1);
}
export function analyticsQuery(range: AnalyticsRange, compare = false): string {
  const query = new URLSearchParams({ start: range.start, end: range.end });
  if (compare) query.set("compare", "previous");
  return query.toString();
}

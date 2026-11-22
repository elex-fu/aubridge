interface GatewayAccess {
  endpoint: string;
  admin_token: string;
}

export interface BuildInfo {
  version: string;
  git_revision: string | null;
  git_tree: string | null;
  mode: "source" | "development" | "distribution";
  profile: string;
  source_verified: boolean;
  pid: number;
  capabilities: string[];
}

interface TauriCore {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
}

export function readableActionError(error: unknown, fallback?: string): string {
  const message = error instanceof Error ? error.message : String(error);
  if (message.includes("update-maintenance-active")) return "正在准备更新，请稍后再试。";
  if (message.includes("update-busy")) return "仍有任务运行，请在任务完成后重试更新。";
  if (message.includes("update-requires-local-desktop-window")) return "请从已安装的 macOS 桌面 App 使用更新功能。";
  return fallback || message;
}

export interface DesktopUpdateStatus {
  state: "not_configured" | "checking" | "current" | "available" | "preparing" | "ready" | "busy" | "installing" | "installed" | "rolled_back" | "failed";
  version: string | null;
  offer_handle: string | null;
  prepared_handle: string | null;
  reason: string | null;
}

function updateCommand(command: "update_status" | "check_update" | "prepare_update" | "prepare_development_update" | "install_update", args?: Record<string, unknown>): Promise<DesktopUpdateStatus> {
  const core = (window as Window & { __TAURI__?: { core: TauriCore } }).__TAURI__?.core;
  if (!core) return Promise.reject(new Error("请从已安装的 macOS 桌面 App 使用更新功能。"));
  return core.invoke<DesktopUpdateStatus>(command, args);
}
export const desktopUpdateStatus = () => updateCommand("update_status");
export const checkDesktopUpdate = () => updateCommand("check_update");
export const prepareDesktopUpdate = (offerHandle: string) => updateCommand("prepare_update", { offerHandle });
export const prepareDevelopmentUpdate = (archivePath: string, receiptPath: string, independentSha256: string) =>
  updateCommand("prepare_development_update", { archivePath, receiptPath, independentSha256, confirmed: true });
export const installDesktopUpdate = (preparedHandle: string) => updateCommand("install_update", { preparedHandle, confirmed: true });

interface TauriEvent {
  listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<() => void>;
}

export function listenServingChanged(handler: (serving: boolean) => void): Promise<() => void> {
  const event = (window as Window & { __TAURI__?: { event?: TauriEvent } }).__TAURI__?.event;
  if (!event) return Promise.reject(new Error("原生状态通知不可用"));
  return event.listen<boolean>("aubridge-serving-changed", message => {
    if (typeof message.payload === "boolean") handler(message.payload);
  });
}

let access: Promise<GatewayAccess> | undefined;

export function resetGatewayAccess(): void { access = undefined; }

function gatewayAccess(): Promise<GatewayAccess> {
  const core = (window as Window & { __TAURI__?: { core: TauriCore } }).__TAURI__?.core;
  if (!core) return Promise.reject(new Error("请从 aubridge 桌面窗口打开管理页"));
  return (access ??= core.invoke<GatewayAccess>("gateway_access").catch(error => {
    access = undefined;
    throw error;
  }));
}

export async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const { endpoint, admin_token } = await gatewayAccess();
  const headers = new Headers(init?.headers);
  headers.set("Authorization", "Bearer " + admin_token);
  const r = await fetch(endpoint + path, { ...init, headers, redirect: "error" });
  if (!r.ok) {
    const failure = await r.json().catch(() => null);
    const error = failure?.error;
    const detail = typeof error === "string" ? error : error?.message;
    const message = typeof detail === "string" ? detail : "status " + r.status;
    const syncIssue = typeof failure?.opencode_issue === "string" ? ` · OpenCode 模型同步失败 · ${failure.opencode_issue}` : "";
    throw new Error(message + syncIssue);
  }
  return r.json() as Promise<T>;
}

// The page test is ordinary inference attributed to ui-test, never an admin call.
async function fetchModelTest(path: string, signal: AbortSignal, body?: unknown): Promise<Response> {
  const { endpoint } = await gatewayAccess();
  signal.throwIfAborted();
  return fetch(endpoint + path, {
    method: body === undefined ? "GET" : "POST",
    headers: {
      Authorization: "Bearer aubridge-ui-test",
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal,
    cache: "no-store",
    redirect: "error",
  });
}

function responseObject(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}

function shortTestText(value: unknown, limit = 240): string | null {
  if (typeof value !== "string" || !value.trim()) return null;
  const chars = Array.from(value.trim());
  return chars.length > limit ? chars.slice(0, limit - 1).join("") + "…" : chars.join("");
}

function modelTestError(body: Record<string, unknown> | null, status: number): string {
  const error = responseObject(body?.error);
  const code = shortTestText(error?.type, 80);
  const message = shortTestText(error?.message);
  if (code || message) return [code, message].filter(Boolean).join(" · ");
  if (status === 503) return "代理暂未接收此请求，请确认网关状态与模型源后重试。";
  if (status === 401 || status === 403) return "请求被拒绝，请检查本机代理与模型源状态。";
  if (status === 429) return "请求受到限流或冷却，请稍后重试。";
  return "未收到有效回复，可检查模型源或查看现有路由诊断。";
}

export async function fetchTestModels(signal: AbortSignal): Promise<string[]> {
  const response = await fetchModelTest("/v1/models", signal);
  const body = responseObject(await response.json().catch(error => {
    if (signal.aborted) throw error;
    return null;
  }));
  if (!response.ok) throw new Error(`HTTP ${response.status} · ${modelTestError(body, response.status)}`);
  if (!body || !Array.isArray(body.data) || body.data.some(entry => {
    const id = responseObject(entry)?.id;
    return typeof id !== "string" || !id.trim();
  })) throw new Error("模型目录格式无效，请刷新目录重试。");
  const models = Array.from(new Set(body.data.map(entry => responseObject(entry)!.id as string)));
  return models.includes("aubridge") ? ["aubridge", ...models.filter(id => id !== "aubridge")] : models;
}

export interface ModelTestReceipt {
  state: "success" | "failed" | "unconfirmed";
  httpStatus: number;
  provider: string | null;
  responseModel: string | null;
  reply: string | null;
  truncated: boolean;
  message: string;
}

/** Only the Messages completion contract can establish this request's success. */
export function classifyModelTest(status: number, value: unknown, provider: string | null): ModelTestReceipt {
  const body = responseObject(value);
  const source = provider?.trim();
  const result: ModelTestReceipt = {
    state: "failed", httpStatus: status,
    provider: source && source !== "none" ? source : null,
    responseModel: typeof body?.model === "string" && body.model.trim() ? body.model : null,
    reply: null, truncated: false, message: "",
  };
  const content = Array.isArray(body?.content) ? body.content : [];
  if (status < 200 || status >= 300 || body?.error != null || body?.type === "error" ||
      content.some(block => responseObject(block)?.type === "error")) {
    return { ...result, message: modelTestError(body, status) };
  }
  if (body?.type !== "message" || body.role !== "assistant") {
    return { ...result, message: "响应不是有效的 Messages 回复，请查看路由诊断。" };
  }
  const text = content.flatMap(block => {
    const entry = responseObject(block);
    return entry?.type === "text" && typeof entry.text === "string" && entry.text.trim() ? [entry.text] : [];
  }).join("\n");
  result.reply = shortTestText(text);
  if (!result.reply) return { ...result, message: "未收到非空普通文本回复，本次测试未成功。" };
  if (content.some(block => responseObject(block)?.type === "tool_use")) {
    return { ...result, state: "unconfirmed", message: "响应包含工具调用，未确认本次文本请求完成。" };
  }
  if (typeof body.stop_reason !== "string" || !["end_turn", "stop_sequence", "max_tokens"].includes(body.stop_reason)) {
    return { ...result, state: "unconfirmed", message: "响应未提供明确完成的终态，未确认本次完成。" };
  }
  return {
    ...result, state: "success", truncated: body.stop_reason === "max_tokens",
    message: body.stop_reason === "max_tokens" ? "本次成功（回复已截断）" : "本次成功",
  };
}

export async function testProxyModel(model: string, signal: AbortSignal): Promise<ModelTestReceipt> {
  const response = await fetchModelTest("/v1/messages", signal, {
    model, stream: false, max_tokens: 512,
    messages: [{ role: "user", content: "只回复 OK。" }],
  });
  const body: unknown = await response.json().catch(error => {
    if (signal.aborted) throw error;
    return null;
  });
  return classifyModelTest(response.status, body, response.headers.get("x-aubridge-provider"));
}

export function postJson<T>(path: string, body: unknown): Promise<T> {
  return fetchJson<T>(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

export function deleteJson<T>(path: string): Promise<T> {
  return fetchJson<T>(path, { method: "DELETE" });
}

export interface OpenCodeSyncReceipt {
  opencode_issue?: string | null;
}

export function reportOpenCodeSync(receipt: OpenCodeSyncReceipt, setErr: (message: string) => void) {
  if (receipt.opencode_issue) setErr(`更改已保存，OpenCode 模型同步失败 · ${receipt.opencode_issue}`);
}

// ---- /admin/dashboard(Agents 汇总区数据源,ui-design §6.1)----

export interface ProviderRow {
  id: string;
  kind: string;
  models?: string[];
  remaining: number | string | null;
  resets_at: number | null;
  util_5h: number | null;
  util_7d: number | null;
  enabled: boolean;
  paused?: boolean;
  resting: boolean;
  resting_until: number | null;
  sunk: boolean;
  healthy: boolean;
  ready: boolean;
  breaker_open?: boolean;
}

export interface TopProvider {
  provider: string;
  benchmark_cost_usd: number;
}

// 最近决策(变更记录 31 批 2 ⑤):attempts 为 [{id, status}] 数组
// (status 0 = 未发出:凭据/网络),坏行为原样字符串容错。
export interface DecisionAttempt {
  id: string;
  status: number;
  model?: string;
  requested_model?: string;
  kind?: string;
  chosen?: boolean;
  reason?: string;
  duration_ms?: number;
  cache_read_tokens?: number | null;
  cache_write_tokens?: number | null;
  upstream_request_id?: string | null;
}

export interface SkippedCandidate {
  id: string;
  model: string;
  kind: string;
  reason: string;
  retry_at?: number;
  context_window?: number;
  estimated_input_tokens?: number;
}

export interface DecisionRow {
  session_id?: string | null;
  parent_session_id?: string | null;
  purpose?: string | null;
  request_id?: string | null;
  request_path?: string | null;
  request_bytes?: number | null;
  cache_read_tokens?: number | null;
  cache_write_tokens?: number | null;
  input_includes_cache?: boolean | null;
  price_snapshot?: PriceSnapshot | null;
  benchmark_snapshot?: PriceSnapshot | null;
  benchmark_estimate?: ValueEstimate | null;
  estimate?: ValueEstimate | null;
  ts: number;
  model: string;
  agent: string;
  provider: string;
  served_model?: string | null;
  input_tokens?: number | null;
  output_tokens?: number | null;
  first_content_ms?: number | null;
  duration_ms?: number | null;
  attempts: DecisionAttempt[] | string;
  skipped?: SkippedCandidate[];
  outcome: string;
}

export interface Dashboard {
  value_summary?: ValueSummary | null;
  monthly_savings_status?: "available" | "unavailable";
  monthly_savings_usd?: number | null;
  share_card: string | null;
  providers?: ProviderRow[];
  top_providers?: TopProvider[] | null;
  recent_decisions?: DecisionRow[] | null;
}

export interface PriceSnapshot {
  currency: string; unit: string;
  model: string; provider?: string; basis?: string; target_provider?: string | null; target_model?: string | null; version?: string | null; source: string; observed_at: number | null;
  price: { input_per_million: number; output_per_million: number;
    cache_read_per_million: number | null; cache_write_per_million: number | null };
}

export interface ValueEstimate {
  api_equivalent_usd: number;
  cache_rate_fallback: boolean;
  partial_usage: boolean;
  cache_value_delta_usd: number | null;
}

export interface ValueSummary {
  records: number; priced_records: number; unpriced_records: number; valued_records: number;
  builtin_priced_records: number; partial_usage_records: number; api_equivalent_usd: number | null;
  official_usd: number | null; official_valued_records: number; official_priced_records: number; official_price_coverage: number | null;
  benchmark_priced_records: number; benchmark_valued_records: number; benchmark_coverage: number | null; benchmark_usd: number | null;
  price_coverage: number | null; cache_read_tokens: number; cache_write_tokens: number;
  cache_reported_records: number; cache_unknown_records: number; cache_input_tokens: number;
  cache_hit_rate: number | null; cache_rate_fallback_records: number;
  cache_value_delta_usd: number | null; cache_value_known_records: number;
}

// ---- /admin/plugins ----

export interface PluginRow {
  id: string;
  version: string;
  kind: string;
  models?: string[];
  enabled: boolean;
}

export interface PluginsPage {
  dir: string | null;
  auto_enable: boolean;
  plugins?: PluginRow[];
  invalid?: Record<string, string[]>;
}

// ---- /admin/settings(Provider 页凭据回写区,§7)----

export interface StashMeta {
  mtime: number;
  sha256: string;
  backed_up_at: number;
}

export type CostIntent = "confirmed_free_only" | "free_then_subscription";
export interface SubscriptionAuthorization {
  provider: string;
  basis: "personal_channel" | "user_declared";
  connection_identity: string;
}
export interface CostSettings {
  cost_intent: CostIntent | null;
  subscription_authorizations: SubscriptionAuthorization[];
  cost_policy_issue?: string | null;
}
export interface SourceHealth {
  credential: { present: boolean; usable: boolean; required?: boolean };
  paused: boolean;
  pending_rebind?: boolean;
  directory: { fetched_at: number | null; stale: boolean; issue: string | null };
  last_check: { status: string; model?: string; scope?: string; observed_at?: number } | null;
  last_real_success_at: number | null;
  cost: {
    basis: "confirmed_free" | "authorized_subscription" | "unconfirmed";
    connection_identity: string;
    subscription_eligible: boolean;
    authorized?: boolean;
    subscription_billing_notice?: string | null;
  };
}
export type HealthProvider = ProviderSourceRow;

export interface SettingsPage extends CostSettings {
  share_login: boolean;
  wired: boolean;
  stash: StashMeta | null;
  risk: string;
  chatgpt_login_source: "codex" | "opencode" | null;
  chatgpt_login_status: "ready" | "missing" | "unavailable";
  chatgpt_login_path: string | null;
}

// ---- /admin/agents(§8.6,变更记录 30/32-②)----

export function supportsWiring(id: string): boolean {
  return id === "claude-code" || id === "opencode" || id === "codex" || id === "gemini";
}

export interface AgentRow {
  id: string;
  name: string;
  wired: boolean;
  detected: boolean;
  /** 仅 Claude Code:clean / replaced(接线字段被第三方改写)/ unwired。 */
  drift: string | null;
  /** 残余用户动作(如 codex 的 export AUBRIDGE_TOKEN=...)。 */
  hint: string | null;
  launch_command?: string | null;
  /** 当前使用模型(变更记录 32-②:编辑弹层数据源;opencode 为 null)。 */
  model: string | null;
  /** 本 agent token = aubridge-<id>,可复制。 */
  token: string | null;
  discovery?: AgentDiscovery;
  validation?: AgentValidationSummary;
}

export interface AgentDiscovery {
  status: "not_checked" | "not_found" | "not_executable" | "version_unknown" | "version_unsupported" | "ready";
  version?: string | null;
  origin?: "override" | "path" | "known_location" | null;
  observed_at?: number | null;
}

export type ValidationStatus = "queued" | "running" | "passed" | "failed" | "cancelled" | "timed_out";
export interface AgentValidationSummary {
  last_job_id?: string | null;
  status: ValidationStatus | "not_run";
  scope?: string | null;
  observed_at?: number | null;
  valid: boolean;
  project_handle?: string | null;
  project_alias?: string | null;
}
export interface AgentValidationJob {
  job_id: string;
  agent_id: string;
  scope: string;
  project_handle?: string | null;
  project_alias?: string | null;
  status: ValidationStatus;
  reason?: string | null;
  started_at?: number | null;
  finished_at?: number | null;
  evidence?: {
    cli_version: string;
    gateway_revision: string;
    session_id: string;
    requests: { request_id: string; provider: string; served_model: string; input_tokens?: number | null; output_tokens?: number | null }[];
    tool_result_matched: boolean;
    final_exact: boolean;
  } | null;
}

export interface AgentsPage {
  configured: boolean;
  agents?: AgentRow[];
  /** 模型下拉(自动别名置首 + 聚合字面模型 + 分组名)。 */
  models?: string[];
}

/** 高级配置:Claude Code 模型角色(settings.json env 的默认模型映射)。 */
export interface AgentModelRole {
  label: string;
  model: string | null;
  name: string | null;
  one_million: boolean;
}

export interface AgentTestInfo {
  supported: boolean;
  command: string | null;
  prompt: string | null;
  note: string | null;
}

export interface AgentConfigPage {
  id: string;
  name: string;
  kind: "claude-code" | "codex";
  detected: boolean;
  wired: boolean;
  path: string;
  /** 乐观锁:保存时原样带回。 */
  revision: string;
  config: Record<string, unknown> | null;
  roles?: Record<string, AgentModelRole> | null;
  text?: string | null;
  /** Claude Code 的密钥环境变量读取时以 •••••••• 显示。 */
  secrets_masked: boolean;
  test: AgentTestInfo;
}

export interface AgentTestResult {
  ok: boolean;
  exit_code: number | null;
  timed_out: boolean;
  duration_ms: number;
  stdout: string;
  stderr: string;
  message: string;
}

export interface DisconnectPreview {
  changes: { field: string; action: "restore" | "remove" | "keep" }[];
  blocked: boolean;
}

// ---- /admin/providers + /admin/keys(变更记录 32-①)----

export interface ProviderSourceRow {
  source_health?: SourceHealth;
  id: string;
  kind: string;
  models?: string[];
  base: string;
  model_facts?: Record<string, ModelFacts>;
  model_availability?: Record<string, boolean | null>;
  store_key: boolean;
  /** 仅报告 Key 是否存在，凭据值永不回显。 */
  key_set: boolean;
  key_source?: "saved" | "environment" | "opencode" | "none";
  ready: boolean;
  /** Only the built-in ChatGPT channel reports a selected CLI login source. */
  login_source?: "codex" | "opencode" | null;
  paused?: boolean;
  routing_disabled?: boolean;
  free_requests?: { model: string; status: string; retry_at: number | null }[] | null;
  routing_status?: { model: string; status: string; retry_at: number | null }[] | null;
  model_discovery_supported?: boolean;
  /** 利用率百分比 0–100(被动采样/主动查询;null = 未知)。 */
  util_5h?: number | null;
  util_7d?: number | null;
  /** 额度重置时刻(unix 秒)。 */
  resets_at?: number | null;
  /** preset / custom / plugin。 */
  source: string;
  quota?: ProviderQuota | null;
}

export interface ModelFacts {
  source?: "declared" | "vendor-directory";
  name?: string | null;
  context?: number | null;
  output?: number | null;
  tools?: boolean | null;
  image_input?: boolean | null;
  reasoning?: boolean | null;
  reasoning_efforts?: string[];
  input_modalities?: string[];
  output_modalities?: string[];
  protocol?: string | null;
  observed_at?: number | null;
  free_evidence: "unknown" | "declared-quota" | "vendor-zero-price" | "subscription";
  price?: { input_per_million: number; output_per_million: number;
    cache_read_per_million: number | null; cache_write_per_million: number | null } | null;
}

export interface ModelDirectory {
  fetched_at: number | null;
  checked_at?: number | null;
  stale: boolean;
  issue: string | null;
  models: { id: string; facts: ModelFacts }[];
}

export interface ProviderQuota {
  observed_at: number | null;
  checked_at: number | null;
  stale: boolean;
  issue: "authentication" | "rate-limited" | "network" | "timeout" | "upstream" | "invalid-response" | "unsupported" | null;
  revision: number;
  history_unavailable: boolean;
  windows: {
    id: string;
    span_secs: number;
    used_percent: number;
    resets_at: number | null;
    observed_at: number;
    source: "api" | "response-headers";
    stale: boolean;
  }[];
}

export interface ProviderCheck {
  status: string;
  model: string | null;
  upstream_status: number | null;
}

export interface ProviderModelCatalog {
  models: string[];
}

export interface OpenRouterCatalog extends OpenCodeSyncReceipt {
  fetched_at: number | null;
  stale: boolean;
  issue: "network" | "http-status" | "invalid-directory" | "storage" | null;
  models: { id: string; tools: boolean; context: number | null }[];
}

export interface ProvidersPage {
  providers?: ProviderSourceRow[];
  openrouter_catalog?: OpenRouterCatalog | null;
  openrouter_key_budget?: {
    limit: number | null;
    limit_remaining: number | null;
    limit_reset: string | null;
  } | null;
}

/** POST /admin/providers body(CustomProvider 字段 + 可选 key)。 */
export interface AddProviderInput {
  id: string;
  kind: "free_tier" | "subscription";
  base: string;
  models: string[];
  protocol: "chat" | "anthropic";
  auth: "bearer" | "api-key";
  key?: string;
}

// ---- /admin/import/cc-switch(cc-switch-import-design §4.3)----

export interface CcSwitchCandidate {
  /** `app_type/id`,POST 回传凭据。 */
  reference: string;
  name: string;
  source: string;
  skip?: string | null;
  /** key 永不回显;provider 缺席 = 不可导入(skip 有原因)。 */
  provider?: {
    id: string;
    base: string;
    protocol: string;
    auth: string;
    kind: string;
    models: string[];
  } | null;
}

export interface CcSwitchImportPage {
  items?: CcSwitchCandidate[];
}

export interface CcSwitchImportResult {
  added: string[];
  failed: { reference: string; error: string }[];
}

// ---- /admin/quota-history(quota-plan-design §3.4,Provider 页曲线)----

export interface QuotaPoint {
  at: number;
  /** 剩余百分比 0–100。 */
  left: number;
  resets_at: number | null;
}

export interface QuotaHistoryItem {
  provider: string;
  window: string;
  legacy?: boolean;
  points: QuotaPoint[];
}

export interface QuotaHistoryPage {
  items?: QuotaHistoryItem[];
}

// ---- /admin/model-groups(变更记录 32-③)----

export interface ModelGroup {
  name: string;
  members: string[];
  /** free_first / free_only / sequential / round_robin / smart / pace。 */
  rule: string;
}

export interface ModelGroupsPage {
  groups?: ModelGroup[];
}

export interface ModelGroupPreview {
  serving?: boolean;
  model: string;
  candidates: Pick<SkippedCandidate, "id" | "model" | "kind">[];
  skipped: SkippedCandidate[];
}

// ---- /admin/gateway(变更记录 32-④,代理页)----

export interface GatewayPage {
  request_limits?: {
    wire_body_bytes: number; decoded_body_bytes: number; body_read_timeout_secs: number;
    admission_timeout_secs: number; max_requests: number; active_requests: number;
    memory_budget_bytes: number; reserved_bytes: number; encodings: string[];
  };
  serving: boolean;
  endpoint: string;
  admin_token: string;
  history_status?: "available" | "unavailable";
  requests?: DecisionRow[] | null;
  next?: string | null;
}

export interface BenchmarkMapping {
  provider: string; model: string; target_provider: string; target_model: string;
  source: string; version: string; observed_at: number; price: PriceSnapshot["price"];
}
export interface PricingPage { mappings: BenchmarkMapping[]; currency: string; unit: string }

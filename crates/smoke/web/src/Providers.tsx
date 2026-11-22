import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  AddProviderInput,
  AgentsPage,
  CcSwitchCandidate,
  CcSwitchImportResult,
  OpenRouterCatalog,
  ProviderCheck,
  ProviderModelCatalog,
  ProviderSourceRow,
  OpenCodeSyncReceipt,
  ProvidersPage,
  QuotaHistoryItem,
  QuotaHistoryPage,
  deleteJson,
  fetchJson,
  postJson,
  reportOpenCodeSync,
  readableActionError,
  supportsWiring,
} from "./api";
import { fmtRouteReason, fmtTime, fmtUtil } from "./format";
import { Dialog } from "./Dialog";
import { TaskView } from "./TaskView";
import { ModelDirectory, ModelFactsText } from "./ModelDirectory";
import { CostIntentPanel } from "./CostIntent";
import { SourceHealthFacts } from "./SourceHealthFacts";

const CATALOG_ISSUES = {
  network: "无法连接到目录",
  "http-status": "目录服务返回错误",
  "invalid-directory": "目录响应无法识别",
  storage: "无法读写本地目录缓存",
};

// 配额窗口曲线(quota-plan-design §3.5):每个窗口一条剩余百分比
// 折线 + 虚线均匀消耗参考线(同起点到 reset 时刻耗尽)。挂载即拉
// 一次(本机 admin 面,本地 SQLite,成本可忽略)。单 series,窗口
// 名即标题;文字用文本 token,不用线的颜色。
function QuotaCurve({ provider, setErr }: { provider: string; setErr: (s: string) => void }) {
  const [data, setData] = useState<QuotaHistoryItem[] | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (data || failed) return;
    fetchJson<QuotaHistoryPage>(`/admin/quota-history?${new URLSearchParams({ provider })}`)
      .then(page => setData(page.items ?? []))
      .catch(e => {
        setFailed(true);
        setErr(String(e));
      });
  }, [provider, data, failed, setErr]);
  if (failed) return <p className="muted">窗口历史暂不可读。</p>;
  if (!data) return <p className="muted">读取窗口历史…</p>;
  if (!data.length) return <p className="muted">暂无窗口读数历史（使用中会自动采样）。</p>;
  return (
    <div className="quota-curves">
      {data.map(item => (
        <QuotaWindowLine key={`${item.window}:${item.legacy ?? false}`} item={item} />
      ))}
    </div>
  );
}

function QuotaWindowLine({ item }: { item: QuotaHistoryItem }) {
  const points = (item.points ?? []).filter(
    p => p != null && typeof p.at === "number" && typeof p.left === "number",
  );
  if (points.length < 2) return null; // 一点画不了线
  const W = 320;
  const H = 96;
  const PAD = 4;
  const at0 = points[0].at;
  const at1 = points[points.length - 1].at;
  const span = Math.max(at1 - at0, 1);
  // 参考线终点:最后一点的 reset(均匀消耗 = 周期起点满额到 reset
  // 时刻耗尽);超出图范围截到右缘。
  const reset = points[points.length - 1].resets_at;
  const x = (at: number) => PAD + ((at - at0) / span) * (W - 2 * PAD);
  const y = (left: number) => PAD + (1 - Math.max(0, Math.min(100, left)) / 100) * (H - 2 * PAD);
  const path = points.map((p, i) => `${i === 0 ? "M" : "L"}${x(p.at).toFixed(1)},${y(p.left).toFixed(1)}`).join(" ");
  const reference = reset != null && reset > at0
    ? `M${x(at0).toFixed(1)},${y(points[0].left).toFixed(1)} L${x(Math.min(reset, at0 + span * 1.15)).toFixed(1)},${(H - PAD).toFixed(1)}`
    : null;
  return (
    <figure className="quota-curve">
      <figcaption className="muted">
        {item.window === "5h" ? "5 小时窗口" : item.window === "7d" ? "7 天窗口" : item.window}
        {" · 剩余百分比(虚线 = 均匀消耗参考)"}
        {item.legacy && " · 未区分登录身份的旧记录"}
      </figcaption>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className="quota-curve-svg"
        role="img"
        aria-label={`${item.window} 窗口剩余额度曲线,共 ${points.length} 个读数,当前剩余 ${points[points.length - 1].left.toFixed(0)}%`}
      >
        {/* recessive 网格:0 / 50 / 100 */}
        {[100, 50, 0].map(v => (
          <line key={v} x1={PAD} x2={W - PAD} y1={y(v)} y2={y(v)} className="quota-grid" />
        ))}
        {reference && <path d={reference} className="quota-reference" />}
        <path d={path} className="quota-line" />
        {points.map(p => (
          <circle key={p.at} cx={x(p.at)} cy={y(p.left)} r={3} className="quota-point">
            <title>{`${fmtTime(p.at)} · 剩余 ${p.left.toFixed(0)}%${p.resets_at != null ? ` · 重置 ${fmtTime(p.resets_at)}` : ""}`}</title>
          </circle>
        ))}
      </svg>
    </figure>
  );
}

function QuotaStatus({ quota }: { quota: ProviderSourceRow["quota"] }) {
  if (!quota) return null;
  const issues = {
    authentication: "额度接口拒绝了当前登录或 Key",
    "rate-limited": "额度查询暂时被限速",
    network: "暂时无法连接额度服务",
    timeout: "额度查询超时",
    upstream: "额度服务返回错误",
    "invalid-response": "额度服务返回了无法识别的读数",
    unsupported: "当前来源未提供可查询的套餐窗口",
  };
  return <div className="quota-status">
    <p className="muted">
      最近有效读数：{quota.observed_at == null ? "尚无" : fmtTime(quota.observed_at)}
      {" · 最近查询："}{quota.checked_at == null ? "尚未查询" : fmtTime(quota.checked_at)}
      {quota.observed_at != null && quota.stale && " · 读数已过期，暂不用于额度排序"}
    </p>
    {quota.issue && <p className="muted">{issues[quota.issue] ?? "额度暂不可查询"}；保留上次有效读数。</p>}
    {quota.history_unavailable && <p className="error">额度已更新，本机历史暂不可写，将自动重试。</p>}
    {(quota.windows ?? []).map(window => <p key={window.id}>
      {window.id} 窗口 · 已用 {window.used_percent.toFixed(1)}%
      {" · "}{window.resets_at == null ? "重置时间未知" : `${fmtTime(window.resets_at)} 重置`}
      {" · "}{window.source === "api" ? "官方额度接口" : "推理响应头"}
      {window.stale && " · 读数已过期"}
    </p>)}
  </div>;
}

function RoutingStatus({ provider }: { provider: ProviderSourceRow }) {
  if (provider.kind !== "subscription" || !Array.isArray(provider.routing_status)) return null;
  const entries = provider.routing_status.filter(entry => entry != null && typeof entry === "object" &&
    !Array.isArray(entry) && typeof entry.model === "string" && typeof entry.status === "string");
  if (!entries.length) return null;
  const blocked = entries.filter(entry => entry.status !== "eligible").length;

  return (
    <section aria-label={`${provider.id} 的路由状态`}>
      <p>路由状态 · {blocked ? `有模型暂不可用 ${blocked}/${entries.length}` : "当前无路由阻塞"}</p>
      <ul aria-label={`${provider.id} 的逐模型路由状态`}>
        {entries.map((entry, index) => {
          const reason = fmtRouteReason(entry.status);
          const retryDate = typeof entry.retry_at === "number" && Number.isFinite(entry.retry_at)
            ? new Date(entry.retry_at * 1000) : null;
          const retryTime = retryDate && Number.isFinite(retryDate.getTime())
            ? retryDate.toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : null;
          return (
            <li key={index}>
              {entry.model} · {entry.status === "eligible" ? "当前无路由阻塞" : typeof reason === "string" ? reason : entry.status}
              {entry.status !== "eligible" && (retryTime != null
                ? ` · ${retryTime} 后可重试` : " · 未提供重试时间")}
            </li>
          );
        })}
      </ul>
      <p className="muted">此为当前路由状态快照，重试时刻不保证恢复。不含请求工具能力、分组筛选、网关开关或实时检测结果。</p>
    </section>
  );
}

// key 设置行内表单:粘贴 → 保存只回布尔回执(key 值永不回显,红线);
// 保存即生效,网关凭据缓存即时失效,无需重启(ui-design §6.2)。
function KeyForm({ p, onDone, setErr, onBusyChange }: { p: ProviderSourceRow; onDone: () => void; setErr: (s: string) => void; onBusyChange: (id: string, busy: boolean) => void }) {
  const [key, setKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  useEffect(()=>{onBusyChange(p.id,saving||confirmClear);},[p.id,saving,confirmClear,onBusyChange]);

  const save = async () => {
    if (!key) return;
    setSaving(true);
    try {
      await postJson("/admin/keys", { id: p.id, key });
      setKey("");
      onDone();
    } catch (e) {
      setErr(`保存 ${p.id} key 失败:${e}`);
    }
    setSaving(false);
  };

  return (
    <>
      <span className="actions">
        <input
          aria-label={`${p.id} 的 API Key`}
          type="password"
          placeholder="粘贴 api key"
          value={key}
          onChange={e => setKey(e.target.value)}
        />{" "}
        <button className="button-primary" onClick={save} disabled={saving || !key}>
          {saving ? "保存中…" : "保存"}
        </button>{" "}
        {p.key_source === "saved" && (
          <button className="ghost button-danger" disabled={saving} onClick={() => setConfirmClear(true)}>
            清除已保存 Key
          </button>
        )}
      </span>
      {p.key_source === "saved" && (
        <div className="muted">{p.id === "openrouter"
          ? "仅清除 aubridge 已保存的 Key；如已配置，之后按环境变量 Key、OpenCode Key 的顺序回退，不删除 OpenCode 登录文件。"
          : "如果启动环境中也配置了 Key，清除后将继续使用环境变量 Key。"}</div>
      )}
      {p.key_source === "environment" && (
        <div className="muted">正在使用启动进程的 KEY_* 环境变量。更改或移除该 Key 后，需重启网关。</div>
      )}
      {p.key_source === "opencode" && (
        <div className="muted">来自 OpenCode（只读）。可在此保存 Key 优先使用；此页不会修改或删除 OpenCode 登录文件。</div>
      )}
      {confirmClear && <Dialog labelledBy="clear-provider-key-title" busy={saving} onClose={() => setConfirmClear(false)} role="alertdialog">
        <div className="modal-header"><h2 id="clear-provider-key-title">清除已保存的 Key？</h2></div>
        <div className="modal-body"><p>仅清除 aubridge 中 {p.id} 的已保存 Key。如有环境变量 Key，网关会继续使用它。{p.id === "openrouter" && "没有环境变量 Key 时，可能继续使用 OpenCode Key；不会删除 OpenCode 登录文件。"}</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={saving} onClick={() => setConfirmClear(false)}>取消</button>
          <button className="button-danger" disabled={saving} onClick={async () => {
            setSaving(true);
            await clearKey(p.id, onDone, setErr);
            setConfirmClear(false);
            setSaving(false);
          }}>{saving ? "清除中…" : "确认清除"}</button>
        </div>
      </Dialog>}
    </>
  );
}

// 清除 key(DELETE /admin/keys 收 JSON body,浏览器 DELETE 带 body
// 走显式 fetch init)。
async function clearKey(id: string, onDone: () => void, setErr: (s: string) => void) {
  try {
    await fetchJson("/admin/keys", {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id }),
    });
    onDone();
  } catch (e) {
    setErr(`清除 ${id} key 失败:${e}`);
  }
}

// 添加 Provider(cc-switch 式卡片流,字段固定,无自由 JSON 编辑器)。
function AddForm({ onDone, onFailed, setErr, onBusyChange }: { onDone: () => void; onFailed: () => void; setErr: (s: string) => void; onBusyChange: (busy:boolean)=>void }) {
  const [id, setId] = useState("");
  const [kind, setKind] = useState<AddProviderInput["kind"] | "">("");
  const [base, setBase] = useState("");
  const [models, setModels] = useState("");
  const [protocol, setProtocol] = useState<AddProviderInput["protocol"]>("chat");
  const [key, setKey] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(()=>{onBusyChange(saving);},[saving,onBusyChange]);
  const save = async () => {
    if (!kind || saving) return;
    const body: AddProviderInput = {
      id,
      kind,
      base,
      models: models.split(",").map(m => m.trim()).filter(Boolean),
      protocol,
      auth: protocol === "anthropic" ? "api-key" : "bearer",
    };
    if (key) body.key = key;
    setSaving(true);
    try {
      const receipt = await postJson<OpenCodeSyncReceipt>("/admin/providers", body);
      reportOpenCodeSync(receipt, setErr);
      setId("");
      setKind("");
      setBase("");
      setModels("");
      setProtocol("chat");
      setKey("");
      onDone();
    } catch (e) {
      setErr(`添加 Provider 失败:${e}`);
      onFailed();
    }
    setSaving(false);
  };

  return (
    <form className="add-form" aria-label="添加模型源" onSubmit={e => { e.preventDefault(); void save(); }}>
      <div className="field-grid">
        <label className="field">源 ID
          <input value={id} disabled={saving} onChange={e => setId(e.target.value)} placeholder="my-glm" />
        </label>
        <label className="field">上游地址
          <input type="url" value={base} disabled={saving} onChange={e => setBase(e.target.value)} placeholder="https://api.example.dev/v1" />
        </label>
        <label className="field">来源类别（你的声明）
          <select aria-label="来源类别" value={kind} disabled={saving} onChange={e => {
            const value = e.target.value;
            setKind(value === "free_tier" || value === "subscription" ? value : "");
          }}>
            <option value="">请选择来源类别</option>
            <option value="free_tier">官方免费档（自行声明）</option>
            <option value="subscription">自有订阅（自行声明）</option>
          </select>
        </label>
        <label className="field">上游协议
          <select value={protocol} disabled={saving} onChange={e => setProtocol(e.target.value === "anthropic" ? "anthropic" : "chat")}>
            <option value="chat">Chat Completions</option>
            <option value="anthropic">Anthropic Messages</option>
          </select>
        </label>
      </div>
      <p className="muted">类别只记录你的声明，不证明免费，也不会自动授权使用订阅。实际可用仍取决于已许可的费用范围、当前模型费用依据及其他可用条件。</p>
      <label className="field">模型 ID
        <input value={models} disabled={saving} onChange={e => setModels(e.target.value)}
          placeholder="逗号分隔，如 glm-4.5-flash, glm-4.6v-flash" />
      </label>
      <label className="field">API Key
        <input type="password" value={key} disabled={saving} autoComplete="off" onChange={e => setKey(e.target.value)} placeholder="可选，保存后立即生效" />
      </label>
      <div className="actions">
        <button className="button-primary" type="submit" disabled={saving || !kind || !id || !base || !models.trim()}>
          {saving ? "保存中…" : "保存模型源"}
        </button>
      </div>
    </form>
  );
}

// 从 cc-switch 导入(勾选即新增,cc-switch-import-design §4.4):
// 候选列表按行勾选,skip 行灰显不可选;提交后逐行报告 added/failed。
function CcSwitchImportDialog({ onClose, onImported, setErr }: {
  onClose: () => void;
  onImported: () => void;
  setErr: (s: string) => void;
}) {
  const [items, setItems] = useState<CcSwitchCandidate[] | null>(null);
  const [loadError, setLoadError] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [importing, setImporting] = useState(false);
  const [result, setResult] = useState<CcSwitchImportResult | null>(null);

  useEffect(() => {
    fetchJson<{ items?: CcSwitchCandidate[] }>("/admin/import/cc-switch", { cache: "no-store" })
      .then(page => setItems(page.items ?? []))
      .catch(e => setLoadError(String(e)));
  }, []);

  const submit = async () => {
    if (!selected.length || importing) return;
    setImporting(true);
    try {
      const receipt = await postJson<CcSwitchImportResult>("/admin/import/cc-switch", { refs: selected });
      setResult(receipt);
      if (receipt.added.length) onImported();
    } catch (e) {
      setErr(`从 cc-switch 导入失败:${e}`);
    } finally {
      setImporting(false);
    }
  };

  const toggle = (reference: string) => {
    setSelected(current => current.includes(reference)
      ? current.filter(ref => ref !== reference)
      : [...current, reference]);
  };

  return (
    <Dialog labelledBy="ccswitch-import-title" busy={importing} onClose={onClose} className="modal-wide">
      <div className="modal-header"><h2 id="ccswitch-import-title">从 cc-switch 导入</h2></div>
      <div className="modal-body">
        {loadError && <p role="alert">读取 cc-switch 配置失败 · {loadError}</p>}
        {items == null && !loadError && <p className="status-loading" role="status">正在读取 cc-switch 配置…</p>}
        {items != null && items.length === 0 && (
          <p className="muted">未找到 cc-switch 配置(需要 ~/.cc-switch/cc-switch.db),或其中没有可导入的第三方模型源。</p>
        )}
        {items != null && items.length > 0 && (
          <>
            <p className="muted">勾选要导入的模型源;官方登录与本地代理行不导入,key 导入后保存在钥匙串。</p>
            <ul className="data-list" aria-label="cc-switch 候选列表">
              {items.map(item => {
                const provider = item.provider;
                return (
                  <li key={item.reference} className={provider ? "" : "muted"}>
                    <label>
                      <input
                        type="checkbox"
                        aria-label={`导入 ${item.name}`}
                        checked={selected.includes(item.reference)}
                        disabled={importing || provider == null || result != null}
                        onChange={() => toggle(item.reference)}
                      />{" "}
                      <strong>{item.name}</strong>
                      <span className="row-meta">
                        {" "}{item.source}
                        {provider ? ` · ${provider.base} · ${provider.protocol === "anthropic" ? "Anthropic Messages" : provider.protocol === "responses" ? "Responses" : "Chat Completions"} · ${provider.models.length} 个模型 → 源 ID ${provider.id}` : null}
                      </span>
                      {provider == null && item.skip && <div className="muted">{item.skip}</div>}
                    </label>
                  </li>
                );
              })}
            </ul>
            {result && (
              <div role="status">
                {result.added.length > 0 && <p>已导入:{result.added.join("、")}</p>}
                {result.failed.map(failure => (
                  <p key={failure.reference} className="alert error" role="alert">导入失败 · {failure.error}</p>
                ))}
                {!result.added.length && !result.failed.length && <p className="muted">没有导入任何源。</p>}
              </div>
            )}
          </>
        )}
      </div>
      <div className="modal-actions">
        <button autoFocus disabled={importing} onClick={onClose}>{result ? "关闭" : "取消"}</button>
        {result == null && (
          <button className="button-primary" disabled={importing || !selected.length} onClick={() => void submit()}>
            {importing ? "导入中…" : `导入 ${selected.length || ""}`}
          </button>
        )}
      </div>
    </Dialog>
  );
}

function ProviderEditor({ provider, onClose, onSaved }: {
  provider: ProviderSourceRow;
  onClose: () => void;
  onSaved: (issue: string | null) => void;
}) {
  const [base, setBase] = useState(provider.base);
  const [text, setText] = useState((provider.models ?? []).join("\n"));
  const [kind, setKind] = useState(provider.kind);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [catalog, setCatalog] = useState<string[] | null>(null);
  const [catalogError, setCatalogError] = useState("");
  const [reading, setReading] = useState(false);
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const catalogRequest = useRef<AbortController | null>(null);
  useEffect(() => () => catalogRequest.current?.abort(), []);
  const address = base.trim();
  const models = text.split(/[\n,]/).map(model => model.trim()).filter(Boolean);
  const matchingModels = (catalog ?? []).filter(model => model.toLocaleLowerCase().includes(search.toLocaleLowerCase()));

  const clearCatalog = () => {
    catalogRequest.current?.abort();
    catalogRequest.current = null;
    setReading(false);
    setCatalog(null);
    setCatalogError("");
    setSearch("");
    setSelected([]);
  };

  const readCatalog = async () => {
    if (saving || address !== provider.base || provider.model_discovery_supported !== true) return;
    clearCatalog();
    const request = new AbortController();
    catalogRequest.current = request;
    setReading(true);
    try {
      const result = await fetchJson<ProviderModelCatalog>(
        `/admin/providers/${encodeURIComponent(provider.id)}/model-catalog`,
        { signal: request.signal, cache: "no-store" },
      );
      if (!request.signal.aborted) setCatalog(result.models);
    } catch (e) {
      if (!request.signal.aborted) setCatalogError(String(e));
    } finally {
      if (!request.signal.aborted) setReading(false);
    }
  };

  const save = async () => {
    if (saving || reading) return;
    setSaving(true);
    setError("");
    try {
      const result = await postJson<{ opencode_issue: string | null }>(
        `/admin/providers/${encodeURIComponent(provider.id)}/models`, {
          models,
          kind,
          base: address === provider.base ? undefined : address,
        },
      );
      onSaved(result.opencode_issue);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog labelledBy="provider-editor-title" busy={saving} onClose={onClose} className="modal-wide">
      <div className="modal-header"><h2 id="provider-editor-title">编辑来源 {provider.id}</h2></div>
      <div className="modal-body">
        <label className="field">上游地址
          <input
            name="base"
            type="url"
            value={base}
            onChange={e => { clearCatalog(); setBase(e.target.value); }}
            aria-describedby="provider-editor-base-hint"
            disabled={saving}
          />
        </label>
        <p className="muted" id="provider-editor-base-hint">只可修改路径，地址须使用相同的协议、主机和端口。</p>
        <label className="field">来源类别（你的声明）
          <select value={kind} onChange={e => setKind(e.target.value)} disabled={saving}>
            <option value="free_tier">官方免费档（自行声明）</option>
            <option value="subscription">自有订阅（自行声明）</option>
          </select>
        </label>
        <p className="muted">类别只记录你的声明，不证明免费，也不会自动授权使用订阅。实际可用仍取决于已许可的费用范围、当前模型费用依据及其他可用条件。</p>
        <label className="field">模型 ID，按行或逗号分隔
          <textarea rows={5} value={text} onChange={e => setText(e.target.value)} autoFocus disabled={saving} />
        </label>
        {provider.model_discovery_supported === true && (
          <div>
            <button type="button" className="ghost" disabled={saving || reading || address !== provider.base}
              onClick={() => void readCatalog()}>{reading ? "读取中…" : "读取来源模型"}</button>{" "}
            {reading && <button type="button" className="ghost" onClick={clearCatalog}>取消读取</button>}
            <p className="muted">读取已保存连接的供应商目录。模型是否有免费额度、支持工具或可推理，需另行确认。</p>
            {address !== provider.base && <p className="muted">请先保存上游地址，再读取来源模型。</p>}
            {reading && <p role="status">正在读取来源模型…</p>}
            {catalogError && <p role="alert">读取来源模型失败 · {catalogError}</p>}
            {catalog && (catalog.length === 0 ? <p role="status">来源模型目录为空。</p> : (
              <>
                <label className="field">搜索来源模型
                  <input aria-label="搜索来源模型" value={search} disabled={saving} onChange={e => setSearch(e.target.value)} />
                </label>
                <p className="muted">匹配 {matchingModels.length} 个，最多显示前 100 个；已选择 {selected.length} 个。</p>
                <ul className="catalog-list" aria-label="来源模型目录">
                  {matchingModels.slice(0, 100).map(model => (
                    <li key={model}>
                      <label><input type="checkbox" aria-label={`选择模型 ${model}`} checked={selected.includes(model)} disabled={saving}
                        onChange={() => setSelected(current => current.includes(model) ? current.filter(id => id !== model) : [...current, model])} /> {model}</label>
                    </li>
                  ))}
                </ul>
                <button type="button" className="ghost" disabled={saving || selected.length === 0} onClick={() => {
                  setText([...new Set([...models, ...selected])].join("\n"));
                  setSelected([]);
                }}>加入模型草稿</button>
              </>
            ))}
          </div>
        )}
        {error && <p className="alert error" role="alert">{error}</p>}
      </div>
      <div className="modal-actions">
        <button disabled={saving} onClick={onClose}>取消</button>
        <button className="button-primary" disabled={saving || reading || !address || models.length === 0} onClick={save}>{saving ? "保存中…" : "保存"}</button>
      </div>
    </Dialog>
  );
}

// Provider 页(ui-design §6.2):行主体 = 名称/类别/模型/key 布尔/
// 服务 agent 数;[+ 添加 Provider] 卡片流;底部"可反代的 Agents"
// 只读区(设置页已独立为最右页签,2026-11-06 用户拍板)。
function OpenRouterCachedModels({setErr,snapshot}:{setErr:(error:string)=>void;snapshot?:OpenRouterCatalog|null}) {
  const [cache,setCache]=useState<import("./api").ModelDirectory|null>(null);
  const [readFailure,setReadFailure]=useState(false);
  const [reading,setReading]=useState(true);
  const [readAttempt,setReadAttempt]=useState(0);
  useEffect(()=>{
    const request=new AbortController();
    setReading(true);setReadFailure(false);
    fetchJson<import("./api").ModelDirectory>("/admin/providers/openrouter/catalog",{signal:request.signal})
      .then(value=>{if(!request.signal.aborted)setCache(value);})
      .catch(error=>{if(!request.signal.aborted){setReadFailure(true);setErr(`读取OpenRouter缓存目录失败：${error}`);}})
      .finally(()=>{if(!request.signal.aborted)setReading(false);});
    return()=>request.abort();
  },[readAttempt]);
  const currentSnapshot=snapshot && snapshot.fetched_at!==null && snapshot.fetched_at>=(cache?.fetched_at??0) ? snapshot : null;
  const readStatus=readFailure ? <p className="error" role="alert">缓存目录读取失败，当前状态未确认。<button disabled={reading} onClick={()=>setReadAttempt(current=>current+1)}>重试只读缓存</button></p> : null;
  if(currentSnapshot)return <>
    {readStatus}
    <p>获取于 {fmtTime(currentSnapshot.fetched_at!)}{currentSnapshot.stale ? " · 已过期" : ""}{currentSnapshot.issue ? " · 最近读取失败，保留上次结果" : ""}</p>
    <ul className="catalog-list">{currentSnapshot.models.map(model=><li key={model.id}>{model.id} · {model.tools?"支持工具":"工具能力未确认"}{model.context ? ` · ${model.context.toLocaleString()} token 上下文` : ""}</li>)}</ul>
    {!currentSnapshot.models.length && <p>缓存中暂无模型。</p>}
  </>;
  return <>{readStatus}{!cache ? !readFailure && <p role="status">正在读取缓存目录…</p> : <>
    <p>{cache.fetched_at ? `获取于 ${fmtTime(cache.fetched_at)}` : "尚无完整目录"}{cache.stale ? " · 已过期" : ""}{cache.issue ? " · 最近读取失败，保留上次结果" : ""}</p>
    <ul className="catalog-list">{cache.models.map(model=><li key={model.id}>{model.id}<ModelFactsText facts={model.facts} /></li>)}</ul>
    {!cache.models.length && <p>缓存中暂无模型。</p>}
  </>}</>;
}

export function Providers({ setErr, onReturn, returnLabel, onBusyChange }: {
  setErr: (s: string) => void; onReturn?: () => void; returnLabel?: string; onBusyChange?: (busy: boolean) => void;
}) {
  const [data, setData] = useState<ProvidersPage | null>(null);
  const [agents, setAgents] = useState<AgentsPage | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [agentsFailed, setAgentsFailed] = useState(false);
  const [adding, setAdding] = useState(false);
  const [addVisited,setAddVisited]=useState(false);
  const [addBusy,setAddBusy]=useState(false);
  const [costBusy, setCostBusy] = useState(false);
  const returnTrigger = useRef<HTMLButtonElement>(null);
  const taskTrigger=useRef<HTMLButtonElement|null>(null);
  const [checks, setChecks] = useState<Record<string, ProviderCheck | "checking">>({});
  const checkRequests = useRef(new Map<string, symbol>());
  const providerConnections=useRef(new Map<string,string>());
  const [checkModels, setCheckModels] = useState<Record<string, string>>({});
  const [refreshing, setRefreshing] = useState(false);
  const [routingPending, setRoutingPending] = useState(false);
  const [editingProvider, setEditingProvider] = useState<ProviderSourceRow | null>(null);
  const [deleteProvider, setDeleteProvider] = useState<ProviderSourceRow | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [ccImporting, setCcImporting] = useState(false);
  // 首次进入额度任务才读取历史，返回保留同一组件。
  const [quotaVisited, setQuotaVisited] = useState<Record<string, boolean>>({});
  const [directoryVisited, setDirectoryVisited] = useState<Record<string, boolean>>({});
  const [managed, setManaged] = useState<Record<string, boolean>>({});
  const [activeProvider, setActiveProvider] = useState<string | null>(null);
  const [purpose, setPurpose] = useState("manage");
  const [catalogVisible, setCatalogVisible] = useState(false);
  const [catalogVisited, setCatalogVisited] = useState(false);
  const [providerBusy,setProviderBusy]=useState<Record<string,boolean>>({});
  const [directoryBusy,setDirectoryBusy]=useState<Record<string,boolean>>({});
  const keyActivity=useCallback((id:string,busy:boolean)=>setProviderBusy(current=>current[id]===busy?current:{...current,[id]:busy}),[]);
  const directoryActivity=useCallback((id:string,busy:boolean)=>setDirectoryBusy(current=>current[id]===busy?current:{...current,[id]:busy}),[]);

  const providerRevision = useRef(0);
  const loadProviders = () => {
    const revision = ++providerRevision.current;
    return fetchJson<ProvidersPage>(onReturn ? "/admin/providers?sample=0" : "/admin/providers").then(value => {
      if (revision !== providerRevision.current) return;
      const changed:string[]=[];
      for(const provider of value.providers??[]) {
        const identity=provider.source_health?.cost.connection_identity??provider.base;
        const previous=providerConnections.current.get(provider.id);
        if(previous!==undefined && previous!==identity) {changed.push(provider.id);checkRequests.current.delete(provider.id);}
        providerConnections.current.set(provider.id,identity);
      }
      if(changed.length)setChecks(current=>Object.fromEntries(Object.entries(current).filter(([id])=>!changed.includes(id))));
      setData(value);
      setLoadFailed(false);
    }).catch(error => {
      if (revision === providerRevision.current) throw error;
    });
  };
  const load = () => {
    loadProviders().catch(e => { setLoadFailed(true); setErr(String(e)); });
    fetchJson<AgentsPage>("/admin/agents")
      .then(value => { setAgents(value); setAgentsFailed(false); })
      .catch(e => { setAgentsFailed(true); setErr(String(e)); });
  };
  useEffect(load, [setErr]);
  useEffect(() => {
    if (onReturn) return;
    const timer = window.setInterval(() => {
      loadProviders().catch(error => { setLoadFailed(true); setErr(String(error)); });
    }, 30_000);
    return () => { window.clearInterval(timer); providerRevision.current += 1; };
  }, [setErr, Boolean(onReturn)]);
  useEffect(() => {
    const requests = checkRequests.current;
    return () => requests.clear();
  }, []);

  useEffect(()=>{
    if(data && activeProvider && !data.providers?.some(provider=>provider.id===activeProvider))setActiveProvider(null);
  },[data,activeProvider]);

  const navigationLocked = costBusy || addBusy || refreshing || routingPending || deleting || Boolean(ccImporting || deleteProvider || editingProvider) || Object.values(providerBusy).some(Boolean) || Object.values(directoryBusy).some(Boolean) || Object.values(checks).some(value => value === "checking");
  useEffect(() => { onBusyChange?.(navigationLocked); return () => onBusyChange?.(false); }, [navigationLocked, onBusyChange]);
  const hasReturn = Boolean(onReturn);
  const hasData = Boolean(data);
  useEffect(() => { if (hasReturn) returnTrigger.current?.focus(); }, [hasReturn, hasData]);
  const returnControl = onReturn && <div className="guide setup-guide">
    <p>这里只处理模型来源与费用前提。返回后会重新读取原工具条件，不会自动保存或运行任务。</p>
    <button ref={returnTrigger} disabled={navigationLocked} onClick={() => { if (!navigationLocked) onReturn(); }}>{returnLabel || "返回原工具任务"}</button>
  </div>;
  if (!data) return <>{returnControl}<p className="status-loading" role="status">{loadFailed ? "模型源暂不可读取。" : "正在读取模型源…"}{loadFailed && <button className="ghost" onClick={load}>重试</button>}</p></>;

  const rows = data.providers || [];
  const directory = data.openrouter_catalog;
  const detected = (agents?.agents || []).filter(a => supportsWiring(a.id) && a.detected);
  const wiredCount = detected.filter(a => a.wired).length;

  const invalidateCheck = (id: string) => {
    checkRequests.current.delete(id);
    setChecks(current => {
      const next = { ...current };
      delete next[id];
      return next;
    });
  };

  const remove = async (p: ProviderSourceRow) => {
    if (deleting) return;
    setDeleting(true);
    try {
      const receipt = await deleteJson<OpenCodeSyncReceipt>(`/admin/providers/${p.id}`);
      invalidateCheck(p.id);
      reportOpenCodeSync(receipt, setErr);
      setDeleteProvider(null);
      setActiveProvider(current=>current===p.id?null:current);
      load();
    } catch (e) {
      setErr(`删除 ${p.id} 失败:${e}`);
      setDeleteProvider(null);
      load();
    } finally {
      setDeleting(false);
    }
  };

  const check = async (p: ProviderSourceRow, model: string | undefined) => {
    if (!model) return;
    const request = Symbol();
    checkRequests.current.set(p.id, request);
    setChecks(current => ({ ...current, [p.id]: "checking" }));
    try {
      const result = await postJson<ProviderCheck>(`/admin/providers/${p.id}/check?${new URLSearchParams({model})}`, {});
      if (checkRequests.current.get(p.id) !== request) return;
      checkRequests.current.delete(p.id);
      setChecks(current => ({ ...current, [p.id]: result }));
    } catch (e) {
      if (checkRequests.current.get(p.id) !== request) return;
      invalidateCheck(p.id);
      setErr(`检测 ${p.id} 失败 · ${readableActionError(e)}`);
    }
  };

  const updateRouting = async (p: ProviderSourceRow, action: "pause" | "resume-routing") => {
    setRoutingPending(true);
    try {
      const body = action === "pause" ? { paused: !p.paused } : {};
      await postJson(`/admin/providers/${encodeURIComponent(p.id)}/${action}`, body);
      await loadProviders();
    } catch (e) {
      setErr(`更新 ${p.id} 的路由状态失败 · ${e}`);
    } finally {
      setRoutingPending(false);
    }
  };

  const refreshDirectory = async () => {
    setRefreshing(true);
    try {
      const result = await postJson<OpenRouterCatalog>("/admin/providers/openrouter/catalog", {});
      reportOpenCodeSync(result, setErr);
      setData(current => current && { ...current, openrouter_catalog: result });
      load();
    } catch (e) {
      setErr(`刷新 OpenRouter 目录失败 · ${e}`);
    } finally {
      setRefreshing(false);
    }
  };

  return (
    <div className="providers-page management-page">
      {returnControl}
      <div className="task-workspace-pane" hidden={activeProvider!==null || catalogVisible || adding}>
      <div className="page-heading">
        <div>
          <h2 className="page-title">模型源</h2>
          <p className="page-description">明确费用范围后，按来源事实管理模型与凭据</p>
        </div>
        <span className="actions">
          <button className="ghost" onClick={() => setCcImporting(true)}>从 cc-switch 导入</button>
          <button className="button-primary" onClick={event=>{taskTrigger.current=event.currentTarget;setAddVisited(true);setAdding(true);}}>
            添加模型源
          </button>
        </span>
      </div>
      <CostIntentPanel providers={rows} onSaved={load} onBusyChange={setCostBusy} />
      <div className="data-list" aria-label="模型源列表">
        {rows.map(p => {
          const result = checks[p.id];
          const models = p.models ?? [];
          const checkModel = models.find(model => model === checkModels[p.id]) ?? models[0];
          return (
            <div className="data-row provider-row" key={p.id}>
              <div className="provider-summary">
                <span className="row-main">
                  <span className="row-title">
                    <strong>{p.id}</strong>
                    <span className={`badge ${p.kind === "subscription" ? "subscription" : "off"}`}>{p.kind === "subscription" ? p.source === "custom" ? "声明订阅" : "自有订阅" : p.kind === "free_tier" ? p.source === "custom" ? "声明免费档" : "免费档" : "类别未知"}</span>
                    {p.source === "custom" && <span className="badge off">自定义</span>}
                    {p.id === "claude-cli" && p.paused ? (
                      <span className="badge off">来源未启用</span>
                    ) : p.source_health?.credential.required === false ? (
                      <span className="badge off">无需凭据</span>
                    ) : p.store_key && p.key_source === "saved" ? (
                      <span className="badge success">已保存 Key</span>
                    ) : p.store_key && p.key_source === "environment" ? (
                      <span className="badge success">环境变量 Key</span>
                    ) : p.key_source === "opencode" ? (
                      <span className="badge success">来自 OpenCode（只读）</span>
                    ) : p.key_set ? (
                      <span className="badge success">凭据已设置</span>
                    ) : p.ready ? (
                      <span className="badge success">凭据已就绪</span>
                    ) : (
                      <span className="badge off">凭据未设置</span>
                    )}
                  </span>
                  <span className="row-meta">
                    {models.length} 个模型
                    {(p.util_5h != null || p.util_7d != null) && (
                      <span title="各窗口已用百分比；在额度历史查看重置时间与读数来源">
                        用量 {p.quota?.windows?.length ? p.quota.windows.map(window => `${window.id} ${window.used_percent.toFixed(0)}%${window.stale ? "（旧）" : ""}`).join(" / ") : fmtUtil(p)}
                        {!p.quota?.windows?.length && p.resets_at != null && ` · ${fmtTime(p.resets_at)} 重置`}
                      </span>
                    )}
                    {p.id === "claude-cli" && <span>本机官方 Claude CLI · 待实机验证 · 套餐与额度未知</span>}
                    {p.id === "chatgpt" && <span>登录来源：{p.login_source === "codex" ? "Codex" : p.login_source === "opencode" ? "OpenCode" : "未知"}</span>}
                    {p.paused && <span className="badge warn">推理已暂停</span>}
                    {p.quota?.issue && <span className="badge warn">额度查询暂不可用</span>}
                    {p.free_requests?.some(entry=>entry.status!=="eligible") && <span className="badge warn">免费请求受限 · {fmtRouteReason(p.free_requests.find(entry=>entry.status!=="eligible")!.status)}</span>}
                    {p.routing_disabled && <span className="badge bad">因上游限制已停用</span>}
                    {Array.isArray(p.routing_status) && p.routing_status.some(entry => entry && typeof entry.status === "string" && entry.status !== "eligible") && <span className="badge warn">有模型暂不可用</span>}
                    {result && result !== "checking" && result.model === checkModel && <span>最近检测：{fmtRouteReason(result.status)}</span>}
                  </span>
                </span>
                <span className="row-actions"><button onClick={event => {taskTrigger.current=event.currentTarget;setManaged(current=>({...current,[p.id]:true}));setActiveProvider(p.id);setPurpose("manage");}}>管理</button></span>
              </div>
            </div>
          );
        })}
        {!rows.length && <div className="empty-state">
          <p>还没有模型源，请先准备你有权使用的外部来源。</p>
          <p>已有受支持的 ChatGPT 登录：到“设置 → 登录与连接”核对 Codex 或 OpenCode 登录来源。</p>
          <p>已有 API Key：添加模型源，填写厂商提供的地址、协议与模型。</p>
          <p>两者都没有：先向厂商准备可用账户或 Key；本应用不附送账号或额度。</p>
          <p>准备后仍需可运行的工具、符合条件的模型和明确的费用许可；添加来源不代表已可调用。</p>
        </div>}
      </div>
      </div>
      {addVisited && <div className="task-workspace-pane" hidden={!adding}><TaskView title="添加模型源" busy={addBusy} onBack={()=>{setAdding(false);window.requestAnimationFrame(()=>taskTrigger.current?.focus());}}>
        <AddForm onBusyChange={setAddBusy}
          onDone={() => { setAdding(false);setAddVisited(false);setAddBusy(false); load(); }}
          onFailed={() => {
            // 读取失败时保留原新增错误。
            void loadProviders().catch(() => {});
          }}
          setErr={setErr}
        />
      </TaskView></div>}
      {rows.map(p => {
        if(!managed[p.id])return null;
        const result=checks[p.id];
        const models=p.models??[];
        const checkModel=models.find(model=>model===checkModels[p.id])??models[0];
        return <div className="task-workspace-pane" hidden={activeProvider!==p.id}
        key={`${p.id}:${p.source_health?.cost.connection_identity ?? p.base}`}>
        <TaskView title={`管理模型源 · ${p.id}`} onBack={() => {setActiveProvider(null);window.requestAnimationFrame(()=>taskTrigger.current?.focus());}}
          busy={routingPending || checks[p.id]==="checking" || deleting || providerBusy[p.id] || directoryBusy[p.id]}>
          <nav className="task-workspace-nav" aria-label={`${p.id} 的管理用途`}>
            {[["manage","连接与凭据"],["quota","额度历史"],["directory","厂商目录"],["routing","模型与路由"],["free","免费请求状态"]].filter(([id])=>id!=="free" || !!p.free_requests?.length).map(([id,label])=><button key={id} aria-pressed={purpose===id}
              disabled={routingPending || checks[p.id]==="checking" || deleting || providerBusy[p.id] || directoryBusy[p.id]}
              onClick={()=>{setPurpose(id);if(id==="quota")setQuotaVisited(current=>({...current,[p.id]:true}));if(id==="directory")setDirectoryVisited(current=>({...current,[p.id]:true}));}}>{label}</button>)}
          </nav>
          <div className="task-workspace-pane" hidden={purpose!=="manage"}>
              <div className="detail-panel provider-sections">
                <SourceHealthFacts provider={p} />
                <section>
                  <h3 className="section-label">凭据</h3>
                  {p.store_key ? <KeyForm p={p} onBusyChange={keyActivity} onDone={() => { invalidateCheck(p.id); load(); }} setErr={setErr} />
                    : <p className="muted">{p.id === "claude-cli" && p.paused ? "来源未启用，启用后读取本机官方 CLI 登录；真实验收待完成。" : p.ready ? "登录凭据已就绪。" : "登录凭据尚未就绪。"}</p>}
                  {p.id === "chatgpt" && <p className="muted">
                    {p.login_source === "opencode" ? "只读取 OpenCode 的当前登录，自动更新仅保存在本应用内。" : p.login_source === "codex" ? "跟随 Codex 的当前登录。" : "登录来源未知，请在设置中确认。"}
                    {" "}可在设置中更改登录来源，不会自动切换到其他来源。
                  </p>}
                </section>
                <section>
                  <h3 className="section-label">连接检测</h3>
                  <div className="actions">
                    {models.length > 1 && <label className="field">检测模型
                      <select className="provider-check-model" aria-label={`${p.id} 的检测模型`} value={checkModel}
                        disabled={result === "checking"} onChange={e => {
                          const model = e.target.value;
                          setCheckModels(current => ({ ...current, [p.id]: model }));
                        }}>
                        {models.map(model => <option key={model} value={model}>{model}</option>)}
                      </select>
                    </label>}
                    <button disabled={!(p.ready || p.source_health?.credential.required === false && p.source_health.credential.usable) || !checkModel || result === "checking"} onClick={() => check(p, checkModel)}>
                      {result === "checking" ? "检测中…" : "检测连接"}
                    </button>
                  </div>
                  <p className="muted">向选中模型发送一条短请求，可能使用额度或产生费用。此显式检测独立于自动推理的费用范围，不会修改费用选择。</p>
                  {result && result !== "checking" && result.model === checkModel && <p role="status">
                    {fmtRouteReason(result.status)}
                    {result.model && ` · ${result.model}`}
                    {result.upstream_status != null && ` · HTTP ${result.upstream_status}`}
                  </p>}
                </section>
                <div className="actions">
                  <button disabled={routingPending} onClick={() => updateRouting(p, "pause")}>{p.id === "claude-cli" ? p.paused ? "启用 Claude CLI（待实机验证）" : "停用" : p.paused ? "恢复推理" : "暂停推理"}</button>
                  {p.routing_disabled && <button disabled={routingPending} onClick={() => updateRouting(p, "resume-routing")}
                    title="确认上游限制已解除后，允许该源再次参与路由">重新允许路由</button>}
                  {p.source === "custom" && <>
                    <button onClick={() => setEditingProvider(p)}>编辑来源</button>
                    <button className="ghost button-danger" onClick={() => setDeleteProvider(p)}>删除来源</button>
                  </>}
                </div>
              </div>
          </div>
          <div className="task-workspace-pane" hidden={purpose!=="quota"}>
                  {(p.util_5h != null || p.util_7d != null) ? (
                    <p className="muted">
                      5 小时 / 7 天窗口已用:{fmtUtil(p)}
                      {p.resets_at != null && ` · ${fmtTime(p.resets_at)} 重置`}
                    </p>
                  ) : (
                    <p className="muted">尚无窗口读数（被动采样与主动查询都会补上）。</p>
                  )}
                  <QuotaStatus quota={p.quota} />
                  {quotaVisited[p.id] && <QuotaCurve key={`${p.id}:${p.quota?.revision ?? 0}`} provider={p.id} setErr={setErr} />}

          </div>
          <div className="task-workspace-pane" hidden={purpose!=="directory"}>
            {directoryVisited[p.id] && <ModelDirectory provider={p} onDone={loadProviders} setErr={setErr} onBusyChange={directoryActivity} />}
          </div>
          <div className="task-workspace-pane" hidden={purpose!=="routing"}>
            {models.length ? <ul className="model-list">{models.map(model=><li key={model}>{model}
              {p.model_availability?.[model]===false && <strong className="error"> · 厂商当前目录未列出</strong>}
              <ModelFactsText facts={p.model_facts?.[model]} /></li>)}</ul> : <p>此源没有模型。</p>}
            <RoutingStatus provider={p} />
          </div>
          <div className="task-workspace-pane" hidden={purpose!=="free"}>
            {!p.free_requests?.length && <p>当前没有免费请求状态记录。</p>}
            <ul>{p.free_requests?.map(entry=><li key={entry.model}>{entry.model} · {fmtRouteReason(entry.status)}{entry.retry_at ? ` · ${fmtTime(entry.retry_at)} 可重试` : ""}</li>)}</ul>
          </div>
        </TaskView>
      </div>;})}
      {(directory || rows.some(p => p.id === "openrouter")) && <section className="panel" hidden={activeProvider!==null || catalogVisible || adding}>
        <div className="panel-header">
          <h3>OpenRouter 免费模型</h3>
          <button disabled={refreshing} onClick={refreshDirectory}>{refreshing ? "刷新中…" : "刷新目录"}</button>
        </div>
        <div className="panel-body">
          {directory ? <>
          <p className="muted" role="status">
            {directory.fetched_at ? `更新于 ${fmtTime(directory.fetched_at)}` : "使用内置清单"}
            {!!directory.fetched_at && !directory.models.length && " · 暂无免费文本模型"}
            {directory.stale && " · 沿用上次有效目录"}
            {directory.issue && ` · ${CATALOG_ISSUES[directory.issue]}`}
          </p>
          {!!directory.models.length && <button onClick={event=>{taskTrigger.current=event.currentTarget;setCatalogVisited(true);setCatalogVisible(true);}}>浏览 {directory.models.length} 个缓存模型</button>}
          </> : <p className="muted">目录状态未知。</p>}
          {rows.some(p => p.id === "openrouter") && <p className="muted">Key 预算（USD） · {data.openrouter_key_budget
            ? data.openrouter_key_budget.limit == null ? "未设上限" : `剩余 ${data.openrouter_key_budget.limit_remaining ?? "未知"} / ${data.openrouter_key_budget.limit}`
            : "未知"}</p>}
        </div>
      </section>}
      {catalogVisited && <div className="task-workspace-pane" hidden={!catalogVisible}><TaskView title="OpenRouter 缓存目录" onBack={()=>{setCatalogVisible(false);window.requestAnimationFrame(()=>taskTrigger.current?.focus());}} busy={refreshing}>
        <p className="muted">仅浏览本地已缓存目录。刷新厂商目录需要返回后明确点击刷新。</p>
        <OpenRouterCachedModels setErr={setErr} snapshot={directory} />
      </TaskView></div>}
      {ccImporting && <CcSwitchImportDialog
        onClose={() => setCcImporting(false)}
        onImported={load}
        setErr={setErr}
      />}
      {deleteProvider && <Dialog labelledBy="delete-provider-title" role="alertdialog" busy={deleting} onClose={() => setDeleteProvider(null)}>
        <div className="modal-header"><h2 id="delete-provider-title">删除模型源？</h2></div>
        <div className="modal-body"><p>{deleteProvider.id} 将不再参与路由，使用该源的模型可能受到影响。</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={deleting} onClick={() => setDeleteProvider(null)}>取消</button>
          <button className="button-danger" disabled={deleting} onClick={() => void remove(deleteProvider)}>{deleting ? "删除中…" : "确认删除"}</button>
        </div>
      </Dialog>}
      {editingProvider && <ProviderEditor provider={editingProvider} onClose={() => setEditingProvider(null)} onSaved={issue => {
        invalidateCheck(editingProvider.id);
        setEditingProvider(null);
        reportOpenCodeSync({ opencode_issue: issue }, setErr);
        load();
      }} />}

      <section className="panel" hidden={activeProvider!==null || catalogVisible || adding}>
      <div className="panel-header"><h3>已检测的 Agents</h3></div>
      <p className="panel-body muted">
        {agentsFailed ? "Agent 配置暂不可读取。" : !agents ? "正在读取 Agent 配置…" : detected.length
          ? detected.map(a => a.name).join(" · ") + `（已接线 ${wiredCount} 个）`
          : "未检测到 Agent 配置（仅检查配置文件，不扫描安装）"}
      </p>
      </section>
    </div>
  );
}

// Explicit request inspection: current local snapshot only, never the sampled source page.
export function SourceInspection({ providerId, onBack, requestFacts }: {
  providerId: string; onBack: () => void; requestFacts?: ReactNode;
}) {
  const [provider, setProvider] = useState<ProviderSourceRow | null>(null);
  const [state, setState] = useState<"reading" | "ready" | "missing" | "failed">("reading");
  const [revision, setRevision] = useState(0);
  const [readProviderId, setReadProviderId] = useState<string | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    setProvider(null); setState("reading");
    fetchJson<ProvidersPage>("/admin/providers?sample=0", { signal: controller.signal, cache: "no-store" })
      .then(page => {
        if (controller.signal.aborted) return;
        if (!Array.isArray(page.providers)) throw new Error("invalid_source_snapshot");
        const found = page.providers.find(row => row.id === providerId) || null;
        setReadProviderId(providerId); setProvider(found); setState(found ? "ready" : "missing");
      })
      .catch(() => { if (!controller.signal.aborted) { setReadProviderId(providerId); setState("failed"); } });
    return () => controller.abort();
  }, [providerId, revision]);
  return <TaskView title={`来源快照 · ${providerId}`} backLabel="返回原请求" onBack={onBack}>
    <p className="muted">上方请求事实与当前来源快照分开记录。这里只读取本机状态，不探测额度、刷新目录或调用模型。</p>
    {requestFacts}
    {(state === "reading" || readProviderId !== providerId) && <p role="status">正在读取当前来源快照…</p>}
    {readProviderId === providerId && state === "failed" && <p className="alert error" role="alert">来源快照暂不可读，不能据此判断当前可用性。</p>}
    {readProviderId === providerId && state === "missing" && <p role="status">当前已找不到此来源；原请求记录仍保留当时事实。</p>}
    {state === "ready" && readProviderId === providerId && provider?.id === providerId && <section>
      <h3>当前本机来源状态</h3>
      <dl className="task-facts">
        <div><dt>来源</dt><dd>{provider.id}</dd></div>
        <div><dt>{provider.source === "custom" ? "类别声明" : "类别"}</dt><dd>{provider.kind === "subscription" ? provider.source === "custom" ? "声明订阅" : "订阅" : provider.kind === "free_tier" ? provider.source === "custom" ? "声明免费档" : "免费档类别" : "未知"}</dd></div>
        <div><dt>路由</dt><dd>{provider.paused ? "已暂停" : provider.routing_disabled ? "因上游限制已停用" : "未暂停；仍受模型、费用与当前条件限制"}</dd></div>
        <div><dt>凭据</dt><dd>{provider.source_health?.credential.required === false ? "此本地来源无需凭据" : provider.source_health?.credential.required === true ? provider.source_health.credential.usable === true ? "当前本机凭据可读取，不代表本次推理成功" : provider.source_health.credential.usable === false ? "当前本机凭据不可用" : "尚未确认" : "尚未确认"}</dd></div>
        <div><dt>费用依据</dt><dd>{provider.source_health?.cost.basis === "confirmed_free" ? "此来源至少一个模型有当前确认免费依据；本次请求按其记录核对" : provider.source_health?.cost.basis === "authorized_subscription" ? "订阅费用依据" : "尚未确认"}；订阅使用授权{provider.source_health?.cost.authorized === true ? "已确认" : provider.source_health?.cost.authorized === false ? "未确认" : "状态未知"}，仍受当前费用规则约束</dd></div>
      </dl>
    </section>}
    <button className="ghost" disabled={state === "reading"} onClick={() => setRevision(value => value + 1)}>重新读取本机快照</button>
  </TaskView>;
}

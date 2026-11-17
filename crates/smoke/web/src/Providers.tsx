import { useEffect, useRef, useState } from "react";
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
  supportsWiring,
} from "./api";
import { fmtRouteReason, fmtTime, fmtUtil } from "./format";
import { Dialog } from "./Dialog";

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
        <QuotaWindowLine key={item.window} item={item} />
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

function RoutingStatus({ provider }: { provider: ProviderSourceRow }) {
  if (provider.kind !== "subscription" || !Array.isArray(provider.routing_status)) return null;
  const entries = provider.routing_status.filter(entry => entry != null && typeof entry === "object" &&
    !Array.isArray(entry) && typeof entry.model === "string" && typeof entry.status === "string");
  if (!entries.length) return null;
  const blocked = entries.filter(entry => entry.status !== "eligible").length;

  return (
    <details className="section-details" aria-label={`${provider.id} 的路由状态`}>
      <summary>路由状态 · {blocked ? `有模型暂不可用 ${blocked}/${entries.length}` : "当前无路由阻塞"}</summary>
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
    </details>
  );
}

// key 设置行内表单:粘贴 → 保存只回布尔回执(key 值永不回显,红线);
// 保存即生效,网关凭据缓存即时失效,无需重启(ui-design §6.2)。
function KeyForm({ p, onDone, setErr }: { p: ProviderSourceRow; onDone: () => void; setErr: (s: string) => void }) {
  const [key, setKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);

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
function AddForm({ onDone, onFailed, setErr }: { onDone: () => void; onFailed: () => void; setErr: (s: string) => void }) {
  const [id, setId] = useState("");
  const [kind, setKind] = useState<AddProviderInput["kind"] | "">("");
  const [base, setBase] = useState("");
  const [models, setModels] = useState("");
  const [protocol, setProtocol] = useState<AddProviderInput["protocol"]>("chat");
  const [key, setKey] = useState("");
  const [saving, setSaving] = useState(false);

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
        <label className="field">来源类别
          <select aria-label="来源类别" value={kind} disabled={saving} onChange={e => {
            const value = e.target.value;
            setKind(value === "free_tier" || value === "subscription" ? value : "");
          }}>
            <option value="">请选择来源类别</option>
            <option value="free_tier">官方免费档</option>
            <option value="subscription">自有订阅（Coding Plan等）</option>
          </select>
        </label>
        <label className="field">上游协议
          <select value={protocol} disabled={saving} onChange={e => setProtocol(e.target.value === "anthropic" ? "anthropic" : "chat")}>
            <option value="chat">Chat Completions</option>
            <option value="anthropic">Anthropic Messages</option>
          </select>
        </label>
      </div>
      <p className="muted">免费档可用于仅免费档分组；自有订阅作为订阅源参与路由。</p>
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
        <label className="field">类别
          <select value={kind} onChange={e => setKind(e.target.value)} disabled={saving}>
            <option value="free_tier">免费档</option>
            <option value="subscription">订阅</option>
          </select>
        </label>
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
export function Providers({ setErr }: { setErr: (s: string) => void }) {
  const [data, setData] = useState<ProvidersPage | null>(null);
  const [agents, setAgents] = useState<AgentsPage | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [agentsFailed, setAgentsFailed] = useState(false);
  const [adding, setAdding] = useState(false);
  const [checks, setChecks] = useState<Record<string, ProviderCheck | "checking">>({});
  const checkRequests = useRef(new Map<string, symbol>());
  const [checkModels, setCheckModels] = useState<Record<string, string>>({});
  const [refreshing, setRefreshing] = useState(false);
  const [routingPending, setRoutingPending] = useState(false);
  const [editingProvider, setEditingProvider] = useState<ProviderSourceRow | null>(null);
  const [deleteProvider, setDeleteProvider] = useState<ProviderSourceRow | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [ccImporting, setCcImporting] = useState(false);
  // 展开过即拉一次窗口历史(曲线懒挂载)。
  const [quotaExpanded, setQuotaExpanded] = useState<Record<string, boolean>>({});

  const loadProviders = () => fetchJson<ProvidersPage>("/admin/providers").then(value => { setData(value); setLoadFailed(false); });
  const load = () => {
    loadProviders().catch(e => { setLoadFailed(true); setErr(String(e)); });
    fetchJson<AgentsPage>("/admin/agents")
      .then(value => { setAgents(value); setAgentsFailed(false); })
      .catch(e => { setAgentsFailed(true); setErr(String(e)); });
  };
  useEffect(load, [setErr]);
  useEffect(() => {
    const requests = checkRequests.current;
    return () => requests.clear();
  }, []);

  if (!data) return <p className="status-loading" role="status">{loadFailed ? "模型源暂不可读取。" : "正在读取模型源…"}{loadFailed && <button className="ghost" onClick={load}>重试</button>}</p>;

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
      setErr(`检测 ${p.id} 失败 · ${e}`);
    }
  };

  const updateRouting = async (p: ProviderSourceRow, action: "pause" | "resume-routing") => {
    setRoutingPending(true);
    try {
      const body = action === "pause" ? { paused: !p.paused } : {};
      await postJson(`/admin/providers/${encodeURIComponent(p.id)}/${action}`, body);
      setData(await fetchJson<ProvidersPage>("/admin/providers"));
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
      <div className="page-heading">
        <div>
          <h2 className="page-title">模型源</h2>
          <p className="page-description">官方免费档优先，自有订阅兜底</p>
        </div>
        <span className="actions">
          <button className="ghost" onClick={() => setCcImporting(true)}>从 cc-switch 导入</button>
          <button className={adding ? "button-secondary" : "button-primary"} onClick={() => setAdding(!adding)}>
            {adding ? "取消添加" : "添加模型源"}
          </button>
        </span>
      </div>
      {adding && <section className="panel">
        <div className="panel-header"><h3>添加模型源</h3></div>
        <AddForm
          onDone={() => { setAdding(false); load(); }}
          onFailed={() => {
            // 读取失败时保留原新增错误。
            void loadProviders().catch(() => {});
          }}
          setErr={setErr}
        />
      </section>}
      <div className="data-list" aria-label="模型源列表">
        {rows.map(p => {
          const result = checks[p.id];
          const models = p.models ?? [];
          const checkModel = models.find(model => model === checkModels[p.id]) ?? models[0];
          return (
            <details
              className="data-row provider-row"
              key={p.id}
              onToggle={e => {
                if (!(e.target as HTMLDetailsElement).open) return;
                setQuotaExpanded(current =>
                  current[p.id] ? current : { ...current, [p.id]: true },
                );
              }}
            >
              <summary className="provider-summary">
                <span className="row-main">
                  <span className="row-title">
                    <strong>{p.id}</strong>
                    <span className={`badge ${p.kind === "subscription" ? "subscription" : "off"}`}>{p.kind === "subscription" ? "自有订阅" : p.kind === "free_tier" ? "免费档" : "类别未知"}</span>
                    {p.source === "custom" && <span className="badge off">自定义</span>}
                    {p.store_key && p.key_source === "saved" ? (
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
                      <span title="5 小时 / 7 天窗口已用百分比（被动采样与主动查询合并）">
                        用量 {fmtUtil(p)}
                        {p.resets_at != null && ` · ${fmtTime(p.resets_at)} 重置`}
                      </span>
                    )}
                    {p.id === "chatgpt" && <span>登录来源：{p.login_source === "codex" ? "Codex" : p.login_source === "opencode" ? "OpenCode" : "未知"}</span>}
                    {p.paused && <span className="badge warn">推理已暂停</span>}
                    {p.routing_disabled && <span className="badge bad">因上游限制已停用</span>}
                    {Array.isArray(p.routing_status) && p.routing_status.some(entry => entry && typeof entry.status === "string" && entry.status !== "eligible") && <span className="badge warn">有模型暂不可用</span>}
                    {result && result !== "checking" && result.model === checkModel && <span>最近检测：{fmtRouteReason(result.status)}</span>}
                  </span>
                </span>
                <span className="row-actions"><span className="detail-label">管理</span></span>
              </summary>
              <div className="detail-panel provider-sections">
                <section>
                  <h3 className="section-label">凭据</h3>
                  {p.store_key ? <KeyForm p={p} onDone={() => { invalidateCheck(p.id); load(); }} setErr={setErr} />
                    : <p className="muted">{p.ready ? "登录凭据已就绪。" : "登录凭据尚未就绪。"}</p>}
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
                    <button disabled={!p.ready || !checkModel || result === "checking"} onClick={() => check(p, checkModel)}>
                      {result === "checking" ? "检测中…" : "检测连接"}
                    </button>
                  </div>
                  <p className="muted">向选中模型发送一条短请求，可能占用少量上游额度。</p>
                  {result && result !== "checking" && result.model === checkModel && <p role="status">
                    {fmtRouteReason(result.status)}
                    {result.model && ` · ${result.model}`}
                    {result.upstream_status != null && ` · HTTP ${result.upstream_status}`}
                  </p>}
                </section>
                <section>
                  <h3 className="section-label">额度</h3>
                  {(p.util_5h != null || p.util_7d != null) ? (
                    <p className="muted">
                      5 小时 / 7 天窗口已用:{fmtUtil(p)}
                      {p.resets_at != null && ` · ${fmtTime(p.resets_at)} 重置`}
                    </p>
                  ) : (
                    <p className="muted">尚无窗口读数（被动采样与主动查询都会补上）。</p>
                  )}
                  {quotaExpanded[p.id] && <QuotaCurve provider={p.id} setErr={setErr} />}
                </section>
                <section>
                  <h3 className="section-label">模型与路由</h3>
                  {models.length ? <ul className="model-list">{models.map(model => <li key={model}>{model}</li>)}</ul> : <p className="muted">此源没有模型。</p>}
                  <RoutingStatus provider={p} />
                  {!!p.free_requests?.length && <details className="section-details">
                    <summary>免费请求状态</summary>
                    <ul>{p.free_requests.map(entry => <li key={entry.model}>
                      {entry.model} · {fmtRouteReason(entry.status)}
                      {entry.retry_at ? ` · ${fmtTime(entry.retry_at)} 可重试` : ""}
                    </li>)}</ul>
                  </details>}
                </section>
                <div className="actions">
                  <button disabled={routingPending} onClick={() => updateRouting(p, "pause")}>{p.paused ? "恢复推理" : "暂停推理"}</button>
                  {p.routing_disabled && <button disabled={routingPending} onClick={() => updateRouting(p, "resume-routing")}
                    title="确认上游限制已解除后，允许该源再次参与路由">重新允许路由</button>}
                  {p.source === "custom" && <>
                    <button onClick={() => setEditingProvider(p)}>编辑来源</button>
                    <button className="ghost button-danger" onClick={() => setDeleteProvider(p)}>删除来源</button>
                  </>}
                </div>
              </div>
            </details>
          );
        })}
        {!rows.length && <p className="empty-state">还没有模型源，添加一个即可开始。</p>}
      </div>
      {(directory || rows.some(p => p.id === "openrouter")) && <section className="panel">
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
          {!!directory.models.length && <details className="section-details">
            <summary>{directory.models.length} 个免费文本模型</summary>
            <ul className="catalog-list">{directory.models.map(model => <li key={model.id}>
              {model.id} · {model.tools ? "支持工具" : "未声明支持工具"}
              {model.context && ` · ${model.context.toLocaleString()} token 上下文`}
            </li>)}</ul>
          </details>}
          </> : <p className="muted">目录状态未知。</p>}
          {rows.some(p => p.id === "openrouter") && <p className="muted">Key 预算（USD） · {data.openrouter_key_budget
            ? data.openrouter_key_budget.limit == null ? "未设上限" : `剩余 ${data.openrouter_key_budget.limit_remaining ?? "未知"} / ${data.openrouter_key_budget.limit}`
            : "未知"}</p>}
        </div>
      </section>}
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

      <section className="panel">
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

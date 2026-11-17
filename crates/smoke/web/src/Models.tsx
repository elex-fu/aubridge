import { useEffect, useState } from "react";
import {
  ModelGroup,
  ModelGroupPreview,
  ModelGroupsPage,
  OpenCodeSyncReceipt,
  ProvidersPage,
  deleteJson,
  fetchJson,
  postJson,
  reportOpenCodeSync,
} from "./api";
import { fmtRouteReason, fmtTime } from "./format";
import { Dialog } from "./Dialog";

// 路由规则 = 固定枚举,只选择(ui-design §6.3,magpie 反面教训:
// 没有规则编辑器/嵌套/意图路由)。
const RULES: { value: string; label: string }[] = [
  { value: "free_first", label: "免费优先" },
  { value: "free_only", label: "仅免费档" },
  { value: "sequential", label: "顺序兜底" },
  { value: "round_robin", label: "轮流" },
];
const FREE_ONLY_WARNING = "缺少免费档成员";

// 新建/编辑分组表单(upsert by name):对外名 + 成员多选(字面
// 模型)+ 成员顺序 + 规则。
function GroupForm({
  initial,
  literalModels,
  freeModels,
  saving,
  setSaving,
  onDone,
  onCancel,
  setErr,
}: {
  initial: ModelGroup | null;
  literalModels: string[];
  freeModels: ReadonlySet<string>;
  saving: boolean;
  setSaving: (saving: boolean) => void;
  onDone: () => Promise<void>;
  onCancel: () => void;
  setErr: (s: string) => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [members, setMembers] = useState<string[]>(initial?.members ?? []);
  const [rule, setRule] = useState(initial?.rule ?? "free_first");
  const options = [...new Set([...literalModels, ...(initial?.members ?? [])])];
  const memberOccurrences = new Map<string, number>();
  const freeOnly = rule === "free_only";
  const memberLabel = (model: string) => model + (
    !literalModels.includes(model) ? "（目录中不可用）" : freeOnly && !freeModels.has(model) ? "（仅订阅源）" : ""
  );

  const toggleMember = (m: string) => {
    setMembers(prev => (prev.includes(m) ? prev.filter(x => x !== m) : [...prev, m]));
  };

  const moveMember = (index: number, offset: -1 | 1) => {
    setMembers(current => {
      const target = index + offset;
      if (target < 0 || target >= current.length) return current;
      const next = [...current];
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  };

  const save = async () => {
    if (saving || !name || !members.length) return;
    setSaving(true);
    try {
      const receipt = await postJson<OpenCodeSyncReceipt>("/admin/model-groups", { name, members, rule });
      reportOpenCodeSync(receipt, setErr);
      await onDone();
    } catch (e) {
      setErr(`保存分组失败:${e}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <form className="add-form panel" aria-label="模型分组" onSubmit={e => { e.preventDefault(); void save(); }}>
      <h3>{initial ? "编辑分组" : "新建分组"}</h3>
      <div className="field-grid">
        <label className="field">对外名
          <input value={name} disabled={saving} onChange={e => setName(e.target.value)} placeholder="my-daily" />
        </label>
        <label className="field">规则
          <select value={rule} disabled={saving} onChange={e => setRule(e.target.value)}>
            {RULES.map(r => <option key={r.value} value={r.value}>{r.label}</option>)}
          </select>
        </label>
      </div>
      <p className="section-label">选择成员 <span className="muted">· 新选成员添加到末尾</span></p>
      <div className="member-options">
        {options.map(m => (
          <label key={m} className="check">
            <input
              type="checkbox"
              checked={members.includes(m)}
              disabled={saving}
              onChange={() => toggleMember(m)}
            />{" "}
            {memberLabel(m)}
          </label>
        ))}
      </div>
      <p className="section-label">已选成员顺序</p>
      {members.length ? (
        <ol className="member-order" aria-label="已选成员顺序">
          {members.map((model, index) => {
            const occurrence = memberOccurrences.get(model) ?? 0;
            memberOccurrences.set(model, occurrence + 1);
            return (
              <li key={JSON.stringify([model, occurrence])}>
                {memberLabel(model)}{" "}
                <button type="button" className="ghost" aria-label={`上移 ${model}`}
                  disabled={saving || index === 0} onClick={() => moveMember(index, -1)}>上移</button>{" "}
                <button type="button" className="ghost" aria-label={`下移 ${model}`}
                  disabled={saving || index === members.length - 1} onClick={() => moveMember(index, 1)}>下移</button>
              </li>
            );
          })}
        </ol>
      ) : <p className="muted">尚未选择成员。</p>}
      {freeOnly && members.length > 0 && !members.some(m => freeModels.has(m)) && (
        <p className="muted" role="status">{FREE_ONLY_WARNING}。请勾选至少一个有免费源的模型。</p>
      )}
      <div className="actions">
        <button type="button" disabled={saving} onClick={onCancel}>取消</button>
        <button className="button-primary" type="submit" disabled={saving || !name || !members.length}>
          {saving ? "保存中…" : "保存分组"}
        </button>
      </div>
    </form>
  );
}

function RoutePreviewResult({ name, tools }: { name: string; tools: boolean }) {
  const [result, setResult] = useState<ModelGroupPreview | { error: string } | null>(null);

  useEffect(() => {
    const request = new AbortController();
    fetchJson<ModelGroupPreview>(`/admin/routing/preview?model=${encodeURIComponent(name)}${tools ? "&tools=true" : ""}`, {
      signal: request.signal,
      cache: "no-store",
    }).then(data => {
      if (!request.signal.aborted) setResult(data);
    }).catch(e => {
      if (!request.signal.aborted) setResult({ error: e instanceof Error ? e.message : String(e) });
    });
    return () => request.abort();
  }, [name, tools]);

  const candidateLabel = ({ id, model, kind }: ModelGroupPreview["candidates"][number]) =>
    `${id} / ${model} · ${kind === "subscription" ? "订阅" : kind === "free_tier" ? "免费档" : "类别未知"}`;

  return (
    <>
      {result === null && <p role="status">正在读取候选…</p>}
      {result && "error" in result && <p role="alert">读取路由预览失败 · {result.error}</p>}
      {result && !("error" in result) && (
        <>
          {result.serving === false && (
            <p className="badge warn" role="status">网关已停止，当前推理请求会被拒绝。以下为恢复后的候选快照。</p>
          )}
          <h3 id="route-preview-candidates">候选顺序</h3>
          {result.candidates.length ? (
            <ol aria-labelledby="route-preview-candidates">
              {result.candidates.map(entry => (
                <li key={`${entry.id}:${entry.model}`}>{candidateLabel(entry)}</li>
              ))}
            </ol>
          ) : <p>当前无可尝试的候选。</p>}
          <h3 id="route-preview-skipped">跳过的候选</h3>
          {result.skipped.length ? (
            <ul aria-labelledby="route-preview-skipped">
              {result.skipped.map(entry => (
                <li key={`${entry.id}:${entry.model}`}>
                  {candidateLabel(entry)} · {fmtRouteReason(entry.reason)}
                  {entry.retry_at ? ` · ${fmtTime(entry.retry_at)} 可重试` : ""}
                </li>
              ))}
            </ul>
          ) : <p>没有跳过的候选。</p>}
        </>
      )}
    </>
  );
}

function RoutePreview({ name, onClose }: { name: string; onClose: () => void }) {
  const [tools, setTools] = useState(false);

  return (
    <Dialog labelledBy="route-preview-title" className="route-preview" onClose={onClose}>
      <div className="modal-header"><h2 id="route-preview-title">预览路由 · {name}</h2></div>
      <div className="modal-body">
        <div className="actions">
          <button type="button" aria-pressed={!tools} onClick={() => setTools(false)}>普通文本</button>
          <button type="button" aria-pressed={tools} onClick={() => setTools(true)}>工具调用</button>
        </div>
        <p className="muted">按{tools ? "工具调用" : "普通文本"}请求生成的本机快照。实际路由会随请求能力、会话和负载变化。</p>
        <RoutePreviewResult key={tools ? "tools" : "text"} name={name} tools={tools} />
      </div>
      <div className="modal-actions"><button type="button" autoFocus onClick={onClose}>关闭</button></div>
    </Dialog>
  );
}

// 模型页(ui-design §6.3):两类分组同一张表——同名多源映射(组名
// = 字面模型名,对外无感)+ 跨模型合并(自定义对外名)。自动
// (aubridge)内置行不可删。字面模型默认免费优先(网关天然行为),
// 想换规则 = 建同名分组。
export function Models({ setErr }: { setErr: (s: string) => void }) {
  const [groups, setGroups] = useState<ModelGroupsPage | null>(null);
  const [provs, setProvs] = useState<ProvidersPage | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [form, setForm] = useState<ModelGroup | null>(null); // null = 收起
  const [showForm, setShowForm] = useState(false);
  const [saving, setSaving] = useState(false);
  const [preview, setPreview] = useState<string | null>(null);
  const [deleteGroup, setDeleteGroup] = useState<ModelGroup | null>(null);

  const load = () => {
    setLoadFailed(false);
    const groupsLoaded = fetchJson<ModelGroupsPage>("/admin/model-groups")
      .then(setGroups)
      .catch(e => { setLoadFailed(true); setErr(String(e)); });
    fetchJson<ProvidersPage>("/admin/providers")
      .then(setProvs)
      .catch(e => { setLoadFailed(true); setErr(String(e)); });
    return groupsLoaded;
  };
  useEffect(() => { void load(); }, [setErr]);

  if (!groups || !provs) return <p className="status-loading" role="status">{loadFailed ? "模型数据暂不可读取。" : "正在读取模型…"}{loadFailed && <button className="ghost" onClick={() => void load()}>重试</button>}</p>;

  const list = groups.groups || [];
  const rows = provs.providers || [];

  // 字面模型 → 提供它的源(同名多源映射的"多源"计数)。
  const byModel = new Map<string, string[]>();
  const freeModels = new Set<string>();
  for (const p of rows) {
    for (const m of p.models || []) {
      byModel.set(m, [...(byModel.get(m) ?? []), p.id]);
      if (p.kind === "free_tier") freeModels.add(m);
    }
  }
  const literalModels = [...byModel.keys()].sort();
  const membersLabel = (group: ModelGroup) => {
    const freeOnly = group.rule === "free_only";
    const labels = group.members.map(member => {
      if (!byModel.has(member)) return `${member}（目录中不可用）`;
      if (freeOnly && !freeModels.has(member)) return `${member}（仅订阅源）`;
      return member;
    });
    return <>
      {labels.length > 2 ? <details className="section-details"><summary>{labels.length} 个成员</summary><ol>{labels.map((label, index) => <li key={index}>{label}</li>)}</ol></details> : labels.join(" + ")}
      {freeOnly && !group.members.some(m => freeModels.has(m)) && (
        <div><span className="badge bad">{FREE_ONLY_WARNING}</span></div>
      )}
    </>;
  };

  const setRule = async (g: ModelGroup, rule: string) => {
    try {
      const receipt = await postJson<OpenCodeSyncReceipt>("/admin/model-groups", { name: g.name, members: g.members, rule });
      reportOpenCodeSync(receipt, setErr);
      load();
    } catch (e) {
      setErr(`改 ${g.name} 规则失败:${e}`);
    }
  };

  const remove = async (g: ModelGroup) => {
    if (saving) return;
    setSaving(true);
    try {
      const receipt = await deleteJson<OpenCodeSyncReceipt>(`/admin/model-groups/${encodeURIComponent(g.name)}`);
      reportOpenCodeSync(receipt, setErr);
      setDeleteGroup(null);
      load();
    } catch (e) {
      setErr(`删除分组 ${g.name} 失败:${e}`);
      setDeleteGroup(null);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="models-page management-page">
      <div className="page-heading">
        <div><h2 className="page-title">模型</h2><p className="page-description">组合模型，选择固定的调度规则</p></div>
        <button className="button-primary" disabled={saving} onClick={() => { setShowForm(showForm && form === null ? false : true); setForm(null); }}>
          {showForm && !form ? "收起分组" : "新建分组"}
        </button>
      </div>
      <div className="panel status-line">
        <strong>自动 (aubridge)</strong>
        <span className="muted">全部源自动调度，免费档优先，订阅兜底</span>
        <span className="badge off">内置 · 不可删</span>
      </div>
      {showForm && (
        <GroupForm
          key={form?.name ?? ""}
          initial={form}
          literalModels={literalModels}
          freeModels={freeModels}
          saving={saving}
          setSaving={setSaving}
          onDone={async () => { await load(); setShowForm(false); }}
          onCancel={() => setShowForm(false)}
          setErr={setErr}
        />
      )}
      <div className="table-scroll panel">
      <table className="models-table">
        <thead>
          <tr>
            <th>对外模型</th>
            <th>说明</th>
            <th>规则</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          {literalModels.map(m => {
            const g = list.find(x => x.name === m);
            const srcs = byModel.get(m) ?? [];
            return (
              <tr key={m}>
                <td>{m}</td>
                <td className="muted">
                  {g ? membersLabel(g) : srcs.length > 2 ? <details className="section-details"><summary>{srcs.length} 个源</summary><ul>{srcs.map(id => <li key={id}>{id}</li>)}</ul></details> : srcs.join(" · ")}
                  {srcs.length > 1 && !g && <span className="hint">默认免费优先</span>}
                </td>
                <td>
                  {g ? (
                    <select aria-label={`${g.name} 的规则`} value={g.rule} disabled={saving} onChange={e => setRule(g, e.target.value)}>
                      {RULES.map(r => (
                        <option key={r.value} value={r.value}>
                          {r.label}
                        </option>
                      ))}
                    </select>
                  ) : (
                    <button
                      className="ghost"
                      disabled={saving}
                      onClick={() => {
                        setForm({ name: m, members: [m], rule: "free_first" });
                        setShowForm(true);
                      }}
                    >
                      免费优先 · 设置
                    </button>
                  )}
                </td>
                <td>
                  {g && <>
                    <button className="ghost" onClick={() => setPreview(g.name)}>预览路由</button>{" "}
                    <button className="ghost" disabled={saving} onClick={() => { setForm(g); setShowForm(true); }}>编辑</button>{" "}
                    <button className="ghost button-danger" disabled={saving} onClick={() => setDeleteGroup(g)}>删除</button>
                  </>}
                </td>
              </tr>
            );
          })}
          {list
            .filter(g => !literalModels.includes(g.name))
            .map(g => (
              <tr key={g.name}>
                <td>
                  {g.name}
                  {g.name !== g.members[0] && <span className="badge off">合并</span>}
                </td>
                <td className="muted">{membersLabel(g)}</td>
                <td>
                  <select aria-label={`${g.name} 的规则`} value={g.rule} disabled={saving} onChange={e => setRule(g, e.target.value)}>
                    {RULES.map(r => (
                      <option key={r.value} value={r.value}>
                        {r.label}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <button className="ghost" onClick={() => setPreview(g.name)}>预览路由</button>{" "}
                  <button
                    className="ghost"
                    disabled={saving}
                    onClick={() => {
                      setForm(g);
                      setShowForm(true);
                    }}
                  >
                    编辑
                  </button>{" "}
                  <button className="ghost button-danger" disabled={saving} onClick={() => setDeleteGroup(g)}>
                    删除
                  </button>
                </td>
              </tr>
            ))}
        </tbody>
      </table>
      </div>
      <details className="section-details">
        <summary>四种规则如何选择</summary>
        <p className="muted">顺序兜底按成员列表顺序尝试。免费优先先尝试可用免费档，再由订阅兜底。仅免费档只使用免费源。轮流以成员列表为基础应用现有轮次，下一次首选取决于轮次和候选可用性。</p>
      </details>
      {deleteGroup && <Dialog labelledBy="delete-model-group-title" role="alertdialog" busy={saving} onClose={() => setDeleteGroup(null)}>
        <div className="modal-header"><h2 id="delete-model-group-title">删除模型分组？</h2></div>
        <div className="modal-body"><p>删除 {deleteGroup.name} 的分组配置。正在使用这个对外名称的 Agent 可能受到影响。</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={saving} onClick={() => setDeleteGroup(null)}>取消</button>
          <button className="button-danger" disabled={saving} onClick={() => void remove(deleteGroup)}>{saving ? "删除中…" : "确认删除"}</button>
        </div>
      </Dialog>}
      {preview !== null && <RoutePreview key={preview} name={preview} onClose={() => setPreview(null)} />}
    </div>
  );
}

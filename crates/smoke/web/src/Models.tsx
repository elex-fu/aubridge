import { useEffect, useRef, useState } from "react";
import {
  ModelGroup,
  ModelGroupPreview,
  ModelGroupsPage,
  OpenCodeSyncReceipt,
  ProvidersPage,
  fetchJson,
  reportOpenCodeSync,
} from "./api";
import { fmtRouteReason, fmtTime } from "./format";
import { Dialog } from "./Dialog";
import { TaskView } from "./TaskView";

// 路由规则 = 固定枚举,只选择(ui-design §6.3,magpie 反面教训:
// 没有规则编辑器/嵌套/意图路由)。
const RULES: { value: string; label: string }[] = [
  { value: "free_first", label: "免费优先" },
  { value: "free_only", label: "仅免费档" },
  { value: "sequential", label: "顺序兜底" },
  { value: "round_robin", label: "轮流" },
  { value: "smart", label: "免费优先 · 订阅临期先用" },
  { value: "pace", label: "免费优先 · 订阅按每小时可用额度" },
];
const RULE_HELP: Record<string,string> = {
  free_first:"先尝试可用免费档，再尝试订阅；仍受全局费用范围限制。",
  free_only:"只使用免费档类别来源；类别不等于价格已确认免费。",
  sequential:"按已选成员顺序尝试可用来源。",
  round_robin:"按现有轮次选择首个可用成员。",
  smart:"免费优先，订阅按现有额度窗口的临期程度排序。",
  pace:"免费优先，订阅按现有每小时可用额度排序。",
};
const FREE_ONLY_WARNING = "缺少免费档成员";
type GroupWrite = { phase: "saving" | "reading" | "unknown" | "idle"; message: string; syncIssue?: string | null; navigation?: boolean };

// 新建/编辑分组表单(upsert by name):对外名 + 成员多选(字面
// 模型)+ 成员顺序 + 规则。
function GroupForm({
  initial,
  literalModels,
  freeModels,
  saving,
  setSaving,
  onDone,
  saveGroup,
  blocked,
  statusFor,
  onRead,
  onCancel,
  setErr,
}: {
  initial: ModelGroup | null;
  literalModels: string[];
  freeModels: ReadonlySet<string>;
  saving: boolean;
  setSaving: (saving: boolean) => void;
  onDone: () => Promise<void>;
  saveGroup: (group: ModelGroup, originalName?: string) => Promise<boolean>;
  blocked: (name: string) => boolean;
  statusFor: (name: string) => GroupWrite | undefined;
  onRead: (name: string) => void;
  onCancel: () => void;
  setErr: (s: string) => void;
}) {
  const submitting = useRef(false);
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
    if (submitting.current || saving || !name || !members.length || blocked(name) || (initial && blocked(initial.name))) return;
    submitting.current = true;
    setSaving(true);
    try {
      if (await saveGroup({ name, members, rule }, initial?.name)) await onDone();
    } catch (e) {
      setErr(`保存分组失败:${e}`);
    } finally {
      submitting.current = false;
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
      <p className="muted">{RULE_HELP[rule] ?? "规则说明未知。"}</p>
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
        <button className="button-primary" type="submit" disabled={saving || !name || !members.length || blocked(name) || Boolean(initial && blocked(initial.name))}>
          {saving ? "保存中…" : "保存分组"}
        </button>
      </div>
      {statusFor(name) && <div role="status">{statusFor(name)?.message}
        {statusFor(name)?.syncIssue && <span> · OpenCode 模型同步失败：{statusFor(name)?.syncIssue}</span>}
        {statusFor(name)?.phase === "unknown" && <button type="button" className="ghost" onClick={() => onRead(name)}>读取当前状态</button>}
      </div>}
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
export function Models({ setErr, onBusyChange }: { setErr: (s: string) => void; onBusyChange?: (busy: boolean) => void }) {
  const [groups, setGroups] = useState<ModelGroupsPage | null>(null);
  const [provs, setProvs] = useState<ProvidersPage | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [form, setForm] = useState<ModelGroup | null>(null); // null = 新建草稿
  const [showForm, setShowForm] = useState(false);
  const [formVisited,setFormVisited]=useState(false);
  const [draftRevision,setDraftRevision]=useState(0);
  const [detail,setDetail]=useState<{title:string;items:string[]}|null>(null);
  const taskTrigger=useRef<HTMLElement|null>(null);
  const openDetail=(value:{title:string;items:string[]})=>{taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setDetail(value);};
  const returnFocus=()=>window.requestAnimationFrame(()=>taskTrigger.current?.focus());
  const finishDraft=()=>{setShowForm(false);setFormVisited(false);setForm(null);setDraftRevision(current=>current+1);returnFocus();};
  const [saving, setSaving] = useState(false);
  const [preview, setPreview] = useState<string | null>(null);
  const [deleteGroup, setDeleteGroup] = useState<ModelGroup | null>(null);

  const [writes, setWrites] = useState(new Map<string, GroupWrite>());
  const writeRef = useRef(new Map<string, GroupWrite>());
  const fresh = useRef(false);
  const originalTargets = useRef(new Map<string, string>());
  const deleting = useRef(false);
  const mounted = useRef(false);
  const mountGeneration = useRef(0);
  const loadSequence = useRef(0);
  const requests = useRef(new Set<AbortController>());
  const busyCallback = useRef(onBusyChange);
  busyCallback.current = onBusyChange;
  const setWrite = (name: string, value: GroupWrite) => {
    if (!mounted.current) return;
    const previous = writeRef.current.get(name);
    writeRef.current.set(name, {
      ...value,
      syncIssue: value.syncIssue === undefined ? previous?.syncIssue : value.syncIssue,
      navigation: value.phase === "saving" || value.phase === "reading" && previous?.navigation === true,
    });
    setWrites(new Map(writeRef.current));
    busyCallback.current?.([...writeRef.current.values()].some(item => item.navigation));
  };
  const request = async <T,>(path: string, init?: RequestInit): Promise<T> => {
    const controller = new AbortController();
    requests.current.add(controller);
    let timer: ReturnType<typeof window.setTimeout> | undefined;
    try {
      const value = await Promise.race([
        fetchJson<T>(path, { ...init, signal: controller.signal, cache: "no-store" }),
        new Promise<never>((_, reject) => {
          timer = window.setTimeout(() => { controller.abort(); reject(new Error("等待当前请求超过 10 秒，结果未确认")); }, 10_000);
        }),
      ]);
      if (controller.signal.aborted) throw new Error("请求已停止，结果未确认");
      return value;
    } finally { window.clearTimeout(timer); requests.current.delete(controller); }
  };
  const readGroups = async () => {
    const value = await request<ModelGroupsPage>("/admin/model-groups");
    if (!value || !Array.isArray(value.groups) || value.groups.some(group =>
      !group || typeof group.name !== "string" || typeof group.rule !== "string" || !Array.isArray(group.members) || group.members.some(member => typeof member !== "string"))) throw new Error("模型分组状态格式未确认");
    return { ...value, groups: value.groups };
  };
  const load = async () => {
    const generation = mountGeneration.current;
    const sequence = ++loadSequence.current;
    const current = () => mounted.current && generation === mountGeneration.current && sequence === loadSequence.current;
    setLoadFailed(false);
    fresh.current = false;
    await Promise.all([
      readGroups().then(value => { if (current()) { setGroups(value); fresh.current = true; } })
        .catch(e => { if (current()) { setLoadFailed(true); setErr(String(e)); } }),
      request<ProvidersPage>("/admin/providers").then(value => { if (current()) setProvs(value); })
        .catch(e => { if (current()) { setLoadFailed(true); setErr(String(e)); } }),
    ]);
  };
  useEffect(() => {
    mounted.current = true;
    mountGeneration.current += 1;
    void load();
    return () => { mounted.current = false; requests.current.forEach(controller => controller.abort()); busyCallback.current?.(false); };
  }, [setErr]);
  const blocked = (name: string) => !fresh.current || [name, originalTargets.current.get(name)].some(key =>
    key !== undefined && ["saving", "reading", "unknown"].includes(writeRef.current.get(key)?.phase ?? ""));
  const readBack = async (name: string, posted = false, deleted = false) => {
    setWrite(name, { phase: "reading", message: "正在读取当前状态…" });
    try {
      const value = await readGroups();
      if (!mounted.current) return false;
      const row = value.groups.find(group => group.name === name);
      if (!row && !deleted) {
        setWrite(name, { phase: "unknown", message: "找不到此分组，当前状态未确认；不能继续修改旧记录。" });
        return false;
      }
      setGroups(current => {
        if (!current) return current;
        const existing = current.groups ?? [];
        const next = row
          ? existing.some(group => group.name === name) ? existing.map(group => group.name === name ? row : group) : [...existing, row]
          : existing.filter(group => group.name !== name);
        return { ...current, groups: next };
      });
      for (const [original, target] of originalTargets.current) {
        if (target === name) originalTargets.current.delete(original);
      }
      setWrite(name, { phase: "idle", message: posted ? "已读取当前状态。" : "已读取当前状态；这不代表上次提交已成功。" });
      return true;
    } catch (e) {
      if (mounted.current) {
        setWrite(name, { phase: "unknown", message: "保存结果未确认，请先读取当前状态；不会自动重发。" });
        setErr(`读取 ${name} 当前状态失败:${e}`);
      }
      return false;
    }
  };
  const writeGroup = async (name: string, method: "POST" | "DELETE", body?: ModelGroup, originalName?: string) => {
    if (blocked(name) || (originalName && blocked(originalName))) return false;
    if (originalName && originalName !== name) originalTargets.current.set(originalName, name);
    setWrite(name, { phase: "saving", message: "保存中…", syncIssue: null });
    let posted = false;
    try {
      const receipt = await request<OpenCodeSyncReceipt>(method === "DELETE" ? `/admin/model-groups/${encodeURIComponent(name)}` : "/admin/model-groups", {
        method, ...(body ? { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {}),
      });
      if (!mounted.current) return false;
      reportOpenCodeSync(receipt, setErr);
      if (receipt.opencode_issue) setWrite(name, { phase: "saving", message: "保存响应已收到，正在核对…", syncIssue: receipt.opencode_issue });
      posted = true;
    } catch (e) { if (mounted.current) setErr(`保存 ${name} 的结果未确认:${e}`); }
    if (!mounted.current) return false;
    const confirmed = await readBack(name, posted, method === "DELETE");
    return posted && confirmed;
  };
  const retryRead = (name: string) => {
    if (writeRef.current.get(name)?.phase === "unknown") void readBack(name);
  };
  const statusKey = (name: string) => {
    const target = originalTargets.current.get(name);
    return target && ["saving", "reading", "unknown"].includes(writes.get(target)?.phase ?? "") ? target : name;
  };
  const writeStatus = (name: string) => {
    const key = statusKey(name);
    const value = writes.get(key);
    return value && <div role="status">{key !== name && `同组另存为 ${key}：`}{value.message}
      {value.syncIssue && <span> · OpenCode 模型同步失败：{value.syncIssue}</span>}
      {value.phase === "unknown" && <button className="ghost" onClick={() => retryRead(key)}>读取当前状态</button>}
    </div>;
  };

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
      {labels.length > 2 ? <button className="ghost" onClick={()=>openDetail({title:`${group.name} 的成员`,items:labels})}>查看 {labels.length} 个成员</button> : labels.join(" + ")}
      {freeOnly && !group.members.some(m => freeModels.has(m)) && (
        <div><span className="badge bad">{FREE_ONLY_WARNING}</span></div>
      )}
    </>;
  };

  const setRule = (g: ModelGroup, rule: string) => {
    if (saving || blocked(g.name)) return;
    void writeGroup(g.name, "POST", { name: g.name, members: g.members, rule });
  };

  const remove = async (g: ModelGroup) => {
    if (deleting.current || saving || blocked(g.name)) return;
    deleting.current = true;
    setSaving(true);
    try { await writeGroup(g.name, "DELETE"); }
    finally { deleting.current = false; if (mounted.current) { setSaving(false); setDeleteGroup(null); } }
  };

  return (
    <div className="models-page management-page">
      <div className="task-workspace-pane" hidden={showForm || detail!==null}>
      <div className="page-heading">
        <div><h2 className="page-title">模型</h2><p className="page-description">组合模型，选择固定的调度规则</p></div>
        <button className="button-primary" disabled={saving} onClick={() => { taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setShowForm(true);setFormVisited(true); setForm(null); }}>
          新建分组
        </button>
      </div>
      <div className="panel status-line">
        <strong>自动 (aubridge)</strong>
        <span className="muted">按当前费用设置与可用条件自动选择合格模型</span>
        <span className="badge off">内置 · 不可删</span>
      </div>
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
                  {g ? membersLabel(g) : srcs.length > 2 ? <button className="ghost" onClick={()=>openDetail({title:`${m} 的来源`,items:srcs})}>查看 {srcs.length} 个来源</button> : srcs.join(" · ")}
                  {srcs.length > 1 && !g && <span className="hint">默认免费优先</span>}
                </td>
                <td>
                  {g ? (
                    <select aria-label={`${g.name} 的规则`} value={g.rule} disabled={saving || blocked(g.name)} onChange={e => setRule(g, e.target.value)}>
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
                        taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setShowForm(true);setFormVisited(true);
                      }}
                    >
                      免费优先 · 设置
                    </button>
                  )}
                  {g && <><p className="muted">{RULE_HELP[g.rule] ?? "规则说明未知。"}</p>{writeStatus(g.name)}</>}
                </td>
                <td>
                  {g && <>
                    <button className="ghost" onClick={() => setPreview(g.name)}>预览路由</button>{" "}
                    <button className="ghost" disabled={saving || blocked(g.name)} onClick={() => { if (blocked(g.name)) return; setForm(g); taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setShowForm(true);setFormVisited(true); }}>编辑</button>{" "}
                    <button className="ghost button-danger" disabled={saving || blocked(g.name)} onClick={() => { if (!blocked(g.name)) setDeleteGroup(g); }}>删除</button>
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
                  <select aria-label={`${g.name} 的规则`} value={g.rule} disabled={saving || blocked(g.name)} onChange={e => setRule(g, e.target.value)}>
                    {RULES.map(r => (
                      <option key={r.value} value={r.value}>
                        {r.label}
                      </option>
                    ))}
                  </select>
                  <p className="muted">{RULE_HELP[g.rule] ?? "规则说明未知。"}</p>{writeStatus(g.name)}
                </td>
                <td>
                  <button className="ghost" onClick={() => setPreview(g.name)}>预览路由</button>{" "}
                  <button
                    className="ghost"
                    disabled={saving || blocked(g.name)}
                    onClick={() => {
                      if (blocked(g.name)) return;
                      setForm(g);
                      taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setShowForm(true);setFormVisited(true);
                    }}
                  >
                    编辑
                  </button>{" "}
                  <button className="ghost button-danger" disabled={saving || blocked(g.name)} onClick={() => { if (!blocked(g.name)) setDeleteGroup(g); }}>
                    删除
                  </button>
                </td>
              </tr>
            ))}
        </tbody>
      </table>
      </div>
      </div>
      {formVisited && <div className="task-workspace-pane" hidden={!showForm}><TaskView title={form ? `编辑模型分组 · ${form.name}` : "新建模型分组"} busy={saving} onBack={()=>{setShowForm(false);returnFocus();}}>
        <GroupForm
          key={`${draftRevision}:${form?.name ?? "new"}`}
          initial={form}
          literalModels={literalModels}
          freeModels={freeModels}
          saving={saving}
          setSaving={setSaving}
          saveGroup={(group, originalName) => writeGroup(group.name, "POST", group, originalName)}
          blocked={blocked}
          statusFor={name => writes.get(statusKey(name))}
          onRead={name => retryRead(statusKey(name))}
          onDone={async () => { finishDraft(); }}
          onCancel={finishDraft}
          setErr={setErr}
        />
      </TaskView></div>}
      {detail && <TaskView title={detail.title} onBack={()=>{setDetail(null);returnFocus();}}><ol>{detail.items.map((item,index)=><li key={index}>{item}</li>)}</ol></TaskView>}
      {deleteGroup && <Dialog labelledBy="delete-model-group-title" role="alertdialog" busy={saving} onClose={() => setDeleteGroup(null)}>
        <div className="modal-header"><h2 id="delete-model-group-title">删除模型分组？</h2></div>
        <div className="modal-body"><p>删除 {deleteGroup.name} 的分组配置。正在使用这个对外名称的 Agent 可能受到影响。</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={saving} onClick={() => setDeleteGroup(null)}>取消</button>
          <button className="button-danger" disabled={saving || blocked(deleteGroup.name)} onClick={() => void remove(deleteGroup)}>{saving ? "删除中…" : "确认删除"}</button>
        </div>
      </Dialog>}
      {preview !== null && <RoutePreview key={preview} name={preview} onClose={() => setPreview(null)} />}
    </div>
  );
}

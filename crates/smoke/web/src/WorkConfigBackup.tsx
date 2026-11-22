import { useEffect, useRef, useState } from "react";
import { CostIntent, fetchJson, postJson } from "./api";
import { Dialog } from "./Dialog";
import { TaskView } from "./TaskView";

type WiringIntent = { agent: string; enabled: boolean; model: string };
type Payload = {
  sources: { id: string; source: string; base: string; auth: string; protocol: string; kind: string; models: string[] }[];
  groups: { name: string; members: string[]; rule: string }[];
  model_selections: Record<string, string[]>;
  paused_providers: string[];
  chatgpt_login_source: string;
  client_wiring: WiringIntent[];
};
type RestoredCost = {
  requested_cost_intent: CostIntent | null;
  effective_cost_intent: CostIntent | null;
  needs_cost_confirmation: boolean;
  cost_intent_recorded: boolean | null;
};
type BackupView = RestoredCost & {
  status: "restore_pending" | "idle";
  restore_id?: string | null;
  target_revision: string;
  restored_client_wiring_intents: WiringIntent[];
  client_wiring_status: "intent_only";
  pending: { payload: Payload; archive_digest: string; bound_sources: string[] } | null;
  dormant?: { payload: Payload; archive_digest: string; bound_sources: string[] }[] | null;
  sources: { id: string; base: string; auth: string; protocol: string; kind: string; expected_identity: string; credential_source: string; credential_status: "bound" | "unconfirmed"; can_supply_key?: boolean }[];
};
type Preview = RestoredCost & { preview_id: string; digest: string; target_revision: string; payload: Payload; status: "awaiting_confirmation"; client_wiring_status: "intent_only" };

const costLabel = (intent: CostIntent | null | undefined) => intent === "confirmed_free_only" ? "只用确认免费" : intent === "free_then_subscription" ? "免费优先，可用我的订阅" : intent === null ? "本机尚未选择费用意图" : "暂不可读取";
const requestedCostLabel = (cost: RestoredCost) => cost.cost_intent_recorded === false ? "归档未记录，保留本机" : cost.requested_cost_intent === null ? "无待确认的恢复意图" : costLabel(cost.requested_cost_intent);
const costConfirmation = (cost: RestoredCost, preview = false) => cost.needs_cost_confirmation ?
  cost.effective_cost_intent === "confirmed_free_only" ? `${preview ? "应用后将" : "已"}按仅确认免费保护，订阅兜底待重新确认。` : "订阅兜底待在模型源重新明确确认。" : "归档不会导入或生成订阅授权。";

function RestoredCostFacts({ cost, preview = false }: { cost: RestoredCost; preview?: boolean }) {
  return <section className="section-details" aria-label={preview ? "归档费用恢复预览" : "费用恢复状态"}>
    <p>请求费用策略：{requestedCostLabel(cost)}</p>
    <p>{preview ? "应用后费用策略" : "当前实际生效策略"}：{costLabel(cost.effective_cost_intent)}</p>
    {cost.cost_intent_recorded === false && <p className="muted">旧归档未记录费用意图，保留本机费用策略及已有的待确认状态。</p>}
    <p className="muted">{costConfirmation(cost, preview)}{cost.needs_cost_confirmation ? "请在模型源的现有费用范围设置中明确选择并重新授权。" : ""}</p>
  </section>;
}

function WiringIntentList({ intents }: { intents: WiringIntent[] }) {
  return <>
    {intents.map(intent => <p key={intent.agent}>{intent.agent} · {intent.enabled ? "希望连接" : "希望断开"}{intent.model ? ` · ${intent.model}` : ""}</p>)}
    <p className="muted">这里只展示恢复意图。实际 CLI 文件及模型选择需要到 Agents 核对后单独应用；不会在恢复时自动改写。</p>
  </>;
}

function PayloadPreview({ payload }: { payload: Payload }) {
  return <div className="work-config-preview">
    <h3>待恢复的来源与模型</h3>
    {payload.sources.map(source => <div className="detail-panel" key={source.id}>
      <strong>{source.id}</strong> · {source.kind === "subscription" ? "订阅类别" : "免费档类别（不是免费价格证明）"}{payload.paused_providers.includes(source.id) ? " · 保持暂停" : ""}
      <p><code>{source.base}</code></p><p>协议 {source.protocol} · 鉴权 {source.auth} · {source.models.length} 个模型</p>
      <p className="muted">{source.models.join(" · ") || "未选择模型"}</p>
    </div>)}
    {!payload.sources.length && <p className="muted">归档没有来源。</p>}
    <h3>分组与选择</h3>
    {payload.groups.map(group => <p key={group.name}>{group.name} · {group.rule} · {group.members.join(" → ")}</p>)}
    {!payload.groups.length && <p className="muted">没有分组。</p>}
    {Object.entries(payload.model_selections).map(([id, models]) => <p key={id}>{id}：{models.join(" · ") || "未选择"}</p>)}
    <p>登录来源偏好：{payload.chatgpt_login_source || "未指定"}；不包含登录凭据或写回授权。</p>
    <h3>客户端连接意图</h3>
    <WiringIntentList intents={payload.client_wiring} />
  </div>;
}

export function WorkConfigBackup({ onChanged, onOpenSources, onOpenAgents, onBusyChange, onErrorChange, onTaskActiveChange }: {
  onChanged: () => void; onOpenSources: () => void; onOpenAgents: () => void;
  onBusyChange?: (busy: boolean) => void; onErrorChange?: (message: string) => void;
  onTaskActiveChange?: (active: boolean) => void;
}) {
  const [view, setView] = useState<BackupView | null>(null);
  const [exportPath, setExportPath] = useState("");
  const [importPath, setImportPath] = useState("");
  const [preview, setPreview] = useState<Preview | null>(null);
  const [confirmed, setConfirmed] = useState<Record<string, boolean>>({});
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [keyWrites, setKeyWrites] = useState<Record<string, boolean>>({});
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [receipt, setReceipt] = useState("");
  const taskTrigger = useRef<HTMLElement | null>(null);
  const [task, setTask] = useState<"export" | "import" | "sources" | "pending" | "dormant" | "key" | null>(null);
  const sourceTaskTrigger = useRef<HTMLElement | null>(null);
  const returnToSources = useRef(false);
  const [taskVisited, setTaskVisited] = useState<Record<string, boolean>>({});
  const [keyTarget, setKeyTarget] = useState("");
  const [undoing, setUndoing] = useState(false);
  const alive = useRef(true);
  const pending = useRef(false);
  const generation = useRef(0);
  const closeTask = () => { const returning = returnToSources.current; returnToSources.current = false; setTask(returning ? "sources" : null); window.requestAnimationFrame(() => { const trigger = taskTrigger.current; if (trigger?.isConnected && !(trigger instanceof HTMLButtonElement && trigger.disabled)) trigger.focus(); else document.getElementById("work-config-title")?.focus(); if (returning) taskTrigger.current = sourceTaskTrigger.current; }); };
  const read = async () => {
    const current = ++generation.current;
    setLoading(true);
    try {
      const next = await fetchJson<BackupView>("/admin/work-config", { cache: "no-store" });
      if (alive.current && current === generation.current) { setView(next); setKeys({}); setKeyWrites({}); }
      return next;
    } finally { if (alive.current && current === generation.current) setLoading(false); }
  };
  useEffect(() => {
    alive.current = true;
    void read().catch(() => { if (alive.current) setError("工作配置恢复状态暂不可读，请重新读取。"); });
    return () => { alive.current = false; generation.current++; };
  }, []);
  const mutate = async (action: () => Promise<void>) => {
    if (pending.current) return;
    pending.current = true; setBusy(true); setError(""); setReceipt("");
    try { await action(); }
    catch (failure) {
      if (alive.current) {
        setError(`操作未完成：${failure instanceof Error ? failure.message : "请重新读取当前状态后继续"}`);
        await read().catch(() => {});
      }
    } finally { pending.current = false; if (alive.current) { setBusy(false); setKeys({}); setKeyWrites({}); } }
  };
  useEffect(() => { onBusyChange?.(busy || Boolean(preview) || undoing); }, [busy, preview, undoing, onBusyChange]);
  useEffect(() => { onErrorChange?.(error); }, [error, onErrorChange]);
  useEffect(() => { onTaskActiveChange?.(task !== null); }, [task, onTaskActiveChange]);
  const paused = view?.pending?.payload.paused_providers || [];
  const readyToPromote = Boolean(view?.pending && view.sources.filter(source => !paused.includes(source.id)).every(source => source.credential_status === "bound"));
  const restoreId = view?.restore_id;
  const exportArchive = () => mutate(async () => {
    const result = await postJson<{ status: string; path: string }>("/admin/work-config/export", { path: exportPath.trim() });
    if (result.status !== "exported") throw new Error("导出回执未确认");
    if (alive.current) setReceipt(`工作配置已导出到 ${result.path}。归档不包含密钥、登录令牌、会话或请求账本。`);
  });
  const inspect = () => mutate(async () => {
    const result = await postJson<Preview>("/admin/work-config/preview", { path: importPath.trim() });
    if (result.status !== "awaiting_confirmation") throw new Error("预览回执未确认");
    if (alive.current) setPreview(result);
  });
  const stage = () => mutate(async () => {
    if (!preview) return;
    const next = await postJson<BackupView>("/admin/work-config/stage", { preview_id: preview.preview_id });
    if (alive.current) {
      setView(next); setPreview(null); setConfirmed({}); closeTask();
      setReceipt("归档已暂存，当前生效配置保持原状。核对各来源完整端点与凭据后，再一次应用。");
    }
  });
  const bind = (source: BackupView["sources"][number]) => mutate(async () => {
    if (!view || !restoreId || !confirmed[`${source.id}:${source.expected_identity}`]) return;
    const next = await postJson<BackupView>("/admin/work-config/rebind", {
      restore_id: restoreId, source_id: source.id, expected_identity: source.expected_identity,
      target_revision: view.target_revision, confirm_endpoint: true,
    });
    if (alive.current) setView(next);
  });
  const promote = () => mutate(async () => {
    if (!view || !restoreId || !readyToPromote) return;
    const result = await postJson<RestoredCost & { status: string }>("/admin/work-config/promote", { restore_id: restoreId, target_revision: view.target_revision });
    if (result.status !== "configuration_restored" && result.status !== "restore_pending") throw new Error("配置生效回执未确认");
    const next = await read();
    if (alive.current) {
      const recovery = next.pending ? "已恢复可生效的部分；仍有来源待完成，工作环境恢复尚未完成。" : `${next.dormant?.length ? "当前可启用的工作配置已恢复；另有休眠来源仅保留意图，不参与推理。" : "工作配置已恢复。"}CLI 连接仍需单独核对，并在原项目完成真实文件工具任务后，才算工作环境恢复完成。`;
      setReceipt(`${recovery}请求费用策略：${requestedCostLabel(result)}；实际生效策略：${costLabel(result.effective_cost_intent)}。${costConfirmation(result)}`);
      closeTask(); onChanged();
    }
  });
  const supplyKey = (source: BackupView["sources"][number]) => mutate(async () => {
    const identity = `${source.id}:${source.expected_identity}`;
    if (!view || !restoreId || !source.can_supply_key || !confirmed[identity] || !keyWrites[identity] || !keys[identity]?.trim()) return;
    try {
      const next = await postJson<BackupView>("/admin/work-config/supply-key", {
        restore_id: restoreId, source_id: source.id, expected_identity: source.expected_identity,
        target_revision: view.target_revision, confirm_endpoint: true, confirm_key_write: true, key: keys[identity].trim(),
      });
      if (alive.current) { setView(next); closeTask(); setReceipt("来源凭据已保存到本机钥匙串并绑定。撤销工作配置会保留本次保存的钥匙串凭据。"); }
    } finally { if (alive.current) { setKeys({}); setKeyWrites({}); } }
  });
  const undo = () => mutate(async () => {
    if (!restoreId) return;
    const result = await postJson<{ status: string }>("/admin/work-config/undo", { restore_id: restoreId });
    if (result.status !== "undone") throw new Error("撤销回执未确认");
    await read();
    if (alive.current) { setUndoing(false); closeTask(); setConfirmed({}); setReceipt("本次配置恢复已撤销。后续外部改动由服务端核对，冲突时会拒绝覆盖。"); onChanged(); }
  });
  const openTask = (next: NonNullable<typeof task>) => { if (task === "sources" && (next === "key" || next === "pending")) { sourceTaskTrigger.current = taskTrigger.current; returnToSources.current = true; } taskTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null; setTaskVisited(current => ({ ...current, [next]: true })); setTask(next); };
  const taskLocked = busy || Boolean(preview) || undoing;
  const remainingSources = view?.sources.filter(source => !paused.includes(source.id) && source.credential_status !== "bound").length ?? 0;
  const keySource = view?.sources.find(source => `${source.id}:${source.expected_identity}` === keyTarget);
  return <section className="panel" aria-labelledby="work-config-title">
    <div className="panel-heading"><h3 id="work-config-title" tabIndex={-1}>同机工作配置备份与恢复</h3><button className="ghost" disabled={busy || loading} onClick={() => { setError(""); void read().catch(() => setError("状态暂不可读，请重试。")); }}>重新读取状态</button></div>
    <p className="muted">保存来源、模型、分组和连接意图到应用数据目录外。不包含凭据、登录写回授权、会话或账本；恢复后需确认本机凭据与真实工具任务。</p>
    {error && <p className="alert error" role="alert">{error}</p>}
    {receipt && <p role="status">{receipt}</p>}
    {view && <p className="settings-backup-stage" role="status">当前阶段：{view.pending ? `归档已暂存 · 仍有 ${remainingSources} 个启用来源待确认` : restoreId ? "工作配置已应用 · CLI 连接与原项目任务仍需核对" : "尚未开始恢复 · 请选择导出或恢复目标"}</p>}
    {view && <p className="muted">当前费用策略：{costLabel(view.effective_cost_intent)}。{costConfirmation(view)}{view.needs_cost_confirmation && <button className="ghost" disabled={busy || loading} onClick={onOpenSources}>去模型源确认费用范围</button>}</p>}
    {view && !view.pending && !!view.restored_client_wiring_intents?.length && <p className="muted">已恢复 {view.restored_client_wiring_intents.length} 个客户端连接意图，尚未自动应用。</p>}
    {loading && !view && <p role="status">正在读取恢复状态…</p>}
    {view?.pending && <><p role="status">恢复待完成。暂存来源不参与自动推理；已暂停的来源保持休眠。</p>{!restoreId && <p role="alert">当前恢复回执不可用，请重新读取；确认回执前不能提交恢复。</p>}</>}
    <div className="task-workspace-pane" hidden={task !== null}>
    <div className="button-row"><button disabled={busy || loading} onClick={() => openTask("export")}>导出工作配置</button><button disabled={busy || loading || Boolean(view?.pending)} onClick={() => openTask("import")}>从归档恢复</button></div>
    {view?.pending && <div className="settings-backup-next">
      <p className="muted">{view.sources.length} 个归档来源，{paused.length} 个保持暂停。完整端点与凭据来源在确认任务中逐项核对。</p>
      <div className="button-row"><button className="button-primary" disabled={busy || loading || !restoreId} onClick={() => openTask("sources")}>继续确认恢复来源</button><button disabled={busy || loading || !restoreId} onClick={() => setUndoing(true)}>取消本次恢复</button></div>
    </div>}
    {view && <button className="ghost" disabled={busy || loading} onClick={() => openTask("pending")}>查看恢复费用与连接意图</button>}
    {view && !view.pending && restoreId && <button disabled={busy || loading} onClick={() => setUndoing(true)}>撤销上次工作配置恢复</button>}
    {!!view?.dormant?.length && <p className="muted">有 {view.dormant.length} 份休眠来源意图，尚未启用。<button className="ghost" disabled={busy || loading} onClick={() => openTask("dormant")}>查看休眠来源</button></p>}
    </div>
    {taskVisited.sources && <div className="task-workspace-pane" hidden={task !== "sources"}><TaskView title="确认本次恢复的来源" onBack={closeTask} busy={taskLocked}>
      <p className="muted">逐项核对完整端点与本机凭据。仅已确认的启用来源可以一次应用；暂停来源保留休眠意图。</p>
    {view?.pending && <div className="work-config-pending">
      {view.sources.map(source => {
        const key = `${source.id}:${source.expected_identity}`;
        const bound = source.credential_status === "bound";
        return <div className="detail-panel" key={key}>
          <strong>{source.id}</strong>{paused.includes(source.id) ? " · 保持暂停，可暂不绑定" : " · 需要确认"}
          <p><code>{source.base}</code></p><p>协议 {source.protocol} · 鉴权 {source.auth} · 类别 {source.kind}</p>
          <p>本机凭据来源：{source.credential_source} · {bound ? "本轮已绑定" : "尚未确认，读取失败视为未取得"}</p>
          {!bound && <><label className="row"><input type="checkbox" checked={Boolean(confirmed[key])} disabled={busy || loading} onChange={event => setConfirmed(current => ({ ...current, [key]: event.target.checked }))} />我已核对完整端点、协议、鉴权、类别与本机凭据来源</label><button disabled={busy || loading || !restoreId || !confirmed[key]} onClick={() => void bind(source)}>确认并绑定此来源</button></>}
          {!bound && source.can_supply_key && <button disabled={busy || loading} onClick={() => { setKeyTarget(key); openTask("key"); }}>补齐或替换此来源 Key</button>}
        </div>;
      })}
      <p className="muted">支持 Key 的来源可在此处补齐；登录来源需在所选官方 CLI 登录后重新绑定。不会把环境变量当作钥匙串恢复回执。</p>
      <div className="button-row"><button className="button-primary" disabled={busy || loading || !restoreId || !readyToPromote} onClick={() => void promote()}>一次应用已确认的工作配置</button><button disabled={busy || loading || !restoreId} onClick={() => setUndoing(true)}>取消本次恢复</button></div>
      <button className="ghost" disabled={busy || loading} onClick={() => openTask("pending")}>核对暂存内容与客户端意图</button>
    </div>}
    </TaskView></div>}
    {taskVisited.export && <div className="task-workspace-pane" hidden={task !== "export"}><TaskView title="导出工作配置" onBack={closeTask} busy={taskLocked}>
      <p>导出来源、模型与连接意图，不包含凭据、会话或账本；请选择应用数据目录外的新文件名。</p>
      <p className="muted">可在访达选中文件或目录，按 Option + Command + C 复制路径。导出时在目录路径后添加新文件名，例如 config-backup.json。</p>
<div className="field"><label htmlFor="work-export-path">导出文件的绝对路径</label><div className="row"><input id="work-export-path" className="wide" placeholder="应用数据目录外的新文件路径" value={exportPath} disabled={busy} onChange={event => setExportPath(event.target.value)} /><button disabled={busy || loading || !exportPath.trim().startsWith("/")} onClick={() => void exportArchive()}>导出工作配置</button></div></div>
    </TaskView></div>}
    {taskVisited.import && <div className="task-workspace-pane" hidden={task !== "import"}><TaskView title="从归档恢复工作配置" onBack={closeTask} busy={taskLocked}>
      <p>先预览并核对归档，再确认暂存；暂存不会立即改写生效来源或 CLI 文件。</p>
      <p className="muted">可在访达选中归档，按 Option + Command + C 复制路径。</p>
    <div className="field"><label htmlFor="work-import-path">要恢复的归档绝对路径</label><div className="row"><input id="work-import-path" className="wide" value={importPath} disabled={busy || Boolean(view?.pending)} onChange={event => { setImportPath(event.target.value); setPreview(null); }} /><button disabled={busy || loading || Boolean(view?.pending) || !importPath.trim().startsWith("/")} onClick={() => void inspect()}>预览归档</button></div></div>
    </TaskView></div>}
    {taskVisited.pending && <div className="task-workspace-pane" hidden={task !== "pending"}><TaskView title="核对恢复费用与客户端意图" onBack={closeTask} busy={taskLocked}>
      {view && <><RestoredCostFacts cost={view} /><button className="ghost" disabled={busy || loading} onClick={onOpenSources}>{view.needs_cost_confirmation ? "去模型源确认费用范围" : "查看模型源费用范围"}</button></>}
      {view?.pending ? <PayloadPreview payload={view.pending.payload} /> : view?.restored_client_wiring_intents?.length ? <><WiringIntentList intents={view.restored_client_wiring_intents} /><button className="ghost" disabled={busy || loading} onClick={onOpenAgents}>去 Agents 核对连接设置</button></> : <p>当前没有待核对的暂存内容，请返回查看恢复状态。</p>}
    </TaskView></div>}
    {taskVisited.dormant && <div className="task-workspace-pane" hidden={task !== "dormant"}><TaskView title="尚未启用的休眠来源" onBack={closeTask} busy={taskLocked}>
      <p className="muted">这些来源没有进入可用目录或后台探测。需要时到模型源页显式添加并确认凭据；不影响开始下一份归档恢复。</p>
      {(view?.dormant || []).flatMap((entry, index) => entry.payload.sources.map(source => <p key={`${index}:${source.id}`}>{source.id} · <code>{source.base}</code> · 保持休眠</p>))}

    </TaskView></div>}
    {taskVisited.key && <div className="task-workspace-pane" hidden={task !== "key"}><TaskView title={keySource ? `补齐 ${keySource.id} 本机凭据` : "来源凭据条件已变化"} onBack={closeTask} busy={taskLocked}>
      {keySource && keySource.credential_status !== "bound" && keySource.can_supply_key ? <>
        <p><strong>{keySource.id}</strong> · <code>{keySource.base}</code></p><p>协议 {keySource.protocol} · 鉴权 {keySource.auth} · 类别 {keySource.kind}</p>
        <p>本机凭据来源：{keySource.credential_source}</p>
        <label className="row"><input type="checkbox" checked={Boolean(confirmed[keyTarget])} disabled={busy || loading} onChange={event => setConfirmed(current => ({ ...current, [keyTarget]: event.target.checked }))} />我已核对完整端点、协议、鉴权、类别与本机凭据来源</label>
        <label className="field">此来源的新 Key<input type="password" autoComplete="off" spellCheck={false} value={keys[keyTarget] || ""} disabled={busy || loading} onChange={event => setKeys(current => ({ ...current, [keyTarget]: event.target.value }))} /></label>
        <label className="row"><input type="checkbox" disabled={busy || loading} checked={Boolean(keyWrites[keyTarget])} onChange={event => setKeyWrites(current => ({ ...current, [keyTarget]: event.target.checked }))} />确认写入或替换此来源 ID 的本机钥匙串凭据</label>
        <p className="muted">撤销工作配置会保留本次保存的钥匙串凭据。Key 不进入归档、预览或页面持久存储。</p>
        <button disabled={busy || loading || !restoreId || !confirmed[keyTarget] || !keyWrites[keyTarget] || !keys[keyTarget]?.trim()} onClick={() => void supplyKey(keySource)}>保存 Key 并绑定来源</button>
      </> : <p>来源已绑定或身份已变化。请返回并重新读取恢复状态，不会使用旧确认提交。</p>}
    </TaskView></div>}

    {preview && <Dialog label="预览工作配置恢复" busy={busy} onClose={() => setPreview(null)}>
      <div className="modal-header"><h2>核对工作配置归档</h2></div>
      <div className="modal-body">{error && <p className="alert error" role="alert">{error}</p>}<p>确认后先保存私有恢复前副本并暂存归档，当前来源与客户端文件不会立即改写。来源全部确认后再一次应用。</p><RestoredCostFacts cost={preview} preview /><PayloadPreview payload={preview.payload} /><p className="muted">归档校验标识：<code>{preview.digest}</code></p></div>
      <div className="modal-actions"><button disabled={busy} onClick={() => setPreview(null)}>取消</button><button className="button-primary" disabled={busy} onClick={() => void stage()}>{busy ? "暂存中…" : "确认并暂存"}</button></div>
    </Dialog>}
    {undoing && <Dialog label="撤销工作配置恢复" role="alertdialog" busy={busy} onClose={() => setUndoing(false)}>
      <div className="modal-header"><h2>{view?.pending ? "取消本次恢复？" : "撤销上次恢复？"}</h2></div><div className="modal-body">{error && <p className="alert error" role="alert">{error}</p>}<p>恢复本次操作前受影响的配置字段。若恢复后又被其他工具修改，服务端会拒绝覆盖；请先核对当前配置。</p></div>
      <div className="modal-actions"><button disabled={busy} onClick={() => setUndoing(false)}>保留当前状态</button><button className="button-danger" disabled={busy} onClick={() => void undo()}>{busy ? "撤销中…" : "确认撤销"}</button></div>
    </Dialog>}
  </section>;
}

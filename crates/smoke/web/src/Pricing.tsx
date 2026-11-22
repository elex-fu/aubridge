import { useEffect, useRef, useState } from "react";
import { BenchmarkMapping, PricingPage, fetchJson, postJson } from "./api";
import { fmtTime } from "./format";
import { TaskView } from "./TaskView";

const fields = [
  ["provider", "实际源 ID"], ["model", "实际模型 ID"],
  ["target_provider", "对标源"], ["target_model", "对标模型"],
  ["input", "输入 $/百万 token"], ["output", "输出 $/百万 token"],
  ["read", "缓存读取 $/百万（留空未知）"], ["write", "缓存创建 $/百万（留空未知）"],
  ["source", "价格依据（链接或说明）"], ["version", "依据版本"],
] as const;

export function Pricing({ setErr, onBusyChange }: { setErr: (s: string) => void; onBusyChange?: (busy: boolean) => void }) {
  const [page, setPage] = useState<PricingPage | null>(null);
  const [failed, setFailed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState<Record<string, string>>({});
  const editorTrigger = useRef<HTMLButtonElement | null>(null);
  const [editing, setEditing] = useState(false);
  const [editorVisited, setEditorVisited] = useState(false);
  const [saved, setSaved] = useState(false);
  const closeEditor = () => { setEditing(false); window.requestAnimationFrame(() => editorTrigger.current?.focus()); };
  const load = async () => {
    try { setPage(await fetchJson<PricingPage>("/admin/pricing")); setFailed(false); }
    catch (e) { setFailed(true); setErr(`价格设置读取失败：${e}`); }
  };
  useEffect(() => { void load(); }, []);
  useEffect(() => { onBusyChange?.(busy); }, [busy, onBusyChange]);
  const save = async (mappings: BenchmarkMapping[]) => {
    if (!page || busy) return;
    setBusy(true); setSaved(false);
    try {
      await postJson("/admin/pricing", { expected: page.mappings, mappings });
      setPage({ ...page, mappings }); setSaved(true); closeEditor();
    } catch (e) { setErr(`价格设置保存失败，请刷新后重试：${e}`); }
    finally { setBusy(false); }
  };
  const add = () => {
    try {
      const rate = (key: string, optional = false) => {
        const value = draft[key]?.trim() ?? "";
        if (!value && optional) return null;
        const number = Number(value);
        if (!value || !Number.isFinite(number) || number < 0) throw new Error("价格应为有限非负数；缓存单价可留空");
        return number;
      };
      const mapping: BenchmarkMapping = {
        provider: draft.provider?.trim() ?? "", model: draft.model?.trim() ?? "",
        target_provider: draft.target_provider?.trim() ?? "", target_model: draft.target_model?.trim() ?? "",
        source: draft.source?.trim() ?? "", version: draft.version?.trim() ?? "", observed_at: Math.floor(Date.now()/1000),
        price: { input_per_million: rate("input")!, output_per_million: rate("output")!,
          cache_read_per_million: rate("read", true), cache_write_per_million: rate("write", true) },
      };
      if (!mapping.provider || !mapping.model || !mapping.target_provider || !mapping.target_model || !mapping.source || !mapping.version) throw new Error("请填写源、模型、依据和版本");
      void save([...(page?.mappings ?? []).filter(m => m.provider !== mapping.provider || m.model !== mapping.model), mapping]);
    } catch (e) { setErr(String(e)); }
  };
  return <section className="panel" aria-labelledby="pricing-title">
    <div className="panel-header"><h3 id="pricing-title">用户对标价格</h3><button disabled={busy} className="ghost" onClick={() => void load()}>刷新价格设置</button></div>
    <div className="panel-body">
      <p className="muted">目录价格与用户对标分别统计。对标只是你选定模型的 API 等值，不表示实际账单或现金节省。价格单位为 USD / 百万 token。</p>
      {!page ? <p role="status">{failed ? "价格设置暂不可用，请重试。" : "正在读取价格设置…"}</p> : <>
        <div className="task-workspace-pane" hidden={editing}>
        {page.mappings.length === 0 && <p>尚未配置对标。无法确认的订阅模型保持未定价。</p>}
        {page.mappings.map(m => <div className="panel" key={`${m.provider}/${m.model}`}>
          <p><code>{m.provider} / {m.model}</code> → {m.target_provider} / {m.target_model}</p>
          <p>输入 ${m.price.input_per_million} · 输出 ${m.price.output_per_million} · 缓存读取 {m.price.cache_read_per_million ?? "未知"} · 创建 {m.price.cache_write_per_million ?? "未知"}</p>
          <p className="muted">用户对标 · {m.source} · {m.version} · {fmtTime(m.observed_at)}</p>
          <button disabled={busy} onClick={() => void save(page.mappings.filter(item => item !== m))}>删除对标</button>
        </div>)}
        <button className="button-primary" disabled={busy} onClick={event => { editorTrigger.current = event.currentTarget; setEditorVisited(true); setEditing(true); }}>添加或更新对标</button>
        </div>
        {editorVisited && <div className="task-workspace-pane" hidden={!editing}><TaskView title="添加或更新价格对标" onBack={closeEditor} busy={busy}>
          <p className="muted">同一实际源与模型会更新配置；只影响后续请求，历史快照保持原值。跨厂商目标仅用于用户对标。</p>
          <form onSubmit={e => { e.preventDefault(); add(); }}>
            {fields.map(([key,label]) => <label className="field" key={key}>{label}<input aria-label={label} disabled={busy}
              value={draft[key] ?? ""} onChange={e => setDraft({ ...draft, [key]: e.target.value })} /></label>)}
            <button className="button-primary" disabled={busy}>{busy ? "保存中…" : "保存对标"}</button>
          </form>
        </TaskView></div>}

        {saved && <p role="status">对标设置已保存，只对后续开始的请求生效。</p>}
      </>}
    </div>
  </section>;
}

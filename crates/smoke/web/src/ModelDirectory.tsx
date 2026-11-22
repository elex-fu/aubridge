import { useEffect, useRef, useState } from "react";
import { fetchJson, ModelDirectory as Directory, ModelFacts, OpenCodeSyncReceipt, postJson, ProviderSourceRow, reportOpenCodeSync } from "./api";
import { fmtTime } from "./format";

const evidence: Record<ModelFacts["free_evidence"], string> = {
  unknown: "免费依据未知",
  "declared-quota": "来源声明的免费额度",
  "vendor-zero-price": "厂商目录全部计价维度为零",
  subscription: "使用自有订阅",
};

export function ModelFactsText({ facts }: { facts?: ModelFacts }) {
  if (!facts) return null;
  const state = (value: boolean | null | undefined) => value == null ? "未知" : value ? "支持" : "不支持";
  return <span className="muted">
    {facts.source === "vendor-directory" ? " · 厂商目录" : " · 本机声明"}
    {" · "}{facts.protocol ?? "协议未知"}
    {" · 上下文 "}{facts.context?.toLocaleString() ?? "未知"}
    {" · 输出 "}{facts.output?.toLocaleString() ?? "未知"}
    {" · 工具 "}{state(facts.tools)}
    {" · 推理 "}{state(facts.reasoning)}
    {facts.reasoning_efforts?.length ? `（${facts.reasoning_efforts.join(" / ")}）` : ""}
    {" · 厂商图像输入 "}{state(facts.image_input)}
    {" · "}{evidence[facts.free_evidence] ?? evidence.unknown}
    {facts.price && ` · API 公布价 $${facts.price.input_per_million} / $${facts.price.output_per_million} 每百万输入/输出 token`}
  </span>;
}

const issues: Record<string,string> = {
  "missing-credential": "请先配置当前来源凭据",
  "provider-changed": "来源或登录已变化，请重新读取",
  network: "连接厂商目录失败", timeout: "目录读取超时",
  authentication: "厂商拒绝了当前凭据", "http-status": "厂商未提供可读目录",
  "invalid-directory": "目录格式或分页不完整", "too-large": "目录超过读取上限",
  storage: "目录暂时无法保存到本机", busy: "已有目录正在读取",
};

export function ModelDirectory({ provider, onDone, setErr, onBusyChange }: {
  provider: ProviderSourceRow; onDone: () => Promise<void>; setErr: (error: string) => void; onBusyChange?: (id:string,busy:boolean)=>void;
}) {
  const [directory,setDirectory]=useState<Directory | null>(null);
  const [pending,setPending]=useState(false);
  const [selected,setSelected]=useState<string[]>(provider.models ?? []);
  const generation=useRef(0);
  useEffect(()=>{onBusyChange?.(provider.id,pending);},[provider.id,pending,onBusyChange]);
  const identity=JSON.stringify([provider.id,provider.base,provider.kind,provider.source_health?.cost.connection_identity,provider.models]);
  const endpoint=`/admin/providers/${encodeURIComponent(provider.id)}/catalog`;
  useEffect(()=> {
    const request=new AbortController();
    const revision=++generation.current;
    setPending(false);
    setDirectory(null);
    setSelected(provider.models ?? []);
    fetchJson<Directory>(endpoint,{signal:request.signal}).then(value=> {
      if (revision===generation.current) setDirectory(value);
    }).catch(error=> {if (revision===generation.current && !request.signal.aborted) setErr(`读取 ${provider.id} 目录状态失败：${error}`);});
    return ()=> {generation.current+=1;request.abort();};
  },[identity,endpoint]);

  const refresh=async()=> {
    const revision=++generation.current;
    setPending(true);
    try {
      const value=await postJson<Directory & OpenCodeSyncReceipt>(endpoint,{});
      if (revision===generation.current) {setDirectory(value);reportOpenCodeSync(value,setErr);await onDone();}
    } catch(error) {if(revision===generation.current)setErr(`读取 ${provider.id} 厂商目录失败：${error}`);}
    finally {if(revision===generation.current)setPending(false);}
  };
  const save=async()=> {
    const revision=++generation.current;
    setPending(true);
    try {
      const receipt=await postJson<OpenCodeSyncReceipt>(`/admin/providers/${encodeURIComponent(provider.id)}/catalog-selection`,{models:selected});
      if(revision===generation.current) {reportOpenCodeSync(receipt,setErr);await onDone();}
    } catch(error) {if(revision===generation.current)setErr(`保存模型选择失败：${error}`);}
    finally {if(revision===generation.current)setPending(false);}
  };
  return <section aria-label="厂商目录与能力">
    <p className="muted">
      {directory?.fetched_at ? `${fmtTime(directory.fetched_at)} 获取 · ${directory.models.length} 个模型${directory.stale ? " · 目录已过期" : ""}` : "尚未获取完整目录"}
      {directory?.issue && ` · ${issues[directory.issue] ?? "读取失败"}，保留上次结果。`}
    </p>
    <button disabled={pending || (!provider.ready && provider.id!=="openrouter")} onClick={refresh}>{pending ? "处理中…" : "刷新厂商目录"}</button>
    {directory && directory.models.length>0 && <>
      <p className="muted">能力和 API 价格来自厂商声明；不代表所有输入形式均已通过当前代理协议验收，也不代表订阅额外计费。</p>
      <ul className="model-list">{directory.models.map(model=><li key={model.id}>
        {provider.kind==="subscription" && <input type="checkbox" aria-label={`启用 ${model.id}`} disabled={pending}
          checked={selected.includes(model.id)} onChange={event=>setSelected(current=>event.target.checked ? [...current,model.id] : current.filter(id=>id!==model.id))} />}
        {model.facts?.name ?? model.id}{model.facts?.name && ` (${model.id})`}
        <ModelFactsText facts={model.facts} />
      </li>)}</ul>
      {provider.kind==="subscription" && <button disabled={pending || directory.stale || !!directory.issue || !selected.length} onClick={save}>保存选中模型</button>}
    </>}
  </section>;
}

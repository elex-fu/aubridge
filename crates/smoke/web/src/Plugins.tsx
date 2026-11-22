import { useEffect, useRef, useState } from "react";
import { OpenCodeSyncReceipt, PluginsPage, fetchJson, reportOpenCodeSync } from "./api";

import { TaskView } from "./TaskView";

type PluginWrite = { phase: "saving" | "reading" | "unknown" | "idle"; message: string; syncIssue?: string | null; navigation?: boolean };

export function Plugins({ setErr, onBusyChange }: { setErr: (s: string) => void; onBusyChange?: (busy: boolean) => void }) {
  const [data, setData] = useState<PluginsPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [reading,setReading]=useState<{title:string;text:string}|null>(null);
  const taskTrigger=useRef<HTMLElement|null>(null);
  const openReading=(value:{title:string;text:string})=>{taskTrigger.current=document.activeElement instanceof HTMLElement?document.activeElement:null;setReading(value);};

  const fresh = useRef(false);
  const [writes, setWrites] = useState(new Map<string, PluginWrite>());
  const writeRef = useRef(new Map<string, PluginWrite>());
  const mounted = useRef(false);
  const mountGeneration = useRef(0);
  const loadSequence = useRef(0);
  const requests = useRef(new Set<AbortController>());
  const busyCallback = useRef(onBusyChange);
  busyCallback.current = onBusyChange;

  const setWrite = (key: string, value: PluginWrite) => {
    if (!mounted.current) return;
    const previous = writeRef.current.get(key);
    writeRef.current.set(key, {
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
  const read = async () => {
    const value = await request<PluginsPage>("/admin/plugins");
    if (!value || !Array.isArray(value.plugins) || typeof value.auto_enable !== "boolean" || value.plugins.some(plugin => !plugin || typeof plugin.id !== "string" || typeof plugin.enabled !== "boolean")) throw new Error("插件状态格式未确认");
    return { ...value, plugins: value.plugins };
  };
  const load = async () => {
    if ([...writeRef.current.values()].some(item => item.phase === "saving" || item.phase === "reading")) return;
    const generation = mountGeneration.current;
    const sequence = ++loadSequence.current;
    const current = () => mounted.current && generation === mountGeneration.current && sequence === loadSequence.current;
    setLoading(true);
    fresh.current = false;
    try {
      const value = await read();
      if (current()) { setData(value); setLoadFailed(false); fresh.current = true; }
    } catch (e) { if (current()) { setLoadFailed(true); setErr(String(e)); } }
    finally { if (current()) setLoading(false); }
  };
  useEffect(() => {
    mounted.current = true;
    mountGeneration.current += 1;
    void load();
    return () => {
      mounted.current = false;
      requests.current.forEach(controller => controller.abort());
      busyCallback.current?.(false);
    };
  }, [setErr]);

  const blocked = (key: string) => !fresh.current || ["saving", "reading", "unknown"].includes(writeRef.current.get(key)?.phase ?? "");
  const readBack = async (key: string, posted = false) => {
    setWrite(key, { phase: "reading", message: "正在读取当前状态…" });
    try {
      const value = await read();
      if (!mounted.current) return;
      if (key === "auto") {
        setData(current => current && { ...current, auto_enable: value.auto_enable });
      } else {
        const id = key.slice(7);
        const row = value.plugins.find(plugin => plugin.id === id);
        if (!row || typeof row.enabled !== "boolean") {
          setWrite(key, { phase: "unknown", message: "找不到此插件，当前状态未确认；不能继续修改旧记录。" });
          return;
        }
        setData(current => current && { ...current, plugins: (current.plugins ?? []).map(plugin => plugin.id === id ? row : plugin) });
      }
      setWrite(key, { phase: "idle", message: posted ? "已读取当前状态。" : "已读取当前状态；这不代表上次提交已成功。" });
    } catch (e) {
      if (mounted.current) {
        setWrite(key, { phase: "unknown", message: "保存结果未确认，请先读取当前状态；不会自动重发。" });
        setErr(`读取插件当前状态失败:${e}`);
      }
    }
  };
  const save = async (key: string, path: string, on: boolean) => {
    if (blocked(key)) return;
    setWrite(key, { phase: "saving", message: "保存中…", syncIssue: null });
    let posted = false;
    try {
      const receipt = await request<OpenCodeSyncReceipt>(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ on }) });
      if (!mounted.current) return;
      reportOpenCodeSync(receipt, setErr);
      if (receipt.opencode_issue) setWrite(key, { phase: "saving", message: "保存响应已收到，正在核对…", syncIssue: receipt.opencode_issue });
      posted = true;
    } catch (e) { if (mounted.current) setErr(`保存结果未确认:${e}`); }
    if (mounted.current) await readBack(key, posted);
  };
  const retryRead = (key: string) => {
    if (!fresh.current || writeRef.current.get(key)?.phase !== "unknown") return;
    void readBack(key);
  };
  const writeStatus = (key: string) => writes.get(key) && <div role="status">
    <span>{writes.get(key)?.message}</span>{writes.get(key)?.syncIssue && <span> · OpenCode 模型同步失败：{writes.get(key)?.syncIssue}</span>}{writes.get(key)?.phase === "unknown" && <button className="ghost" disabled={!fresh.current} onClick={() => retryRead(key)}>读取当前状态</button>}
  </div>;

  if (!data) return <p className="status-loading" role="status">{loadFailed ? "插件暂不可读取。" : "正在读取插件…"}{loadFailed && <button className="ghost" onClick={() => void load()}>重试</button>}</p>;

  const invalid = Object.entries(data.invalid || {});

  return (
    <div className="plugins-page management-page">
      {loadFailed && <p className="alert error" role="alert">当前插件状态未确认，不能继续修改。<button className="ghost" disabled={loading} onClick={() => void load()}>重新读取</button></p>}
      <div className="task-workspace-pane" hidden={reading!==null}>
      <div className="page-heading"><div>
        <h2 className="page-title">插件</h2>
        <p className="page-description">扩展模型源，默认关闭</p>
      </div></div>
      <section className="panel">
        <div className="panel-body status-line">
          <label className="switch">
            <input aria-label="自动开启新插件" type="checkbox" checked={!!data.auto_enable} disabled={blocked("auto")} onChange={e => void save("auto", "/admin/plugins/auto", e.target.checked)} />
            <i></i>
          </label>
          <div><strong>自动开启新插件</strong><p className="muted">开启后，新放入目录且通过校验的插件会自动启用。</p></div>
          <span className="badge off">{data.auto_enable ? "已开启" : "已关闭"}</span>
          {writeStatus("auto")}
        </div>
      </section>
      <div className="panel table-scroll">
      <table>
        <thead>
          <tr><th>开启</th><th>插件</th><th>版本</th><th>类别</th><th>模型</th></tr>
        </thead>
        <tbody>
          {(data.plugins || []).map(p => (
            <tr key={p.id}>
              <td>
                <label className="switch">
                  <input
                    type="checkbox"
                    aria-label={`启用插件 ${p.id}`}
                    checked={p.enabled}
                    disabled={blocked(`plugin:${p.id}`)}
                    onChange={e => void save(`plugin:${p.id}`, `/admin/plugins/${encodeURIComponent(p.id)}/enable`, e.target.checked)}
                  />
                  <i></i>
                </label>
              </td>
              <td>{p.id}{writeStatus(`plugin:${p.id}`)}</td>
              <td>{p.version}</td>
              <td><span className={`badge ${p.kind === "subscription" ? "subscription" : "off"}`}>{p.kind === "subscription" ? "自有订阅" : p.kind === "free_tier" ? "免费档" : "类别未知"}</span></td>
              <td>{(p.models || []).join(", ")}</td>
            </tr>
          ))}
          {!(data.plugins || []).length && (
            <tr><td colSpan={5} className="muted">目录中暂无插件</td></tr>
          )}
        </tbody>
      </table>
      </div>
      <section className="panel">
        <h3>本机插件目录</h3>
        <button className="ghost" onClick={()=>openReading({title:"本机插件目录",text:data.dir || "未配置；独立网关默认 ~/.aubridge/plugins，可通过 PLUGINS_DIR 指定。"})}>查看目录路径</button>
      </section>
      {invalid.length>0 && <section className="invalid" role="status">
        <h3>校验失败 · {invalid.length} 个文件</h3>
        <p>修正对应文件后重新读取；校验失败的文件不会因此被启用。</p>
        <button disabled={loading || [...writeRef.current.values()].some(item => item.phase === "saving" || item.phase === "reading")} onClick={() => void load()}>重新读取</button>
        {invalid.map(([file,errors])=><div className="panel" key={file}>
          <strong>{file.split("/").pop() || file}</strong>
          <p>{(errors[0] || "校验失败").slice(0,140)}{(errors[0]?.length??0)>140 ? "…" : ""}</p>
          <button className="ghost" onClick={()=>openReading({title:"插件校验错误",text:[file,...errors].join("\n")})}>读取完整错误</button>
        </div>)}
      </section>}
      </div>
      {reading && <TaskView title={reading.title} onBack={()=>{setReading(null);window.requestAnimationFrame(()=>taskTrigger.current?.focus());}}><pre className="task-log">{reading.text}</pre></TaskView>}

    </div>
  );
}

import { useEffect, useState } from "react";
import { OpenCodeSyncReceipt, PluginsPage, PluginRow, fetchJson, postJson, reportOpenCodeSync } from "./api";

export function Plugins({ setErr }: { setErr: (s: string) => void }) {
  const [data, setData] = useState<PluginsPage | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);

  const load = () => {
    fetchJson<PluginsPage>("/admin/plugins")
      .then(value => { setData(value); setLoadFailed(false); })
      .catch(e => { setLoadFailed(true); setErr(String(e)); });
  };
  useEffect(load, [setErr]);

  if (!data) return <p className="status-loading" role="status">{loadFailed ? "插件暂不可读取。" : "正在读取插件…"}{loadFailed && <button className="ghost" onClick={load}>重试</button>}</p>;

  const togglePlugin = async (p: PluginRow, on: boolean) => {
    try {
      const receipt = await postJson<OpenCodeSyncReceipt>(`/admin/plugins/${p.id}/enable`, { on });
      reportOpenCodeSync(receipt, setErr);
      load();
    } catch (e) {
      setErr(`切换 ${p.id} 失败:${e}`);
    }
  };

  const toggleAuto = async (on: boolean) => {
    try {
      const receipt = await postJson<OpenCodeSyncReceipt>("/admin/plugins/auto", { on });
      reportOpenCodeSync(receipt, setErr);
      load();
    } catch (e) {
      setErr(`切换自动开启失败:${e}`);
    }
  };

  const invalid = Object.entries(data.invalid || {});

  return (
    <div className="plugins-page management-page">
      <div className="page-heading"><div>
        <h2 className="page-title">插件</h2>
        <p className="page-description">扩展模型源，默认关闭</p>
      </div></div>
      <section className="panel">
        <div className="panel-body status-line">
          <label className="switch">
            <input aria-label="自动开启新插件" type="checkbox" checked={!!data.auto_enable} onChange={e => toggleAuto(e.target.checked)} />
            <i></i>
          </label>
          <div><strong>自动开启新插件</strong><p className="muted">开启后，新放入目录且通过校验的插件会自动启用。</p></div>
          <span className="badge off">{data.auto_enable ? "已开启" : "已关闭"}</span>
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
                    onChange={e => togglePlugin(p, e.target.checked)}
                  />
                  <i></i>
                </label>
              </td>
              <td>{p.id}</td>
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
      <details className="section-details panel">
        <summary>本机插件目录</summary>
        <p className="muted">{data.dir || "未配置；独立网关默认使用 ~/.aubridge/plugins，可通过 PLUGINS_DIR 指定。"}</p>
      </details>
      {invalid.length > 0 && (
        <div className="invalid">
          <details open>
            <summary>校验失败 · {invalid.length} 个文件</summary>
            {invalid.map(([f, errs]) => (
              <pre key={f}>{f}{"\n"}{errs.join("\n")}</pre>
            ))}
          </details>
        </div>
      )}
    </div>
  );
}

import { useEffect, useState } from "react";
import { SettingsPage, fetchJson, postJson } from "./api";
import { fmtTime } from "./format";
import { Dialog } from "./Dialog";

export function Settings({ setErr }: { setErr: (s: string) => void }) {
  const [data, setData] = useState<SettingsPage | null>(null);
  const [pending, setPending] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [confirmRestore, setConfirmRestore] = useState(false);
  const [draftSource, setDraftSource] = useState<"codex" | "opencode" | "">("");
  const [savingSource, setSavingSource] = useState(false);
  const [sourceSaved, setSourceSaved] = useState(false);

  const load = () => fetchJson<SettingsPage>("/admin/settings").then(value => {
    setData(value);
    setDraftSource(value.chatgpt_login_source === "codex" || value.chatgpt_login_source === "opencode" ? value.chatgpt_login_source : "");
    setLoadFailed(false);
  });
  useEffect(() => {
    void load().catch(e => { setLoadFailed(true); setErr(String(e)); });
  }, [setErr]);

  const refresh = () => {
    return load().catch(e => setErr(`刷新设置元信息失败:${e}`));
  };

  if (!data) return <p className="status-loading" role="status">{loadFailed ? "设置暂不可读取。" : "正在读取设置…"}{loadFailed && <button className="ghost" onClick={() => void refresh()}>重试</button>}</p>;

  const codexSelected = data.chatgpt_login_source === "codex";
  const openCodeSelected = data.chatgpt_login_source === "opencode";

  const applySource = async () => {
    if (pending || !data.wired || !draftSource || draftSource === data.chatgpt_login_source) return;
    setPending(true);
    setSavingSource(true);
    setSourceSaved(false);
    try {
      const receipt = await postJson<Pick<SettingsPage, "chatgpt_login_source">>("/admin/settings", { chatgpt_login_source: draftSource });
      setData(current => current && {
        ...current, chatgpt_login_source: receipt.chatgpt_login_source,
        chatgpt_login_status: "unavailable", chatgpt_login_path: null,
      });
      setDraftSource(receipt.chatgpt_login_source ?? "");
      setSourceSaved(true);
      await refresh();
    } catch (e) {
      setErr(`应用 ChatGPT 登录来源失败:${e}`);
    } finally {
      setPending(false);
      setSavingSource(false);
    }
  };

  const toggleShare = async (next: boolean) => {
    if (pending || !codexSelected || !data.wired) return;
    setPending(true);
    try {
      const receipt = await postJson<Pick<SettingsPage, "share_login">>("/admin/settings", { share_login: next });
      setData(current => current && { ...current, share_login: receipt.share_login });
      await refresh();
    } catch (e) {
      setErr(`切换共享登录失败:${e}`);
    } finally {
      setPending(false);
    }
  };

  const restoreStash = async () => {
    if (pending || !codexSelected || !data.wired) return;
    setPending(true);
    try {
      await postJson("/admin/settings/restore", {});
      await refresh();
      setConfirmRestore(false);
    } catch (e) {
      setErr(`还原失败:${e}`);
      setConfirmRestore(false);
    } finally {
      setPending(false);
    }
  };

  return (
    <div className="settings-page management-page">
      <div className="page-heading"><h2 className="page-title">设置</h2></div>
      <section className="panel chatgpt-login-panel" aria-labelledby="chatgpt-login-title">
        <div className="panel-header">
          <h3 id="chatgpt-login-title">ChatGPT 登录来源</h3>
          <button className="ghost" disabled={pending} onClick={async () => {
            setPending(true);
            try { await refresh(); } finally { setPending(false); }
          }}>刷新登录状态</button>
        </div>
        <div className="panel-body">
          <div className="chatgpt-login-controls">
            <label className="field">登录来源
              <select aria-label="ChatGPT 登录来源" value={draftSource} disabled={!data.wired || pending}
                onChange={event => {
                  const next = event.target.value;
                  setDraftSource(next === "codex" || next === "opencode" ? next : "");
                  setSourceSaved(false);
                }}>
                {!draftSource && <option value="">来源未知，请选择</option>}
                <option value="codex">Codex（默认）</option>
                <option value="opencode">OpenCode</option>
              </select>
            </label>
            <button className="button-primary" disabled={!data.wired || pending || !draftSource || draftSource === data.chatgpt_login_source}
              onClick={() => void applySource()}>{savingSource ? "应用中…" : "应用"}</button>
          </div>
          <p className="muted" role="status">
            当前来源：{codexSelected ? "Codex" : openCodeSelected ? "OpenCode" : "未知"} · {data.chatgpt_login_status === "ready" ? "登录凭据已读取，未验证上游授权或额度。"
              : data.chatgpt_login_status === "missing" ? "未找到所选来源的登录凭据，请在对应 CLI 中登录。"
              : "登录来源或凭据暂不可读，请刷新状态；也可明确选择来源后应用。"}
          </p>
          {sourceSaved && <p className="muted" role="status">来源已保存，后续请求跟随此 CLI 的当前登录，不会自动切换到其他来源。</p>}
          <p className="muted">跟随所选 CLI 的当前登录；文件更新无需重启，不会自动切换到其他来源。更改来源不会发送测试请求。</p>
          {openCodeSelected && <p className="muted">OpenCode 始终只读。自动更新仅保存在本应用内；同时在 OpenCode 使用同一登录时可能需要重新登录。</p>}
          <details className="chatgpt-login-path">
            <summary>当前登录文件</summary>
            <p className="muted">{data.chatgpt_login_path ? <code>{data.chatgpt_login_path}</code> : "路径未知，请刷新登录状态。"}</p>
          </details>
          {!data.wired && <p className="muted">设置存储未接线，暂时无法应用登录来源。</p>}
        </div>
      </section>
      <section className="panel">
        <div className="panel-header">
          <h3>与 Codex 共享登录</h3>
          <div className="actions">
            <span className="muted">{codexSelected ? data.share_login ? "已开启" : "已关闭" : `Codex 偏好${data.share_login ? "开启" : "关闭"} · 当前不生效`}</span>
            <label className="switch">
              <input aria-label="与 Codex 共享登录" type="checkbox" disabled={!data.wired || pending || !codexSelected}
                checked={data.share_login} onChange={e => toggleShare(e.target.checked)} />
              <i></i>
            </label>
          </div>
        </div>
        <div className="panel-body">
          <p className="muted">仅对 Codex 来源生效。默认只读取登录信息；开启后，会将刷新后的登录凭据写回 Codex，并保留开启前备份。</p>
          {!codexSelected && <p className="muted">{openCodeSelected ? "OpenCode 始终只读；Codex 共享偏好已保留，切回 Codex 后才生效。" : "当前来源未确认；确认使用 Codex 后可更改共享登录。"}</p>}
          {data.risk && <p className="alert warn" role="status">Codex 共享登录：{data.risk}</p>}
          {!data.wired && <p className="alert warn" role="status">设置存储未接线，暂时无法更改共享登录或还原备份。</p>}
        </div>
      </section>
      <section className="panel">
        <div className="panel-header">
          <h3>Codex 登录凭据备份</h3>
          {data.wired && data.stash && <button disabled={pending || !codexSelected} onClick={() => setConfirmRestore(true)}>还原备份</button>}
        </div>
        <div className="panel-body">
          <p className="muted">{data.stash ? `Codex 开启共享前备份 · ${fmtTime(data.stash.mtime)}` : "暂无 Codex 备份；开启 Codex 共享登录时会自动创建。"}</p>
          {!codexSelected && data.stash && <p className="muted">备份仅用于恢复 Codex 登录；请切回 Codex 后还原。</p>}
          <details className="section-details">
            <summary>备份详情</summary>
            <p className="muted">{data.wired ? "设置存储已接线。" : "独立网关默认设置路径：~/.aubridge/config.json"}</p>
            {data.stash && <p className="muted">SHA-256：<code>{data.stash.sha256}</code></p>}
          </details>
        </div>
      </section>
      {confirmRestore && <Dialog labelledBy="restore-settings-title" role="alertdialog" busy={pending} onClose={() => setConfirmRestore(false)}>
        <div className="modal-header"><h2 id="restore-settings-title">还原 Codex 登录凭据备份？</h2></div>
        <div className="modal-body"><p>将 Codex 登录凭据恢复为开启共享登录前的备份。当前 Codex 凭据文件会被备份内容替换，不更改 OpenCode 登录文件。</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={pending} onClick={() => setConfirmRestore(false)}>取消</button>
          <button className="button-danger" disabled={pending} onClick={() => void restoreStash()}>{pending ? "还原中…" : "确认还原"}</button>
        </div>
      </Dialog>}
    </div>
  );
}

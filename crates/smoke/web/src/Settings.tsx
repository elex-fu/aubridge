import { useEffect, useRef, useState } from "react";
import { BuildInfo, SettingsPage, fetchJson, postJson } from "./api";
import { fmtTime } from "./format";
import { Dialog } from "./Dialog";
import { Pricing } from "./Pricing";
import { WorkConfigBackup } from "./WorkConfigBackup";
import { UpdatePanel } from "./UpdatePanel";
import { TaskView } from "./TaskView";
import "./SettingsPresentation.css";

const CATEGORIES = [
  ["connections", "登录与连接"], ["backup", "备份与恢复"],
  ["pricing", "计价设置"], ["updates", "版本与更新"],
] as const;
type SettingsCategory = typeof CATEGORIES[number][0];

const CAPABILITIES: Record<string, string> = {
  messages: "Messages 请求", chat: "Chat 请求", "responses-http": "Codex HTTP 请求",
  "quota-windows": "后台额度窗口", "model-directory": "厂商模型目录", "request-limits": "长请求保护",
  "cache-usage": "缓存用量", "price-snapshots": "价格快照", "request-id": "请求诊断", "opencode-cli-test": "OpenCode 测试",
  "codex-compaction": "Codex 原生压缩与续聊",
  "claude-cli-transport": "Claude 订阅 CLI 通道（默认关闭，待实机验证）",
  "session-diagnostics": "会话诊断与缓存估值",
  "media-preflight": "图片文件透传与能力预检",
  "benchmark-snapshots": "独立用户对标价与历史快照",
};

export function Settings({ setErr, onOpenSources, onOpenAgents }: { setErr: (s: string) => void; onOpenSources: () => void; onOpenAgents: () => void }) {
  const [data, setData] = useState<SettingsPage | null>(null);
  const [pending, setPending] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [confirmRestore, setConfirmRestore] = useState(false);
  const [draftSource, setDraftSource] = useState<"codex" | "opencode" | "">("");
  const [savingSource, setSavingSource] = useState(false);
  const [sourceSaved, setSourceSaved] = useState(false);
  const [build, setBuild] = useState<BuildInfo | null>(null);
  const [buildError, setBuildError] = useState("");
  const infoTrigger = useRef<HTMLElement | null>(null);
  const [infoTask, setInfoTask] = useState<"login" | "share" | "backup" | "build" | null>(null);
  const [infoVisited, setInfoVisited] = useState<Record<string, boolean>>({});
  const [category, setCategory] = useState<SettingsCategory>("connections");
  const [backupTaskActive, setBackupTaskActive] = useState(false);
  const [updateTaskActive, setUpdateTaskActive] = useState(false);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [backupBusy, setBackupBusy] = useState(false);
  const [pricingBusy, setPricingBusy] = useState(false);
  const [updateError, setUpdateError] = useState("");
  const [backupError, setBackupError] = useState("");
  const loadBuild = () => fetchJson<BuildInfo>("/admin/build").then(info => { setBuild(info); setBuildError(""); })
    .catch(error => setBuildError(String(error).includes("404") ? "当前网关版本尚未提供构建信息。" : "构建信息暂不可读。"));

  const load = () => fetchJson<SettingsPage>("/admin/settings").then(value => {
    setData(value);
    setDraftSource(value.chatgpt_login_source === "codex" || value.chatgpt_login_source === "opencode" ? value.chatgpt_login_source : "");
    setLoadFailed(false);
  });
  useEffect(() => {
    void load().catch(e => { setLoadFailed(true); setErr(String(e)); });
    void loadBuild();
  }, [setErr]);

  useEffect(() => {
    if (updateBusy && !backupBusy && !pending && !confirmRestore && !pricingBusy) setCategory("updates");
    else if (backupBusy && !updateBusy && !pending && !confirmRestore && !pricingBusy) setCategory("backup");
  }, [updateBusy, backupBusy, pending, confirmRestore, pricingBusy]);

  useEffect(() => { if (updateBusy || backupBusy) setInfoTask(null); }, [updateBusy, backupBusy]);

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
    if (pending || backupBusy || updateBusy || pricingBusy || !codexSelected || !data.wired) return;
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

  const closeInfo = () => { setInfoTask(null); window.requestAnimationFrame(() => infoTrigger.current?.focus()); };
  const categoryLocked = pending || confirmRestore || updateBusy || backupBusy || pricingBusy;
  const openInfo = (task: "login" | "share" | "backup" | "build") => {
    if (categoryLocked || task === "backup" && backupTaskActive || task === "build" && updateTaskActive) return;
    infoTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setInfoVisited(current => ({ ...current, [task]: true })); setInfoTask(task);
  };
  const openStashRestore = () => {
    if (categoryLocked || backupTaskActive || !codexSelected || !data.wired) return;
    setConfirmRestore(true);
  };
  return (
    <div className="settings-page management-page settings-sections settings-presentation">
      <div className="page-heading"><h2 className="page-title">设置</h2></div>
      <nav className="settings-nav" aria-label="设置分类">
        {CATEGORIES.map(([id, label]) => <button key={id} id={`settings-category-${id}`}
          type="button" aria-pressed={category === id} aria-controls={`settings-content-${id}`}
          disabled={categoryLocked && category !== id} onClick={() => setCategory(id)}>{label}</button>)}
      </nav>
      {categoryLocked && <p className="settings-category-notice muted" role="status">请先完成当前操作或关闭确认窗口，再切换设置分类。</p>}
      {(category !== "backup" || infoTask === "backup") && backupError && <p className="settings-category-notice alert error" role="alert">
        备份与恢复：{backupError}<button className="ghost" disabled={categoryLocked} onClick={() => { setInfoTask(null); setCategory("backup"); }}>查看</button>
      </p>}
      {(category !== "updates" || infoTask === "build") && (updateError || buildError) && <p className="settings-category-notice alert error" role="alert">
        版本与更新：{updateError || buildError}<button className="ghost" disabled={categoryLocked} onClick={() => { setInfoTask(null); setCategory("updates"); }}>查看</button>
      </p>}
      <div id="settings-content-connections" className="settings-content" role="region" aria-labelledby="settings-category-connections" hidden={category !== "connections"}>
        <div className="task-workspace-pane" hidden={infoTask === "login" || infoTask === "share"}>
        <div className="settings-login-workspace">
      <section className="panel chatgpt-login-panel" aria-labelledby="chatgpt-login-title">
        <div className="panel-header">
          <h3 id="chatgpt-login-title">ChatGPT 登录来源</h3>
          <button className={data.chatgpt_login_status !== "ready" && draftSource === data.chatgpt_login_source ? "button-primary" : "ghost"} disabled={pending} onClick={async () => {
            setPending(true);
            try { await refresh(); } finally { setPending(false); }
          }}>刷新登录状态</button>
        </div>
        <div className="panel-body">
          <p className="settings-current-state" role="status">
            当前来源：{codexSelected ? "Codex" : openCodeSelected ? "OpenCode" : "未知"} · {data.chatgpt_login_status === "ready" ? "凭据已读取；上游授权与额度未验证。"
              : data.chatgpt_login_status === "missing" ? "未找到所选来源的登录凭据，请在对应 CLI 中登录。"
              : "登录来源或凭据暂不可读，请刷新状态；也可明确选择来源后应用。"}
          </p>
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
            <button className={draftSource && draftSource !== data.chatgpt_login_source ? "button-primary" : "ghost"} disabled={!data.wired || pending || !draftSource || draftSource === data.chatgpt_login_source}
              onClick={() => void applySource()}>{savingSource ? "应用中…" : "应用"}</button>
          </div>
          {draftSource === data.chatgpt_login_source && data.chatgpt_login_status === "ready" && <div className="settings-next-action"><span className="muted">下一步：核对模型源与费用范围，再验证工具。</span><button className="button-primary" disabled={categoryLocked} onClick={onOpenSources}>去模型源核对</button></div>}
          {draftSource === data.chatgpt_login_source && data.chatgpt_login_status !== "ready" && <p className="muted">下一步：{data.chatgpt_login_status === "missing" ? "在当前 CLI 登录后，刷新登录状态。" : "刷新登录状态，或明确选择来源后应用。"}</p>}
          {sourceSaved && <p className="muted" role="status">来源已保存，后续请求跟随此 CLI 的当前登录，不会自动切换到其他来源。</p>}
          {openCodeSelected && <p className="muted">OpenCode 始终只读。自动更新仅保存在本应用内；同时在 OpenCode 使用同一登录时可能需要重新登录。</p>}
          <button className="ghost" disabled={categoryLocked} onClick={() => openInfo("login")}>查看登录文件与同步说明</button>
          {!data.wired && <p className="muted">设置存储未接线，暂时无法应用登录来源。</p>}
        </div>
      </section>
      <div className="settings-secondary-status">
        <p>与 Codex 共享登录 · <span className="muted">{codexSelected ? data.share_login ? "已开启" : "已关闭" : `Codex 偏好${data.share_login ? "开启" : "关闭"} · 当前不生效`}</span></p>
        <button className="ghost" disabled={categoryLocked} onClick={() => openInfo("share")}>共享登录说明与设置</button>
      </div>
        </div>
        </div>
        {infoVisited.share && <div className="task-workspace-pane" hidden={infoTask !== "share"}><TaskView title="与 Codex 共享登录" onBack={closeInfo} busy={categoryLocked}>
          <p>默认只读。开启后将更新的登录凭据写回 Codex，并先保存凭据备份。</p>
          <p className="muted">进入此说明不会更改设置；下方开关会立即应用你的选择。</p>
          {!codexSelected && <p className="muted">{openCodeSelected ? "OpenCode 始终只读；Codex 共享偏好已保留，切回 Codex 后才生效。" : "当前来源未确认；确认使用 Codex 后可更改共享登录。"}</p>}
          {data.risk && <p className="alert warn" role="status">Codex 共享登录：{data.risk}</p>}
          {!data.wired && <p className="alert warn" role="status">设置存储未接线，暂时无法更改共享登录或还原备份。</p>}
          <div className="settings-share-control"><span>与 Codex 共享登录 · {data.share_login ? "已开启" : "已关闭"}</span>
            <label className="switch">
              <input aria-label="与 Codex 共享登录" type="checkbox" disabled={!data.wired || pending || !codexSelected}
                checked={data.share_login} onChange={e => toggleShare(e.target.checked)} />
              <i></i>
            </label></div>
        </TaskView></div>}
        {infoVisited.login && <div className="task-workspace-pane" hidden={infoTask !== "login"}><TaskView title="登录文件与同步说明" onBack={closeInfo} busy={categoryLocked}><p>当前来源：{codexSelected ? "Codex" : openCodeSelected ? "OpenCode" : "未知"}</p><p className="muted">跟随此 CLI 的当前登录，文件更新无需重启；不会自动切换来源，更改选择不会发送测试请求。</p><p>{data.chatgpt_login_path ? <code>{data.chatgpt_login_path}</code> : "路径未知，请返回后刷新登录状态。"}</p>{data.risk && <p className="alert warn" role="status">Codex 共享登录：{data.risk}</p>}{!data.wired && <p className="alert warn" role="status">设置存储未接线，暂时无法更改登录与共享设置。</p>}</TaskView></div>}
      </div>
      <div id="settings-content-backup" className="settings-content settings-backup-content" role="region" aria-labelledby="settings-category-backup" hidden={category !== "backup"}>
        <div className="task-workspace-pane" hidden={infoTask === "backup"}>
        <WorkConfigBackup onChanged={() => void refresh()} onOpenSources={onOpenSources} onOpenAgents={onOpenAgents}
          onBusyChange={setBackupBusy} onErrorChange={setBackupError} onTaskActiveChange={setBackupTaskActive} />
      <section className="panel task-workspace-pane" hidden={backupTaskActive}>
        <div className="panel-header">
          <h3>Codex 登录凭据备份</h3>
          {data.wired && data.stash && <button disabled={categoryLocked || backupTaskActive || !codexSelected} onClick={openStashRestore}>还原备份</button>}
        </div>
        <div className="panel-body">
          <p className="muted">{data.stash ? `Codex 开启共享前备份 · ${fmtTime(data.stash.mtime)}` : "暂无 Codex 备份；开启 Codex 共享登录时会自动创建。"}</p>
          {!codexSelected && data.stash && <p className="muted">备份仅用于恢复 Codex 登录；请切回 Codex 后还原。</p>}
          <button className="ghost" disabled={categoryLocked} onClick={() => openInfo("backup")}>查看凭据备份信息</button>
        </div>
      </section>
        </div>
        {infoVisited.backup && <div className="task-workspace-pane" hidden={infoTask !== "backup"}><TaskView title="Codex 凭据备份信息" onBack={closeInfo} busy={categoryLocked}><p>{data.wired ? "设置存储已接线。" : "独立网关默认设置路径：~/.aubridge/config.json"}</p>{data.stash ? <><p>备份时间：{fmtTime(data.stash.mtime)}</p><p>SHA-256：<code>{data.stash.sha256}</code></p></> : <p>当前没有 Codex 凭据备份。</p>}</TaskView></div>}
      </div>
      <div id="settings-content-pricing" className="settings-content" role="region" aria-labelledby="settings-category-pricing" hidden={category !== "pricing"}>
        <div className="settings-category-summary"><h3>估值模式：目录价格与独立用户对标</h3><p className="muted">缺少价格时保持未定价；API 等值不代表实际付款或现金节省。费用许可在模型源单独确认。</p></div>
        <Pricing setErr={setErr} onBusyChange={setPricingBusy} />
      </div>
      <div id="settings-content-updates" className="settings-content" role="region" aria-labelledby="settings-category-updates" hidden={category !== "updates"}>
        <div className="task-workspace-pane" hidden={infoTask === "build"}>
      <section className="panel task-workspace-pane" aria-labelledby="build-info-title" hidden={updateTaskActive}>
        <div className="panel-header"><h3 id="build-info-title">当前运行版本</h3>
          <button className="ghost" onClick={() => void loadBuild()}>刷新版本</button></div>
        <div className="panel-body">
          {build ? <>
            <p>{build.version} · {build.mode === "development" ? "开发包" : build.mode === "distribution" ? "发行构建" : "源码运行"}</p>
            <button className="ghost" disabled={categoryLocked} onClick={() => openInfo("build")}>查看构建身份与能力</button>
          </> : <p className="muted" role="status">{buildError || "正在读取运行版本…"}</p>}
          {build && buildError && <p className="alert error" role="alert">{buildError}</p>}
        </div>
      </section>
      <UpdatePanel onBusyChange={setUpdateBusy} onErrorChange={setUpdateError} onTaskActiveChange={setUpdateTaskActive} />
        </div>
        {infoVisited.build && <div className="task-workspace-pane" hidden={infoTask !== "build"}><TaskView title="当前构建身份与能力" onBack={closeInfo} busy={categoryLocked}>{build ? <><p>版本 {build.version} · {build.profile}</p><p>构建提交：<code>{build.git_revision || "未知"}</code> · 进程 {build.pid}</p><p>{build.source_verified ? "此包来自通过源码门禁的固定检出。" : "此构建未附带干净检出验收标记。"}</p><p className="muted">{build.capabilities.map(capability => CAPABILITIES[capability] || capability).join(" · ")}</p></> : <p>{buildError || "构建信息尚未读取。"}</p>}</TaskView></div>}
      </div>
      {confirmRestore && <Dialog labelledBy="restore-settings-title" role="alertdialog" busy={pending} onClose={() => setConfirmRestore(false)}>
        <div className="modal-header"><h2 id="restore-settings-title">还原 Codex 登录凭据备份？</h2></div>
        <div className="modal-body"><p>将 Codex 登录凭据恢复为开启共享登录前的备份。当前 Codex 凭据文件会被备份内容替换，不更改 OpenCode 登录文件。</p></div>
        <div className="modal-actions">
          <button autoFocus disabled={pending} onClick={() => setConfirmRestore(false)}>取消</button>
          <button className="button-danger" disabled={pending || backupBusy || updateBusy || pricingBusy} onClick={() => void restoreStash()}>{pending ? "还原中…" : "确认还原"}</button>
        </div>
      </Dialog>}
    </div>
  );
}

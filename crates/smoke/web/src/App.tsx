import { useState } from "react";
import appIcon from "../../icons/app-icon.svg";
import { Agents } from "./Agents";
import { DialogErrorContext } from "./Dialog";
import { Models } from "./Models";
import { Plugins } from "./Plugins";
import { Providers } from "./Providers";
import { Proxy } from "./Proxy";
import { Settings } from "./Settings";

// 六页导航(ui-design §5 v1.1 + 用户拍板 2026-11-06:设置页独立,
// 最右):仪表盘并入 Agents;设置页 = 共享登录开关 + stash 备份
// 还原(凭据回写高级区,原暂驻 Provider 页)。Tab 跳转通道:
// 引导卡/失败行 → Provider。
type Tab = "agents" | "providers" | "models" | "proxy" | "plugins" | "settings";

const TABS: { key: Tab; label: string }[] = [
  { key: "agents", label: "Agents" },
  { key: "providers", label: "模型源" },
  { key: "models", label: "模型" },
  { key: "proxy", label: "代理" },
  { key: "plugins", label: "插件" },
  { key: "settings", label: "设置" },
];

export function App() {
  const [tab, setTab] = useState<Tab>("agents");
  const [err, setErr] = useState("");
  return (
    <DialogErrorContext.Provider value={{ message: err, clear: () => setErr("") }}>
      <div className="app-shell">
        <header className="app-header">
          <h1 className="brand"><img src={appIcon} alt="" width="24" height="24" />aubridge</h1>
          <nav className="app-nav" aria-label="主导航">
            {TABS.map(t => (
              <button key={t.key} className={tab === t.key ? "on" : ""} aria-current={tab === t.key ? "page" : undefined} onClick={() => setTab(t.key)}>
                {t.label}
              </button>
            ))}
          </nav>
        </header>
        {err && (
          <div className="alert error app-error" role="alert">
            <div className="app-error-message">{err}</div>
            <button className="ghost" aria-label="关闭错误提示" onClick={() => setErr("")}>关闭</button>
          </div>
        )}
        <main className={`app-main ${tab}-view`}>
          {tab === "agents" && <Agents setErr={setErr} go={setTab} />}
          {tab === "providers" && <Providers setErr={setErr} />}
          {tab === "models" && <Models setErr={setErr} />}
          {tab === "proxy" && <Proxy setErr={setErr} go={setTab} />}
          {tab === "plugins" && <Plugins setErr={setErr} />}
          {tab === "settings" && <Settings setErr={setErr} />}
        </main>
      </div>
    </DialogErrorContext.Provider>
  );
}

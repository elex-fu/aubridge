import { useCallback, useEffect, useRef, useState } from "react";
import { CostIntent, CostSettings, HealthProvider, fetchJson, postJson } from "./api";

export function CostIntentPanel({ providers, onSaved, onBusyChange }: { providers: HealthProvider[]; onSaved: () => void; onBusyChange?: (busy: boolean) => void }) {
  const [settings, setSettings] = useState<CostSettings | null>(null);
  const [error, setError] = useState(false);
  const [revision, setRevision] = useState(0);
  const [editorBusy, setEditorBusy] = useState(false);
  const reportBusy = useCallback((busy: boolean) => { setEditorBusy(busy); onBusyChange?.(busy); }, [onBusyChange]);
  useEffect(() => {
    let active = true;
    setError(false);
    fetchJson<CostSettings>("/admin/settings").then(value => { if (active) setSettings(value); })
      .catch(() => { if (active) setError(true); });
    return () => { active = false; };
  }, [revision]);
  const key = JSON.stringify([settings?.cost_intent, settings?.subscription_authorizations, settings?.cost_policy_issue, providers.map(provider => [provider.id, provider.source_health?.cost.connection_identity])]);
  return <section className="panel" aria-labelledby="cost-intent-title">
    <div className="panel-heading"><h3 id="cost-intent-title">费用范围</h3><button className="ghost" disabled={editorBusy} onClick={() => setRevision(value => value + 1)}>重新读取费用选择</button></div>
    {error ? <p role="alert">费用选择暂不可读取，请重新读取后继续。</p> : !settings ? <p role="status">正在读取费用选择…</p> :
      <CostIntentEditor key={key} onBusyChange={reportBusy} settings={settings} providers={providers} onSaved={() => { setRevision(value => value + 1); onSaved(); }} />}
  </section>;
}

export function CostIntentEditor({ settings, providers, onSaved, compact = false, disabled = false, onBusyChange }: {
  settings: CostSettings; providers: HealthProvider[]; onSaved: () => void; compact?: boolean; disabled?: boolean; onBusyChange?: (busy: boolean) => void;
}) {
  const [intent, setIntent] = useState<CostIntent | "">(settings.cost_intent || "");
  const eligible = providers.filter(provider => provider.source_health?.cost.subscription_eligible);
  const [authorized, setAuthorized] = useState<string[]>(() => eligible.filter(provider => (settings.subscription_authorizations || []).some(auth =>
    auth.provider === provider.id && auth.connection_identity === provider.source_health?.cost.connection_identity,
  )).map(provider => provider.id));
  const identities = useRef(new Map(eligible.map(provider => [provider.id, provider.source_health!.cost.connection_identity])));
  const isAuthorized = (provider: HealthProvider) => authorized.includes(provider.id) &&
    identities.current.get(provider.id) === provider.source_health?.cost.connection_identity;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const pending = useRef(false);
  useEffect(() => { onBusyChange?.(busy); return () => onBusyChange?.(false); }, [busy, onBusyChange]);
  const save = async () => {
    if (!intent || disabled || pending.current) return;
    pending.current = true; setBusy(true); setError(""); setSaved(false);
    const subscription_authorizations = eligible.filter(isAuthorized).map(provider => ({
      provider: provider.id, basis: provider.source_health?.cost.subscription_billing_notice === "kimi_vendor_extra_usage" ? "user_declared" : "personal_channel", connection_identity: provider.source_health!.cost.connection_identity,
    }));
    try {
      await postJson("/admin/settings", { cost_intent: intent, subscription_authorizations });
      setSaved(true); onSaved();
    } catch { setError("费用选择未保存，请重新读取来源状态后再试。"); }
    finally { pending.current = false; setBusy(false); }
  };
  return <div className={compact ? "cost-intent compact" : "cost-intent"}>
    {settings.cost_policy_issue ? <p className="alert error" role="alert">费用配置读取异常，自动推理已停止。请核对并修正原配置；损坏文档不会被自动覆盖。重新读取后明确选择费用范围。</p> :
      <p className="muted" role="status">{settings.cost_intent === "confirmed_free_only" ? "当前：只用确认免费" : settings.cost_intent === "free_then_subscription" ? "当前：免费优先，可用我的订阅" : "尚未选择，沿用原路由；尚未启用严格费用策略。"}</p>}
    <fieldset disabled={busy || disabled}>
      <legend>选择自动推理的费用范围</legend>
      <label><input type="radio" name={compact ? "first-cost-intent" : "source-cost-intent"} checked={intent === "confirmed_free_only"} onChange={() => { setIntent("confirmed_free_only"); setSaved(false); }} />只用确认免费</label>
      <label><input type="radio" name={compact ? "first-cost-intent" : "source-cost-intent"} checked={intent === "free_then_subscription"} onChange={() => { setIntent("free_then_subscription"); setSaved(false); }} />免费优先，可用我的订阅</label>
    </fieldset>
    {!compact && <p className="muted">确认免费须有新鲜的可信价格依据，手填免费标签与未知价格均不算。保存仅授权下面勾选的专属个人订阅通道，不会自动使用普通按量 API。</p>}
    {intent === "free_then_subscription" && <div className="subscription-authorizations">
      {eligible.map(provider => <div key={provider.id}><label><input type="checkbox" disabled={busy || disabled} checked={isAuthorized(provider)} onChange={event => {
        identities.current.set(provider.id, provider.source_health!.cost.connection_identity);
        setAuthorized(current => event.target.checked ? [...new Set([...current, provider.id])] : current.filter(id => id !== provider.id)); setSaved(false);
      }} />{provider.source_health?.cost.subscription_billing_notice === "kimi_vendor_extra_usage" ? "我已购买此个人订阅，允许使用" : "允许使用我的专属订阅通道"} · {provider.id}{provider.paused ? "（已暂停，不会自动恢复）" : ""}</label>
        {provider.source_health?.cost.subscription_billing_notice === "kimi_vendor_extra_usage" && <p className="muted">若你在 Kimi 厂商侧开启额外用量，套餐耗尽可能自动扣额外余额；此选择不保证仅使用套餐内额度。</p>}
      </div>)}
      {!eligible.length && <p className="muted">尚未发现可授权的专属个人订阅通道，普通 API 不会自动作为订阅使用。</p>}
      {eligible.length > 0 && !eligible.some(isAuthorized) && <p className="muted">尚未授权订阅；保存后仍只能尝试符合免费依据的模型。</p>}
    </div>}
    {compact && <p className="muted">未知价格不算确认免费；普通按量 API 不会自动当作订阅。</p>}
    {error && <p className="alert error" role="alert">{error}</p>}
    {saved && <p role="status">费用选择已保存，后续自动推理按此范围筛选；不会自动恢复已暂停的来源。</p>}
    <button className="button-primary" disabled={busy || disabled || !intent} onClick={() => void save()}>{busy ? "保存中…" : "保存费用选择"}</button>
  </div>;
}

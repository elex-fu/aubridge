import { HealthProvider } from "./api";
import { fmtRouteReason, fmtTime } from "./format";

export function SourceHealthFacts({ provider }: { provider: HealthProvider }) {
  const health = provider.source_health;
  return <section className="source-health-facts" aria-label={`${provider.id} 的独立健康事实`}>
    <h3 className="section-label">来源事实</h3>
    {!health ? <p className="muted">健康事实尚不可读取，不能据此判断模型可用。</p> : <>
      <p>凭据：{health.credential.required === false ? "无需凭据" : health.credential.present ? "已发现" : "未发现"}{health.credential.required === false ? "；模型可用性仍需实际请求确认" : health.credential.usable ? " · 当前凭据未被本机路由阻止；上游授权仍需实际请求确认" : " · 当前凭据尚不可用于自动路由"}</p>
      <p>自动推理：{health.pending_rebind ? "待重新绑定凭据" : health.paused ? "此来源已暂停" : "未暂停；仍受费用范围与模型路由限制"}</p>
      <p>最近来源短检：{health.last_check ? `${fmtRouteReason(health.last_check.status)}${health.last_check.model ? ` · ${health.last_check.model}` : ""}${health.last_check.observed_at ? ` · ${fmtTime(health.last_check.observed_at)}` : " · 时间未知"}` : "尚无可归属的记录"}</p>
      <p>最近真实请求成功：{health.last_real_success_at == null ? "未知" : fmtTime(health.last_real_success_at)}</p>
      <p>模型目录：{health.directory.fetched_at == null ? "更新时间未知" : fmtTime(health.directory.fetched_at)} · {health.directory.fetched_at == null ? "有效性未知" : health.directory.stale ? "已过期" : "当前有效"}{health.directory.issue ? " · 最近读取失败，保留已有事实" : ""}</p>
      <p>费用依据：{health.cost.basis === "confirmed_free" ? "此来源存在符合确认免费依据的模型，不代表全部模型免费" : health.cost.basis === "authorized_subscription" ? "已明确授权专属个人订阅通道；套餐与剩余额度须另行确认" : "尚无确认免费或已授权订阅依据"}</p>
      <p className="muted">以上事实相互独立。凭据、目录或额度存在，均不代表文件工具任务已通过。显式短检可能使用额度或产生费用，不会修改自动推理的费用选择。</p>
    </>}
  </section>;
}

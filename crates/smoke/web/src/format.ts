export function fmtTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts * 1000).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function fmtDuration(ms: number | null | undefined): string {
  if (ms == null) return "—";
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`;
}

export function fmtUtil(p: { util_5h?: number | null; util_7d?: number | null }): string {
  // 百分比 0–100;null = 未知(§8.2 被动采样/主动查询都未覆盖)。
  const f = (v: number | null | undefined) => (v == null ? "—" : `${v.toFixed(0)}%`);
  return `${f(p.util_5h)} / ${f(p.util_7d)}`;
}

const ROUTE_REASONS: Record<string, string> = {
  "ok": "成功",
  "eligible": "可尝试，剩余请求数未知",
  "missing-credential": "缺凭据",
  "credential-rejected": "凭据被拒绝",
  "access-denied": "上游拒绝访问",
  "tools-unsupported": "模型不支持工具",
  "model-not-listed": "厂商当前目录未列出此模型",
  "output-limit-exceeded": "请求输出超过模型上限",
  "reasoning-unsupported": "模型未提供推理能力",
  "request-too-large": "请求体超过本机容量",
  "invalid-body": "请求体无法读取",
  "invalid-encoding": "压缩请求体无效",
  "unsupported-encoding": "不支持该请求编码",
  "body-timeout": "请求体读取超时",
  "gateway-busy": "本机入口容量已满",
  "native-protocol-required": "请求需要与客户端相同的协议",
  "free-route-restricted": "请求超出内置免费源范围",
  "context-window-too-small": "估算超出模型上下文窗口",
  "context-fallback-window": "上下文超限后，候选没有足够大的已知窗口",
  "model-rate-limit": "模型限流",
  "model-quota-exhausted": "模型额度耗尽",
  "model-unavailable": "模型不可用",
  "model-unlisted": "模型已下架",
  "account-rate-limit": "账号限流",
  "account-quota-exhausted": "账号额度耗尽",
  "account-credit-exhausted": "账号余额不足",
  "provider-cooldown": "源冷却中",
  "provider-overloaded": "上游暂时过载",
  "rate-limited": "等待限流或额度恢复",
  "circuit-open": "连接故障冷却中",
  "provider-disabled": "源不可用",
  "provider-changed": "源连接配置已变化",
  "invalid-provider-url": "上游地址不合法或超出声明域名",
  "provider-paused": "用户已暂停推理",
  "subscription-excluded": "仅免费档分组跳过订阅",
  "cost-unconfirmed-free": "缺少新鲜的可信免费价格依据，请核对目录与费用范围",
  "cost-subscription-not-authorized": "尚未授权此专属订阅通道，请到模型源明确选择",
  "cost-policy-invalid": "费用配置读取异常，自动推理已停止，请核对原配置",
  "update-maintenance-active": "正在准备更新，请稍后再试",
  "update-busy": "仍有任务运行，请在任务完成后重试更新",
  "provider-busy": "源繁忙",
  "retry-budget": "已达重试预算",
  "response-budget": "首段等待预算已用完",
  "response-timeout": "请求等待超时",
  "context_length_exceeded": "上下文超限",
  "invalid_request": "请求无效",
  "request_too_large": "请求体过大",
  "upstream-error": "上游错误",
  "network-error": "连接失败",
  "no-candidate": "无可用源",
  "model_not_found": "模型未配置或已移除",
  "cancelled": "已取消",
  "incomplete": "响应未完成",
  "stream-error": "流错误",
  "empty-stream": "空流",
  "first-content-timeout": "首段超时",
};

export function fmtRouteReason(reason: string): string {
  return ROUTE_REASONS[reason] ?? reason;
}

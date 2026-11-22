import type { AgentRow } from "./api";

export type AgentTarget = "connection" | "private" | "project";
export type AgentEntry = { target: AgentTarget; sequence: number };
export type AgentPrerequisite = { agentId: string; agentName: string; task: "validation" };

export const cliInstallations: Record<string, string> = {
  "claude-code": "https://code.claude.com/docs/en/setup",
  codex: "https://developers.openai.com/codex/cli/",
  opencode: "https://opencode.ai/docs/",
  gemini: "https://geminicli.com/docs/get-started/installation/",
};

// Display copy only; the gateway remains authoritative for version support.
const fileValidationVersions: Record<string, string> = {
  "claude-code": "2.1.325",
  codex: "0.169.0-alpha.11.1 或 0.169.1",
  opencode: "2.2.5",
  gemini: "0.68.0 或 0.70.0",
};

export function cliPreparation(agent: AgentRow): { message: string; action: string; ready: boolean } {
  switch (agent.discovery?.status) {
    case "ready": return { message: "CLI 可执行文件与版本已确认", action: "重新检测 CLI 与版本", ready: true };
    case "not_found": return { message: "未找到此 CLI 的可执行文件，请查看官方安装说明后重新检测", action: "查看 CLI 安装准备", ready: false };
    case "not_executable": return { message: "找到了 CLI 文件，但当前不可执行；请按官方说明核对安装", action: "查看 CLI 安装准备", ready: false };
    case "version_unsupported": return { message: `当前 CLI 版本尚不支持此文件验证。本版文件验证支持 ${agent.name} ${fileValidationVersions[agent.id] || "已确认的精确版本"}；可保留当前 CLI 并手动配置连接，重新检测只读取版本号`, action: "查看 CLI 版本说明", ready: false };
    case "version_unknown": return { message: "尚不能确认 CLI 版本；不能据此判断工具未安装", action: "重新检测 CLI 与版本", ready: false };
    default: return { message: "尚未检查此 CLI 的可执行文件与版本", action: "检测 CLI 与版本", ready: false };
  }
}

export function validationFailureGuide(reason?: string | null): { message: string; action: string; target: "cli" | "conditions" } {
  switch (reason) {
    case "cli_failed": return { message: "本次 CLI 执行未完成；先核对安装、版本与连接条件，不能据此判断具体上游故障", action: "核对 CLI 与版本", target: "cli" };
    case "version_unknown": case "version_unsupported": return { message: "本次未能使用受支持的 CLI 完成验证，请核对版本与官方说明", action: "核对 CLI 与版本", target: "cli" };
    case "not_wired": case "identity_changed": return { message: "本次连接条件未确认或已经改变，请重新读取；不会自动改写接线或重新运行", action: "重新读取连接条件", target: "conditions" };
    case "session_unverified": return { message: "本次未能核实 CLI 会话与请求归属，不能作为文件验证通过证据", action: "查看文件任务条件", target: "conditions" };
    case "file_task_unverified": return { message: "本次未核实完整文件工具证据；不能将文本回复或 HTTP 成功当作文件任务通过", action: "查看文件任务条件", target: "conditions" };
    case "timed_out": return { message: "本次验证已超时终止，未证明文件工具任务完成；先核对当前条件", action: "查看文件任务条件", target: "conditions" };
    default: return { message: "本次验证已终止且未通过；请核对本任务条件与实际原因后再决定是否新运行", action: "查看文件任务条件", target: "conditions" };
  }
}

// Read-only UI decisions. No click represented here starts a CLI or saves settings.
export function agentStep(agent: AgentRow, confirmed: boolean): {
  status: string; tone: string; action: string; target: AgentTarget; refresh?: boolean;
} {
  const validation = agent.validation;
  const scope = validation?.scope === "project" ? "原项目" : "示例目录";
  const target = validation?.scope === "project" ? "project" : "private";
  if (!confirmed) return { status: "连接状态尚未确认", tone: "off", action: "重新读取状态", target: "connection", refresh: true };
  if (validation?.status === "running" || validation?.status === "queued") return { status: `${scope}文件任务进行中`, tone: "off", action: "查看任务进度", target };
  if (validation?.last_job_id && validation.status !== "not_run") {
    if (validation.status === "passed" && validation.valid && agent.wired && (!agent.drift || agent.drift === "clean")) {
      if (validation.scope === "project") return { status: `原项目验证有效${validation.project_alias ? ` · ${validation.project_alias}` : ""}`, tone: "success", action: "查看配置与启动", target: "connection" };
      if (validation.scope === "private_probe") return { status: "示例目录已验证 · 原项目尚未验证", tone: "success", action: "检查原项目", target: "project" };
    }
    return { status: validation.status === "passed" ? `${scope}历史验证需重新核对` : `${scope}任务${validation.status === "cancelled" ? "已取消" : validation.status === "timed_out" ? "已超时" : "未通过"}`, tone: "warn", action: "查看原任务与下一步", target };
  }
  if (validation && validation.status !== "not_run") return { status: "验证记录缺少可恢复的任务 ID", tone: "warn", action: "核对验证记录", target };
  const cli = cliPreparation(agent);
  if (!cli.ready) {
    const labels: Record<string, string> = { not_found: "未找到 CLI 可执行文件", not_executable: "CLI 文件当前不可执行", version_unknown: "CLI 版本尚未确认", version_unsupported: "当前版本暂不支持文件验证" };
    return { status: labels[agent.discovery?.status || ""] || "CLI 与版本尚未检查", tone: agent.discovery?.status === "not_checked" || !agent.discovery ? "off" : "warn", action: cli.action, target: "private" };
  }
  if (agent.drift === "replaced") return { status: "接线字段被第三方改写", tone: "bad", action: "核对当前接线", target: "connection" };
  if (agent.drift === "port-changed") return { status: "网关端口已改变，需重新连接", tone: "warn", action: "更新连接设置", target: "connection" };
  if (!agent.wired) return { status: agent.detected ? "尚未连接" : "尚未发现客户端配置", tone: "off", action: "开始设置", target: "connection" };
  return { status: "已连接 · 尚待文件工具验证", tone: "off", action: "准备文件验证", target: "private" };
}

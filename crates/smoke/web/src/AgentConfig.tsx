import { useEffect, useState } from "react";
import { AgentConfigPage, AgentRow, AgentTestResult, fetchJson, postJson } from "./api";
import { CopyBtn } from "./CopyButton";
import { Dialog } from "./Dialog";

type Tab = "form" | "json" | "test";
type JsonObject = Record<string, unknown>;

const ROLE_IDS = ["sonnet", "opus", "haiku", "fable"] as const;
const ROLE_LABELS: Record<(typeof ROLE_IDS)[number], string> = {
  sonnet: "Sonnet",
  opus: "Opus",
  haiku: "Haiku",
  fable: "Fable",
};

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function object(value: unknown): JsonObject {
  return isObject(value) ? value : {};
}

function stringValue(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function parseSettings(text: string): { value?: JsonObject; error?: string } {
  try {
    const value: unknown = JSON.parse(text);
    return isObject(value) ? { value } : { error: "配置根节点必须是 JSON 对象。" };
  } catch (error) {
    return { error: `JSON 格式无效:${error instanceof Error ? error.message : String(error)}` };
  }
}

function roleKeys(id: (typeof ROLE_IDS)[number]): { model: string; name: string } {
  const prefix = `ANTHROPIC_DEFAULT_${id.toUpperCase()}_MODEL`;
  return { model: prefix, name: `${prefix}_NAME` };
}

function headerValue(headers: string, name: string): string {
  const target = name.toLowerCase();
  for (const line of headers.split(/\r?\n/)) {
    const colon = line.indexOf(":");
    if (colon < 0) continue;
    if (line.slice(0, colon).trim().toLowerCase() === target) {
      return line.slice(colon + 1).trim();
    }
  }
  return "";
}

function withHeader(headers: string, name: string, value: string): string {
  const target = name.toLowerCase();
  const lines = headers
    .split(/\r?\n/)
    .filter(line => line.trim())
    .filter(line => {
      const colon = line.indexOf(":");
      return colon < 0 || line.slice(0, colon).trim().toLowerCase() !== target;
    });
  if (value.trim()) lines.push(`${name}: ${value.trim()}`);
  return lines.join("\n");
}

export function AgentConfigModal({
  agent,
  models,
  initialTab = "form",
  onClose,
  onSaved,
  setErr,
}: {
  agent: AgentRow;
  models: string[];
  initialTab?: Tab;
  onClose: () => void;
  onSaved: () => void;
  setErr: (s: string) => void;
}) {
  const [data, setData] = useState<AgentConfigPage | null>(null);
  const [loadError, setLoadError] = useState("");
  const [tab, setTab] = useState<Tab>(initialTab);
  const [settings, setSettings] = useState<JsonObject | null>(null);
  const [codex, setCodex] = useState<JsonObject | null>(null);
  const [jsonDraft, setJsonDraft] = useState("");
  const [jsonError, setJsonError] = useState("");
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<AgentTestResult | null>(null);
  const [testError, setTestError] = useState("");
  const [showToken, setShowToken] = useState(false);
  const [saved, setSaved] = useState(false);

  const applyPage = (page: AgentConfigPage) => {
    setData(page);
    setSaved(false);
    if (page.kind === "claude-code") {
      setSettings(isObject(page.config) ? page.config : {});
    } else {
      setCodex(isObject(page.config) ? page.config : null);
    }
  };

  const load = () => {
    setLoadError("");
    fetchJson<AgentConfigPage>(`/admin/agents/${agent.id}/config`)
      .then(applyPage)
      .catch(error => setLoadError(String(error)));
  };

  useEffect(load, [agent.id]);

  if (!data) {
    return (
      <Dialog label={`${agent.name} 高级配置`} onClose={onClose} className="modal-wide agent-config-modal">
        <div className="modal-header"><h2>{agent.name} 高级配置</h2></div>
        <div className="modal-body">
          {loadError ? (
            <>
              <p className="alert error" role="alert">{loadError}</p>
              <button className="ghost" onClick={load}>重新加载</button>
            </>
          ) : <p className="muted">正在读取配置…</p>}
        </div>
        <div className="modal-actions"><button className="ghost" onClick={onClose}>关闭</button></div>
      </Dialog>
    );
  }

  const isClaude = data.kind === "claude-code";
  const claude = settings ?? {};
  const env = object(claude.env);
  const permissions = object(claude.permissions);
  const envValue = (key: string) => stringValue(env[key]);
  const quickModel = isClaude ? stringValue(claude.model) : stringValue(codex?.model) || agent.model || "";
  const canEdit = isClaude || data.wired;
  const tabs: { id: Tab; label: string }[] = [
    { id: "form", label: "参数" },
    ...(isClaude ? [{ id: "json" as const, label: "完整 JSON" }] : []),
    { id: "test", label: "测试验证" },
  ];

  const updateEnv = (updates: Record<string, string | null>) => {
    setSettings(current => {
      const next = { ...(current ?? {}) };
      const nextEnv = { ...object(next.env) };
      for (const [key, value] of Object.entries(updates)) {
        if (value) nextEnv[key] = value;
        else delete nextEnv[key];
      }
      if (Object.keys(nextEnv).length) next.env = nextEnv;
      else delete next.env;
      return next;
    });
  };

  const updateTop = (key: string, value: unknown) => {
    setSettings(current => {
      const next = { ...(current ?? {}) };
      if (value === undefined) delete next[key];
      else next[key] = value;
      return next;
    });
  };

  const setPermissionMode = (mode: string) => {
    const next = { ...permissions };
    if (mode) next.defaultMode = mode;
    else delete next.defaultMode;
    updateTop("permissions", Object.keys(next).length ? next : undefined);
  };

  const setRole = (
    id: (typeof ROLE_IDS)[number],
    patch: { model?: string; name?: string; oneMillion?: boolean },
  ) => {
    setSettings(current => {
      const next = { ...(current ?? {}) };
      const nextEnv = { ...object(next.env) };
      const keys = roleKeys(id);
      let model = patch.model ?? stringValue(nextEnv[keys.model]);
      const name = patch.name ?? stringValue(nextEnv[keys.name]);
      if (patch.oneMillion !== undefined) {
        const base = model.replace(/\[1m\]$/i, "");
        model = patch.oneMillion && base ? `${base}[1M]` : base;
      }
      if (model) nextEnv[keys.model] = model;
      else delete nextEnv[keys.model];
      if (name) nextEnv[keys.name] = name;
      else delete nextEnv[keys.name];
      if (Object.keys(nextEnv).length) next.env = nextEnv;
      else delete next.env;
      return next;
    });
  };

  const applyQuickModel = (model: string) => {
    if (!model) return;
    if (isClaude) {
      setSettings(current => {
        const next = { ...(current ?? {}) };
        next.model = model;
        const nextEnv = { ...object(next.env) };
        // 快捷选择必须压过 Claude Code 的环境变量优先级；已存在的
        // 角色映射与子 Agent 模型一起同步，避免出现"选了不生效"。
        nextEnv.ANTHROPIC_MODEL = model;
        for (const id of ROLE_IDS) {
          const keys = roleKeys(id);
          if (Object.prototype.hasOwnProperty.call(nextEnv, keys.model)) nextEnv[keys.model] = model;
          if (Object.prototype.hasOwnProperty.call(nextEnv, keys.name)) nextEnv[keys.name] = model;
        }
        if (Object.prototype.hasOwnProperty.call(nextEnv, "CLAUDE_CODE_SUBAGENT_MODEL")) {
          nextEnv.CLAUDE_CODE_SUBAGENT_MODEL = model;
        }
        next.env = nextEnv;
        return next;
      });
    } else {
      setCodex(current => current && { ...current, model });
    }
  };

  const updateCodex = (key: string, value: unknown) => {
    setCodex(current => {
      if (!current) return current;
      const next = { ...current };
      if (value === undefined || value === "") delete next[key];
      else next[key] = value;
      return next;
    });
  };

  const activeSettings = () => {
    if (tab !== "json") return { value: claude };
    const parsed = parseSettings(jsonDraft);
    if (parsed.error) {
      setJsonError(parsed.error);
      return null;
    }
    setJsonError("");
    return { value: parsed.value as JsonObject };
  };

  const switchTab = (next: Tab) => {
    if (next === tab) return;
    if (tab === "json" && next !== "test") {
      const parsed = parseSettings(jsonDraft);
      if (parsed.error) {
        setJsonError(parsed.error);
        return;
      }
      setSettings(parsed.value ?? {});
      setJsonError("");
    }
    if (next === "json") {
      setJsonDraft(JSON.stringify(settings ?? {}, null, 2));
      setJsonError("");
    }
    setTab(next);
  };

  const save = async () => {
    if (!canEdit || saving) return;
    let config: JsonObject | null = codex;
    if (isClaude) {
      const active = activeSettings();
      if (!active) return;
      config = active.value;
    }
    if (!config) {
      setErr(isClaude ? "配置为空" : "请先接线 Codex 并保存受管 profile");
      return;
    }
    if (isClaude) {
      const body = object(config.env).CLAUDE_CODE_EXTRA_BODY;
      if (typeof body === "string" && body.trim()) {
        try {
          if (!isObject(JSON.parse(body) as unknown)) {
            setErr("CLAUDE_CODE_EXTRA_BODY 必须是 JSON 对象。");
            return;
          }
        } catch (error) {
          setErr(`CLAUDE_CODE_EXTRA_BODY 不是有效 JSON:${error instanceof Error ? error.message : String(error)}`);
          return;
        }
      }
    }
    setSaving(true);
    try {
      const next = await postJson<AgentConfigPage>(`/admin/agents/${agent.id}/config`, {
        revision: data.revision,
        config,
      });
      applyPage(next);
      setSaved(true);
      setTestResult(null);
      setTestError("");
      onSaved();
    } catch (error) {
      setErr(`保存 ${agent.name} 高级配置失败:${String(error)}`);
    } finally {
      setSaving(false);
    }
  };

  const runTest = async () => {
    if (testing || !data.wired) return;
    setTesting(true);
    setTestError("");
    setTestResult(null);
    try {
      const result = await postJson<AgentTestResult>(`/admin/agents/${agent.id}/test`, {});
      setTestResult(result);
    } catch (error) {
      setTestError(String(error));
    } finally {
      setTesting(false);
    }
  };

  return (
    <Dialog label={`${agent.name} 高级配置`} busy={saving} onClose={onClose} className="modal-wide agent-config-modal">
      <div className="modal-header">
        <div className="row">
          <h2>{agent.name} 高级配置</h2>
          <span className={`badge ${data.wired ? "success" : "off"}`}>{data.wired ? "已接线" : "未接线"}</span>
        </div>
        <p className="muted">{data.path}</p>
      </div>
      <div className="modal-body">
        <div className="agent-config-tabs" role="tablist" aria-label="配置分区">
          {tabs.map(item => (
            <button
              key={item.id}
              role="tab"
              aria-selected={tab === item.id}
              className={tab === item.id ? "on" : ""}
              onClick={() => switchTab(item.id)}
            >
              {item.label}
            </button>
          ))}
        </div>

        {tab === "form" && (
          <>
            {!isClaude && !data.wired && (
              <p className="alert">Codex 使用独立的受管 profile。请先在 Agents 行打开接线开关，再回来配置。</p>
            )}
            <section className="agent-config-section">
              <div className="section-head">
                <h3>使用模型</h3>
                <span className="muted">保存后立即写入 Agent 配置</span>
              </div>
              <div className="field-grid">
                <label className="field">快捷选择已有模型
                  <select
                    aria-label={`${agent.name} 使用模型`}
                    value={quickModel}
                    disabled={!canEdit || saving}
                    onChange={event => applyQuickModel(event.target.value)}
                  >
                    {!quickModel && <option value="">选择模型</option>}
                    {quickModel && !models.includes(quickModel) && <option value={quickModel}>{quickModel}</option>}
                    {models.map(model => (
                      <option key={model} value={model}>{model === "aubridge" ? "自动(aubridge)" : model}</option>
                    ))}
                  </select>
                </label>
              </div>
              {isClaude && (
                <p className="muted">
                  快捷选择会同时写入 <code>model</code>、<code>ANTHROPIC_MODEL</code> 以及已存在的角色映射，避免环境变量覆盖。
                </p>
              )}
            </section>

            {isClaude ? (
              <>
                <section className="agent-config-section">
                  <div className="section-head">
                    <h3>模型角色映射</h3>
                    <span className="muted">Claude Code 的 Sonnet / Opus / Haiku / Fable</span>
                  </div>
                  <datalist id="agent-model-options">
                    {models.map(model => <option key={model} value={model} />)}
                  </datalist>
                  <div className="agent-role-table">
                    {ROLE_IDS.map(id => {
                      const keys = roleKeys(id);
                      const model = envValue(keys.model);
                      const name = envValue(keys.name);
                      return (
                        <div className="agent-role-row" key={id}>
                          <strong>{ROLE_LABELS[id]}</strong>
                          <input
                            aria-label={`${ROLE_LABELS[id]} 显示名称`}
                            placeholder="显示名称（可选）"
                            value={name}
                            disabled={!canEdit || saving}
                            onChange={event => setRole(id, { name: event.target.value })}
                          />
                          <input
                            aria-label={`${ROLE_LABELS[id]} 实际模型`}
                            placeholder={quickModel || "跟随默认模型"}
                            list="agent-model-options"
                            value={model}
                            disabled={!canEdit || saving}
                            onChange={event => setRole(id, { model: event.target.value })}
                          />
                          <label className="check">
                            <input
                              type="checkbox"
                              aria-label={`${ROLE_LABELS[id]} 声明 1M 上下文`}
                              checked={/\[1m\]$/i.test(model)}
                              disabled={!canEdit || saving || !model}
                              onChange={event => setRole(id, { oneMillion: event.target.checked })}
                            />
                            1M
                          </label>
                        </div>
                      );
                    })}
                  </div>
                  <p className="muted">1M 会以 <code>[1M]</code> 后缀写入实际模型；显示名称可留空。</p>
                </section>

                <section className="agent-config-section">
                  <h3>默认与子 Agent 模型</h3>
                  <div className="field-grid">
                    <label className="field">兜底模型（ANTHROPIC_MODEL）
                      <input
                        list="agent-model-options"
                        value={envValue("ANTHROPIC_MODEL")}
                        placeholder="留空则使用顶层 model"
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({ ANTHROPIC_MODEL: event.target.value })}
                      />
                    </label>
                    <label className="field">子 Agent 模型
                      <input
                        list="agent-model-options"
                        value={envValue("CLAUDE_CODE_SUBAGENT_MODEL")}
                        placeholder="可选"
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({ CLAUDE_CODE_SUBAGENT_MODEL: event.target.value })}
                      />
                    </label>
                    <label className="field">权限模式
                      <select
                        value={stringValue(permissions.defaultMode)}
                        disabled={!canEdit || saving}
                        onChange={event => setPermissionMode(event.target.value)}
                      >
                        <option value="">沿用 Claude Code 默认</option>
                        <option value="default">default</option>
                        <option value="acceptEdits">acceptEdits</option>
                        <option value="plan">plan</option>
                        <option value="bypassPermissions">bypassPermissions</option>
                      </select>
                    </label>
                  </div>
                </section>

                <section className="agent-config-section">
                  <h3>CLI 环境变量</h3>
                  <div className="field-grid">
                    <label className="field">最大输出 tokens
                      <input
                        inputMode="numeric"
                        value={envValue("CLAUDE_CODE_MAX_OUTPUT_TOKENS")}
                        placeholder="例如 131072"
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({ CLAUDE_CODE_MAX_OUTPUT_TOKENS: event.target.value })}
                      />
                    </label>
                    <label className="field">上下文窗口 tokens
                      <input
                        inputMode="numeric"
                        value={envValue("CLAUDE_CODE_MAX_CONTEXT_TOKENS")}
                        placeholder="例如 400000"
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({ CLAUDE_CODE_MAX_CONTEXT_TOKENS: event.target.value })}
                      />
                    </label>
                    <label className="field">思考预算 tokens
                      <input
                        inputMode="numeric"
                        value={envValue("MAX_THINKING_TOKENS")}
                        placeholder="可选"
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({ MAX_THINKING_TOKENS: event.target.value })}
                      />
                    </label>
                  </div>
                  <label className="field">自定义 User-Agent
                    <input
                      value={headerValue(envValue("ANTHROPIC_CUSTOM_HEADERS"), "User-Agent")}
                      placeholder="例如 aubridge/1.0"
                      disabled={!canEdit || saving}
                      onChange={event => updateEnv({
                        ANTHROPIC_CUSTOM_HEADERS: withHeader(
                          envValue("ANTHROPIC_CUSTOM_HEADERS"),
                          "User-Agent",
                          event.target.value,
                        ),
                      })}
                    />
                  </label>
                  <label className="field">自定义请求头（ANTHROPIC_CUSTOM_HEADERS）
                    <textarea
                      rows={3}
                      value={envValue("ANTHROPIC_CUSTOM_HEADERS")}
                      placeholder={"Name: Value\n每行一个"}
                      disabled={!canEdit || saving}
                      onChange={event => updateEnv({ ANTHROPIC_CUSTOM_HEADERS: event.target.value })}
                    />
                  </label>
                  <label className="field">请求体覆盖（CLAUDE_CODE_EXTRA_BODY）
                    <textarea
                      rows={3}
                      value={envValue("CLAUDE_CODE_EXTRA_BODY")}
                      placeholder={'{"reasoning":{"effort":"high"}}'}
                      disabled={!canEdit || saving}
                      onChange={event => updateEnv({ CLAUDE_CODE_EXTRA_BODY: event.target.value })}
                    />
                  </label>
                  <p className="muted">请求头每行一个；敏感头读取时打码，保存时未改动的占位符回填原值。请求体必须是 JSON 对象，会合并到模型请求顶层。</p>
                  <div className="row agent-config-checks">
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={envValue("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC") === "1"}
                        disabled={!canEdit || saving}
                        onChange={event => updateEnv({
                          CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: event.target.checked ? "1" : null,
                        })}
                      />
                      禁用非必要流量
                    </label>
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={claude.includeCoAuthoredBy === false}
                        disabled={!canEdit || saving}
                        onChange={event => updateTop("includeCoAuthoredBy", event.target.checked ? false : undefined)}
                      />
                      隐藏 AI 署名
                    </label>
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={claude.skipDangerousModePermissionPrompt === true}
                        disabled={!canEdit || saving}
                        onChange={event => updateTop("skipDangerousModePermissionPrompt", event.target.checked ? true : undefined)}
                      />
                      跳过危险模式提示
                    </label>
                  </div>
                </section>
              </>
            ) : (
              <section className="agent-config-section">
                <div className="section-head">
                  <h3>Codex profile 参数</h3>
                  <span className="muted">接入地址与凭据由 aubridge 管理</span>
                </div>
                {codex ? (
                  <>
                    <div className="field-grid">
                      <label className="field">推理强度
                        <input
                          list="codex-effort-options"
                          value={stringValue(codex.model_reasoning_effort)}
                          placeholder="例如 high"
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("model_reasoning_effort", event.target.value)}
                        />
                        <datalist id="codex-effort-options">
                          {["minimal", "low", "medium", "high", "xhigh"].map(value => <option key={value} value={value} />)}
                        </datalist>
                      </label>
                      <label className="field">推理摘要
                        <select
                          value={stringValue(codex.model_reasoning_summary)}
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("model_reasoning_summary", event.target.value)}
                        >
                          <option value="">默认</option>
                          {["auto", "concise", "detailed", "none"].map(value => <option key={value} value={value}>{value}</option>)}
                        </select>
                      </label>
                      <label className="field">输出详细度
                        <select
                          value={stringValue(codex.model_verbosity)}
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("model_verbosity", event.target.value)}
                        >
                          <option value="">默认</option>
                          {["low", "medium", "high"].map(value => <option key={value} value={value}>{value}</option>)}
                        </select>
                      </label>
                      <label className="field">审批策略
                        <select
                          value={stringValue(codex.approval_policy)}
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("approval_policy", event.target.value)}
                        >
                          <option value="">默认</option>
                          {["untrusted", "on-failure", "on-request", "never"].map(value => <option key={value} value={value}>{value}</option>)}
                        </select>
                      </label>
                      <label className="field">沙箱模式
                        <select
                          value={stringValue(codex.sandbox_mode)}
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("sandbox_mode", event.target.value)}
                        >
                          <option value="">默认</option>
                          {["read-only", "workspace-write", "danger-full-access"].map(value => <option key={value} value={value}>{value}</option>)}
                        </select>
                      </label>
                      <label className="field">Web 搜索
                        <input
                          list="codex-web-search-options"
                          value={stringValue(codex.web_search)}
                          placeholder="disabled"
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("web_search", event.target.value)}
                        />
                        <datalist id="codex-web-search-options">
                          {["disabled", "cached", "live"].map(value => <option key={value} value={value} />)}
                        </datalist>
                      </label>
                      <label className="field">上下文窗口
                        <input
                          type="number"
                          min={1}
                          value={typeof codex.model_context_window === "number" ? codex.model_context_window : ""}
                          placeholder="例如 400000"
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("model_context_window", event.target.value ? Number(event.target.value) : undefined)}
                        />
                      </label>
                      <label className="field">最大输出 tokens
                        <input
                          type="number"
                          min={1}
                          value={typeof codex.model_max_output_tokens === "number" ? codex.model_max_output_tokens : ""}
                          placeholder="例如 131072"
                          disabled={!canEdit || saving}
                          onChange={event => updateCodex("model_max_output_tokens", event.target.value ? Number(event.target.value) : undefined)}
                        />
                      </label>
                    </div>
                    {data.text && (
                      <details className="section-details">
                        <summary>当前受管 profile TOML</summary>
                        <pre>{data.text}</pre>
                      </details>
                    )}
                  </>
                ) : <p className="muted">尚未接线，没有可编辑的受管 profile。</p>}
              </section>
            )}

            {agent.token && (
              <section className="agent-config-section">
                <span className="section-label">本 Agent token</span>
                <div className="row">
                  <code>{showToken ? agent.token : "••••••••"}</code>
                  <button className="ghost" aria-pressed={showToken} onClick={() => setShowToken(!showToken)}>
                    {showToken ? "隐藏" : "显示"}
                  </button>
                  <CopyBtn key={`${agent.id}:${agent.token}`} text={agent.token} id={`复制 ${agent.name} Agent token`} />
                </div>
              </section>
            )}
          </>
        )}

        {tab === "json" && (
          <section className="agent-config-section">
            <div className="section-head">
              <h3>完整配置 JSON</h3>
              <button className="ghost" disabled={saving} onClick={() => setJsonDraft(JSON.stringify(settings ?? {}, null, 2))}>
                用表单值重置
              </button>
            </div>
            <p className="muted">直接编辑 <code>{data.path}</code> 的完整内容。未知字段会原样保留。</p>
            {data.secrets_masked && (
              <p className="muted">密钥以 <code>••••••••</code> 显示；保存时保持原样不会覆盖真实值。</p>
            )}
            <textarea
              className="agent-config-json"
              aria-label="完整配置 JSON"
              spellCheck={false}
              rows={18}
              value={jsonDraft}
              disabled={saving}
              onChange={event => {
                setJsonDraft(event.target.value);
                setJsonError("");
              }}
            />
            {jsonError && <p className="alert error" role="alert">{jsonError}</p>}
          </section>
        )}

        {tab === "test" && (
          <section className="agent-config-section">
            <div className="section-head">
              <h3>测试验证</h3>
              <button className="button-primary" disabled={testing || !data.wired} onClick={runTest}>
                {testing ? "测试中…" : "运行测试"}
              </button>
            </div>
            <p className="muted">{data.test.note || "调用本机 CLI 验证配置。"}测试使用已保存的配置，可能会消耗订阅额度。</p>
            {data.test.command && (
              <div className="field">
                <span className="section-label">固定命令</span>
                <code className="agent-test-command">{data.test.command}</code>
              </div>
            )}
            {!data.wired && <p className="alert">请先接线后再测试；未接线的请求不会经过 aubridge。</p>}
            {testError && <p className="alert error" role="alert">{testError}</p>}
            {testResult && (
              <div className="agent-test-result">
                <div className="row">
                  <span className={`badge ${testResult.ok ? "success" : "bad"}`}>{testResult.ok ? "通过" : "失败"}</span>
                  <strong>{testResult.message}</strong>
                </div>
                <dl className="agent-test-facts">
                  <div><dt>耗时</dt><dd>{testResult.duration_ms} ms</dd></div>
                  <div><dt>退出码</dt><dd>{testResult.exit_code ?? "—"}</dd></div>
                  <div><dt>超时</dt><dd>{testResult.timed_out ? "是" : "否"}</dd></div>
                </dl>
                <div>
                  <span className="section-label">stdout</span>
                  <pre className="agent-test-output">{testResult.stdout || "（无输出）"}</pre>
                </div>
                <div>
                  <span className="section-label">stderr</span>
                  <pre className="agent-test-output">{testResult.stderr || "（无输出）"}</pre>
                </div>
              </div>
            )}
          </section>
        )}
      </div>
      <div className="modal-actions">
        {saved && <span className="muted">已保存；测试将使用新配置</span>}
        <button className="ghost" disabled={saving} onClick={onClose}>关闭</button>
        {tab !== "test" && (
          <button className="button-primary" onClick={save} disabled={saving || !canEdit}>
            {saving ? "保存中…" : "保存"}
          </button>
        )}
      </div>
    </Dialog>
  );
}

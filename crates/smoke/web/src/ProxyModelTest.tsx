import { useEffect, useRef, useState } from "react";
import { ModelTestReceipt, fetchTestModels, testProxyModel } from "./api";
import { fmtDuration } from "./format";

interface TestRun {
  id: number;
  model: string;
  at: number;
  started: number;
  controller: AbortController;
  timer: number;
}

interface TestResult {
  model: string;
  at: number;
  duration: number;
  state: ModelTestReceipt["state"] | "cancelled";
  message: string;
  receipt?: ModelTestReceipt;
  stale: boolean;
}

function snapshot(run: TestRun, state: TestResult["state"], message: string, receipt?: ModelTestReceipt): TestResult {
  return { model: run.model, at: run.at, duration: Math.round(performance.now() - run.started), state, message, receipt, stale: false };
}

export function ProxyModelTest({ serving, onCheckSources, onBusyChange }: { serving: boolean; onCheckSources: () => void; onBusyChange: (busy: boolean) => void }) {
  const [models, setModels] = useState<string[] | null>(null);
  const [model, setModel] = useState("aubridge");
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogError, setCatalogError] = useState("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<TestResult | null>(null);
  const catalogRequest = useRef<AbortController | null>(null);
  const activeRun = useRef<TestRun | null>(null);
  const sequence = useRef(0);

  useEffect(() => () => {
    sequence.current++;
    catalogRequest.current?.abort();
    catalogRequest.current = null;
    if (activeRun.current) {
      window.clearTimeout(activeRun.current.timer);
      activeRun.current.controller.abort();
      activeRun.current = null;
    }
  }, []);

  const refreshModels = async () => {
    if (activeRun.current || catalogRequest.current) return;
    const controller = new AbortController();
    catalogRequest.current = controller;
    setCatalogLoading(true);
    setCatalogError("");
    setResult(current => current && { ...current, stale: true });
    try {
      const next = await fetchTestModels(controller.signal);
      if (catalogRequest.current !== controller || controller.signal.aborted) return;
      setModels(next);
    } catch (error) {
      if (catalogRequest.current !== controller || controller.signal.aborted) return;
      setCatalogError(error instanceof Error && error.name !== "TypeError"
        ? error.message : "无法读取本机模型目录，请确认网关状态后重试。");
    } finally {
      if (catalogRequest.current === controller) {
        catalogRequest.current = null;
        setCatalogLoading(false);
      }
    }
  };

  const stopWaiting = (reason: "cancelled" | "timeout") => {
    const run = activeRun.current;
    if (!run) return;
    sequence.current++;
    activeRun.current = null;
    window.clearTimeout(run.timer);
    run.controller.abort();
    setRunning(false);
    setResult(snapshot(run, reason === "cancelled" ? "cancelled" : "unconfirmed", reason === "cancelled"
      ? "已取消等待；上游可能仍在处理，用量以历史为准。"
      : "等待超过 65 秒，未确认完成；可查看历史或手动重试。"));
  };

  const startTest = async () => {
    if (activeRun.current || catalogRequest.current || !serving || !models?.includes(model)) return;
    const run: TestRun = {
      id: ++sequence.current, model, at: Date.now(), started: performance.now(),
      controller: new AbortController(), timer: 0,
    };
    activeRun.current = run;
    setRunning(true);
    setResult(null);
    run.timer = window.setTimeout(() => stopWaiting("timeout"), 65_000);
    try {
      const receipt = await testProxyModel(run.model, run.controller.signal);
      if (sequence.current !== run.id || activeRun.current !== run) return;
      setResult(snapshot(run, receipt.state, receipt.message, receipt));
    } catch {
      if (sequence.current !== run.id || activeRun.current !== run) return;
      setResult(snapshot(run, "failed", "无法连接本机代理，请确认网关状态后重试。"));
    } finally {
      window.clearTimeout(run.timer);
      if (sequence.current === run.id && activeRun.current === run) {
        activeRun.current = null;
        setRunning(false);
      }
    }
  };

  useEffect(() => { void refreshModels(); }, []);

  const missing = models !== null && !models.includes(model);
  const busy = running || catalogLoading;
  useEffect(() => { onBusyChange(running); }, [running, onBusyChange]);
  const receipt = result?.receipt;
  const stateClass = result?.state === "success" ? "success" : result?.state === "failed" ? "bad" : "off";

  return (
    <section className="model-test" aria-label="测试模型">
      <div className="panel-body model-test-body">
        <div className="model-test-controls">
          <div className="field">
            <label htmlFor="proxy-test-model">对外模型</label>
            <select id="proxy-test-model" value={model} disabled={busy || models === null} onChange={event => {
              setModel(event.target.value);
              setResult(null);
            }}>
              {(models === null || missing) && <option value={model}>{model === "aubridge" ? "自动（免费优先）" : model}{missing ? "（已不在当前目录）" : ""}</option>}
              {models?.map(id => <option value={id} key={id}>{id === "aubridge" ? "自动（免费优先）" : id}</option>)}
            </select>
          </div>
          <div className="actions">
            <button disabled={busy} onClick={() => void refreshModels()}>刷新目录</button>
            <button className="btn-primary" disabled={busy || !serving || !models?.includes(model)} onClick={() => void startTest()}>
              {running ? "测试中…" : "开始测试"}
            </button>
            {running && <button onClick={() => stopWaiting("cancelled")}>取消测试</button>}
          </div>
        </div>
        <p className="muted">会通过当前代理发送一条真实请求，可能消耗免费额度或订阅用量。</p>
        <p className="muted">测试内容：只回复 OK。</p>
        {catalogLoading && <p className="muted" role="status">读取模型中…</p>}
        {catalogError && <p className="model-test-error" role="status">{models !== null ? "目录刷新失败，当前为上次读取结果。" : "模型目录读取失败。"}{catalogError} 请点击“刷新目录”重试。</p>}
        {missing && <p className="model-test-error">当前选择已不在当前目录，请重新选择模型。</p>}
        {!serving && <p className="muted">恢复新请求后可测试。</p>}
        {models !== null && <p className="muted">目录仅说明模型名可选，不代表凭据、额度或推理能力已就绪。</p>}
        <div className="model-test-result" aria-live="polite" aria-atomic="true">
          {running && <p className="muted" role="status">正在等待本次完整回复… 取消仅停止等待，上游可能继续处理。</p>}
          {result && <>
            <p><span className={`badge ${stateClass}`}>{result.stale ? "上次结果，未重新测试" : result.state === "success" ? "本次成功" : result.state === "failed" ? "本次失败" : result.state === "cancelled" ? "已取消等待" : "未确认"}</span></p>
            <p>{result.stale ? "上次测试：" : ""}{result.message}</p>
            <dl className="model-test-facts">
              <div><dt>请求模型</dt><dd>{result.model}</dd></div>
              <div><dt>测试时间</dt><dd>{new Date(result.at).toLocaleString()}</dd></div>
              <div><dt>总耗时</dt><dd>{fmtDuration(result.duration)}</dd></div>
              <div><dt>HTTP 状态</dt><dd>{receipt?.httpStatus ?? "未知"}</dd></div>
              <div><dt>实际来源</dt><dd>{receipt?.provider ?? "未知"}</dd></div>
              <div><dt>响应模型</dt><dd>{receipt?.responseModel ?? "未知"}</dd></div>
            </dl>
            {receipt?.responseModel === "aubridge" && result.model === "aubridge" && <p className="muted">响应返回自动别名，实际模型未确认。</p>}
            <p className="muted">自动或分组可能映射其他模型，详细路由见历史。</p>
            {receipt?.reply && <div className="model-test-reply"><span className="section-label">短回复</span><p>{receipt.reply}</p></div>}
            {result.state === "failed" && <button className="ghost" onClick={onCheckSources}>检查模型源</button>}
            <p className="muted">记录归属 ui-test，可在下方历史请求中刷新查看；用量以历史为准。</p>
          </>}
        </div>
      </div>
    </section>
  );
}

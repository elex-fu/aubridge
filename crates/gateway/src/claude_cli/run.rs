use super::auth;
use axum::http::Response;
use proto::protocol::MessagesRequest;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{mpsc, oneshot, watch, OwnedSemaphorePermit},
};

const MAX_LINE: usize = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const PARK_TTL: Duration = Duration::from_secs(120);
const TOTAL_TTL: Duration = Duration::from_secs(600);
type Segment = mpsc::Sender<Result<Vec<u8>, std::io::Error>>;

pub(super) struct Run {
    pub id: String,
    owner: String,
    subject: String,
    cancel: watch::Sender<bool>,
    done: AtomicBool,
    data: Mutex<Data>,
}
struct Data {
    expected: BTreeMap<String, String>, // public id -> exact native toolUseId
    claimed: BTreeSet<String>,
    waiting: BTreeMap<String, oneshot::Sender<Value>>,
    early: BTreeMap<String, Value>,
    seen_callbacks: BTreeSet<String>,
    segment: Option<Segment>,
    delivered: bool,
}
#[derive(Clone)]
pub(crate) struct Lease {
    inner: Arc<LeaseInner>,
}
struct LeaseInner {
    run: Arc<Run>,
    delivered: AtomicBool,
}
impl Lease {
    pub(crate) fn delivered(&self) {
        self.inner.delivered.store(true, Ordering::SeqCst);
        self.inner.run.data.lock().expect("claude run").delivered = true;
    }
}
impl Drop for LeaseInner {
    fn drop(&mut self) {
        if !self.delivered.load(Ordering::SeqCst) {
            self.run.cancel();
        }
    }
}
impl Run {
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::SeqCst) || *self.cancel.borrow()
    }
    pub fn contains(&self, id: &str) -> bool {
        self.data
            .lock()
            .expect("claude run")
            .expected
            .contains_key(id)
    }
    fn response(
        self: &Arc<Self>,
        receiver: mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    ) -> reqwest::Response {
        let source = futures_util::stream::unfold(receiver, |mut rx| async {
            rx.recv().await.map(|bytes| (bytes, rx))
        });
        let mut response = Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(source))
            .expect("claude response");
        response.extensions_mut().insert(Lease {
            inner: Arc::new(LeaseInner {
                run: self.clone(),
                delivered: AtomicBool::new(false),
            }),
        });
        response.into()
    }
    pub fn claim(
        self: &Arc<Self>,
        owner: &str,
        subject: &str,
        results: Vec<(String, Value)>,
    ) -> Result<reqwest::Response, &'static str> {
        if owner != self.owner || subject != self.subject || self.finished() {
            return Err("claude-continuation-owner-mismatch");
        }
        let mut data = self.data.lock().expect("claude run");
        if data.segment.is_some() {
            return Err("claude-continuation-busy");
        }
        let ids: BTreeSet<_> = results.iter().map(|r| r.0.as_str()).collect();
        if ids.len() != results.len()
            || results
                .iter()
                .any(|(id, _)| !data.expected.contains_key(id) || data.claimed.contains(id))
        {
            return Err("claude-continuation-invalid");
        }
        let outstanding = data
            .expected
            .keys()
            .filter(|id| !data.claimed.contains(*id))
            .count();
        if results.len() != outstanding {
            return Err("claude-continuation-incomplete");
        }
        let (tx, rx) = mpsc::channel(4);
        data.segment = Some(tx);
        data.delivered = false;
        for (id, result) in results {
            let native = data.expected[&id].clone();
            data.claimed.insert(id);
            let content = match &result["content"] {
                Value::String(s) => json!([{"type":"text", "text":s}]),
                Value::Array(blocks) => Value::Array(blocks.clone()),
                _ => json!([{"type":"text", "text":""}]),
            };
            let value =
                json!({"content":content, "isError":result["is_error"].as_bool().unwrap_or(false)});
            if let Some(waiter) = data.waiting.remove(&native) {
                let _ = waiter.send(value);
            } else {
                data.early.insert(native, value);
            }
        }
        drop(data);
        Ok(self.response(rx))
    }
    pub async fn callback(&self, native: String) -> Result<Value, &'static str> {
        let rx = {
            let mut data = self.data.lock().expect("claude run");
            if !data.seen_callbacks.insert(native.clone()) {
                return Err("duplicate tool callback");
            }
            if let Some(value) = data.early.remove(&native) {
                return Ok(value);
            }
            if data.waiting.len() >= 64 {
                return Err("too many pending tools");
            }
            let (tx, rx) = oneshot::channel();
            data.waiting.insert(native, tx);
            rx
        };
        let mut cancel = self.cancel.subscribe();
        tokio::select! {
            result = rx => result.map_err(|_| "tool call cancelled"),
            _ = cancel.changed() => Err("tool call cancelled"),
            _ = tokio::time::sleep(PARK_TTL) => { self.cancel(); Err("tool result expired") }
        }
    }
}

pub(super) fn random() -> Result<String, &'static str> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| "random-unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn private_dir() -> Result<PathBuf, &'static str> {
    let path = std::env::temp_dir().join(format!("aubridge-claude-{}", random()?));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| "claude-private-dir")?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir(&path).map_err(|_| "claude-private-dir")?;
    Ok(path)
}
pub(super) fn private_file(path: &Path, bytes: &[u8]) -> Result<(), &'static str> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|_| "claude-private-file")
}
struct Reservation(Option<Arc<Run>>);
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(run) = &self.0 {
            run.cancel();
        }
    }
}
struct Workspace(PathBuf);
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn start(
    program: &PathBuf,
    login: auth::Login,
    owner: String,
    req: &MessagesRequest,
    permit: OwnedSemaphorePermit,
    activity: crate::state::UpdateActivityLease,
    register: impl FnOnce(&Arc<Run>) -> Result<(), &'static str>,
) -> Result<(Arc<Run>, reqwest::Response), &'static str> {
    let dir = Workspace(private_dir()?);
    let (cancel, _) = watch::channel(false);
    let (tx, rx) = mpsc::channel(4);
    let run = Arc::new(Run {
        id: random()?,
        owner,
        subject: login.subject.clone(),
        cancel,
        done: AtomicBool::new(false),
        data: Mutex::new(Data {
            expected: BTreeMap::new(),
            claimed: BTreeSet::new(),
            waiting: BTreeMap::new(),
            early: BTreeMap::new(),
            seen_callbacks: BTreeSet::new(),
            segment: Some(tx),
            delivered: false,
        }),
    });
    register(&run)?;
    let mut reservation = Reservation(Some(run.clone()));
    let tools = req.tools.as_ref().map(|tools| tools.iter().filter(|t| match &req.tool_choice {
        Some(proto::protocol::ToolChoice::None) => false,
        Some(proto::protocol::ToolChoice::Tool { name, .. }) => &t.name == name,
        _ => true,
    }).enumerate().map(|(i, t)| {
        (format!("tool_{i}"), (t.name.clone(), json!({"name":format!("tool_{i}"), "description":t.description, "inputSchema":t.input_schema})))
    }).collect::<BTreeMap<_, _>>()).unwrap_or_default();
    let server = super::mcp::start(run.clone(), &dir.0, tools.clone()).await?;
    let mut command = auth::command(program, &login.env);
    command
        .current_dir(&dir.0)
        .args([
            "--setting-sources",
            "",
            "-p",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
        ])
        .arg(dir.0.join("mcp.json"))
        .args([
            "--allowedTools",
            "mcp__aubridge__*",
            "--permission-mode",
            "dontAsk",
            "--model",
            &req.model,
        ]);
    if let Some(system) = &req.system {
        let system = match system {
            proto::protocol::SystemContent::Text(text) => text.clone(),
            proto::protocol::SystemContent::Blocks(blocks) => blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let system_path = dir.0.join("system.txt");
        private_file(&system_path, system.as_bytes())?;
        command.arg("--append-system-prompt-file").arg(system_path);
    }
    if req.max_tokens > 0 {
        command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", req.max_tokens.to_string());
    }
    if run.finished() {
        return Err("claude-login-changed");
    }
    let mut child = command.spawn().map_err(|_| "claude-spawn-failed")?;
    let mut stdin = child.stdin.take().ok_or("claude-stdin")?;
    let stdout = child.stdout.take().ok_or("claude-stdout")?;
    let history = serde_json::to_value(&req.messages).map_err(|_| "encode-error")?;
    let prompt = json!({"type":"user", "message":{"role":"user", "content":format!("Continue the following conversation. Tool names available in this run: {}. Conversation: {}", tools.iter().map(|(n,(original,_))|format!("{original}={n}")).collect::<Vec<_>>().join(", "), history)}}).to_string()+"\n";
    let response = run.response(rx);
    let task_run = run.clone();
    tokio::spawn(async move {
        let _activity = activity;
        let _permit = permit;
        let _dir = dir;
        let mut cancel = task_run.cancel.subscribe();
        let already_cancelled = *cancel.borrow();
        let outcome = if already_cancelled {
            Err("claude-cancelled")
        } else {
            tokio::select! {
                result = async {
                    stdin.write_all(prompt.as_bytes()).await.map_err(|_| "claude-stdin")?;
                    let _stdin = stdin;
                    read_output(&task_run, stdout, &tools).await
                } => result,
                _ = cancel.changed() => Err("claude-cancelled"),
                _ = tokio::time::sleep(TOTAL_TTL) => Err("claude-total-timeout"),
            }
        };
        if let Err(reason) = outcome {
            let tx = task_run.data.lock().expect("claude run").segment.take();
            if let Some(tx) = tx {
                let _ = tx.send(Err(std::io::Error::other(reason))).await;
            }
        }
        task_run.done.store(true, Ordering::SeqCst);
        task_run.cancel();
        drop(server);
        kill_group(&mut child).await;
        task_run.data.lock().expect("claude run").waiting.clear();
    });
    reservation.0 = None;
    Ok((run, response))
}

async fn read_output(
    run: &Arc<Run>,
    stdout: tokio::process::ChildStdout,
    tools: &BTreeMap<String, (String, Value)>,
) -> Result<(), &'static str> {
    let mut reader = BufReader::new(stdout);
    let mut total = 0;
    let mut message: Option<Value> = None;
    let mut inputs: BTreeMap<usize, String> = BTreeMap::new();
    let mut final_message = None;
    loop {
        let mut line = Vec::new();
        // fill_buf bounds memory before allocation, including lines without newline.
        loop {
            let chunk = tokio::time::timeout(PARK_TTL, reader.fill_buf())
                .await
                .map_err(|_| "claude-idle-timeout")?
                .map_err(|_| "claude-read")?;
            if chunk.is_empty() {
                return Err("claude-unexpected-eof");
            }
            let size = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map_or(chunk.len(), |n| n + 1);
            if line.len() + size > MAX_LINE || total + size > MAX_OUTPUT {
                return Err("claude-output-limit");
            }
            let end = chunk[size - 1] == b'\n';
            line.extend_from_slice(&chunk[..size]);
            total += size;
            reader.consume(size);
            if end {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&line).map_err(|_| "claude-invalid-json")?;
        if value["type"] == "result" {
            if value["is_error"] == true {
                return Err("claude-result-error");
            }
            let message = final_message
                .take()
                .ok_or("claude-result-without-message")?;
            emit_message(run, message, tools).await?;
            return Ok(());
        }
        if value["type"] != "stream_event" {
            continue;
        }
        let event = &value["event"];
        match event["type"].as_str() {
            Some("message_start") => {
                message = Some(event["message"].clone());
                inputs.clear();
                if let Some(m) = &mut message {
                    m["content"] = json!([]);
                }
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().ok_or("claude-block-index")? as usize;
                let blocks = message
                    .as_mut()
                    .and_then(|m| m["content"].as_array_mut())
                    .ok_or("claude-block-without-message")?;
                if index != blocks.len() {
                    return Err("claude-block-order");
                }
                blocks.push(event["content_block"].clone());
            }
            Some("content_block_delta") => {
                let index = event["index"].as_u64().ok_or("claude-block-index")? as usize;
                let block = message
                    .as_mut()
                    .and_then(|m| m["content"].as_array_mut())
                    .and_then(|b| b.get_mut(index))
                    .ok_or("claude-block-index")?;
                let delta = &event["delta"];
                let (field, source) = match delta["type"].as_str() {
                    Some("text_delta") => ("text", "text"),
                    Some("thinking_delta") => ("thinking", "thinking"),
                    Some("signature_delta") => ("signature", "signature"),
                    Some("input_json_delta") => {
                        inputs
                            .entry(index)
                            .or_default()
                            .push_str(delta["partial_json"].as_str().ok_or("claude-tool-json")?);
                        continue;
                    }
                    _ => return Err("claude-unsupported-delta"),
                };
                block[field] = json!(format!(
                    "{}{}",
                    block[field].as_str().unwrap_or_default(),
                    delta[source].as_str().unwrap_or_default()
                ));
            }
            Some("content_block_stop") => {
                let index = event["index"].as_u64().ok_or("claude-block-index")? as usize;
                if let Some(input) = inputs.remove(&index) {
                    let m = message.as_mut().ok_or("claude-block-without-message")?;
                    m["content"][index]["input"] =
                        serde_json::from_str(&input).map_err(|_| "claude-tool-json")?;
                }
            }
            Some("message_delta") => {
                let m = message.as_mut().ok_or("claude-delta-without-message")?;
                m["stop_reason"] = event["delta"]["stop_reason"].clone();
                if let Some(usage) = event["usage"].as_object() {
                    for (key, count) in usage {
                        m["usage"][key] = count.clone();
                    }
                }
            }
            Some("message_stop") => {
                let m = message.take().ok_or("claude-stop-without-message")?;
                let is_tool = m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_use"));
                if is_tool {
                    emit_message(run, m, tools).await?;
                } else {
                    final_message = Some(m);
                }
            }
            Some("error") => return Err("claude-stream-error"),
            _ => {}
        }
    }
}
async fn emit_message(
    run: &Arc<Run>,
    mut message: Value,
    tools: &BTreeMap<String, (String, Value)>,
) -> Result<(), &'static str> {
    let mut calls = Vec::new();
    if let Some(blocks) = message["content"].as_array_mut() {
        for block in blocks {
            if block["type"] != "tool_use" {
                continue;
            }
            let native = block["id"].as_str().ok_or("claude-tool-id")?.to_owned();
            let name = block["name"]
                .as_str()
                .and_then(|n| n.strip_prefix("mcp__aubridge__"))
                .and_then(|n| tools.get(n))
                .ok_or("claude-unoffered-tool")?
                .0
                .clone();
            let public = format!("{}{}", super::CALL_PREFIX, random()?);
            block["id"] = json!(public);
            block["name"] = json!(name);
            calls.push((public, native));
        }
    }
    let tx = {
        let mut data = run.data.lock().expect("claude run");
        data.expected.extend(calls);
        data.segment.take().ok_or("claude-unexpected-assistant")?
    };
    tx.send(Ok(message_sse(&message)))
        .await
        .map_err(|_| "claude-client-disconnected")?;
    Ok(())
}

fn message_sse(message: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut emit = |kind: &str, value: Value| {
        bytes.extend_from_slice(proto::sse::frame_event(kind, &value.to_string()).as_bytes())
    };
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    emit(
        "message_start",
        json!({"type":"message_start", "message":start}),
    );
    if let Some(blocks) = message["content"].as_array() {
        for (i, block) in blocks.iter().enumerate() {
            let mut start = block.clone();
            match block["type"].as_str() {
                Some("text") => start["text"] = json!(""),
                Some("thinking") => {
                    start["thinking"] = json!("");
                    start["signature"] = json!("");
                }
                Some("tool_use") => start["input"] = json!({}),
                _ => {}
            }
            emit(
                "content_block_start",
                json!({"type":"content_block_start", "index":i, "content_block":start}),
            );
            let delta = match block["type"].as_str() {
                Some("text") => Some(json!({"type":"text_delta", "text":block["text"]})),
                Some("thinking") => {
                    Some(json!({"type":"thinking_delta", "thinking":block["thinking"]}))
                }
                Some("tool_use") => Some(
                    json!({"type":"input_json_delta", "partial_json":block["input"].to_string()}),
                ),
                _ => None,
            };
            if let Some(delta) = delta {
                emit(
                    "content_block_delta",
                    json!({"type":"content_block_delta", "index":i, "delta":delta}),
                );
            }
            if block["type"] == "thinking" && block["signature"].is_string() {
                emit(
                    "content_block_delta",
                    json!({"type":"content_block_delta", "index":i, "delta":{"type":"signature_delta", "signature":block["signature"]}}),
                );
            }
            emit(
                "content_block_stop",
                json!({"type":"content_block_stop", "index":i}),
            );
        }
    }
    emit(
        "message_delta",
        json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"], "stop_sequence":null}, "usage":message.get("usage").cloned().unwrap_or(json!({}))}),
    );
    emit("message_stop", json!({"type":"message_stop"}));
    bytes
}
pub(super) async fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn update_maintenance_waits_for_native_child_cancellation_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = Workspace(private_dir().unwrap());
        let program = workspace.0.join("claude");
        private_file(&program, b"#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = crate::state::GatewayState::from_manifests(Default::default(), "test");
        let activity = state.begin_update_activity().unwrap();
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        let request: MessagesRequest = serde_json::from_value(json!({
            "model":"fixture", "max_tokens":8,
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let (_run, response) = start(
            &program,
            auth::Login {
                subject: "fixture".into(),
                env: BTreeMap::new(),
            },
            "caller/session".into(),
            &request,
            permit,
            activity,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert!(state.try_update_maintenance().is_err());
        drop(response);
        let maintenance = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(lease) = state.try_update_maintenance() {
                    break lease;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled child must release activity after cleanup");
        assert!(state.is_serving());
        drop(maintenance);
    }

    fn parked() -> Arc<Run> {
        Arc::new(Run {
            id: "run".into(),
            owner: "caller/session".into(),
            subject: "subject-a".into(),
            cancel: watch::channel(false).0,
            done: AtomicBool::new(false),
            data: Mutex::new(Data {
                expected: BTreeMap::from([
                    ("aubcc_first".into(), "toolu_first".into()),
                    ("aubcc_second".into(), "toolu_second".into()),
                ]),
                claimed: BTreeSet::new(),
                waiting: BTreeMap::new(),
                early: BTreeMap::new(),
                seen_callbacks: BTreeSet::new(),
                segment: None,
                delivered: true,
            }),
        })
    }
    fn results() -> Vec<(String, Value)> {
        vec![
            ("aubcc_second".into(), json!({"content":"second"})),
            ("aubcc_first".into(), json!({"content":"first"})),
        ]
    }
    #[tokio::test]
    async fn reverse_order_early_results_use_native_ids_exactly_once() {
        let run = parked();
        let response = run.claim("caller/session", "subject-a", results()).unwrap();
        assert_eq!(
            run.callback("toolu_first".into()).await.unwrap()["content"][0]["text"],
            "first"
        );
        assert_eq!(
            run.callback("toolu_second".into()).await.unwrap()["content"][0]["text"],
            "second"
        );
        assert!(run.callback("toolu_first".into()).await.is_err());
        assert!(run.claim("caller/session", "subject-a", results()).is_err());
        drop(response);
        assert!(run.finished());
    }
    #[test]
    fn claim_is_atomic_and_rejects_cross_owner_subject_mixed_and_duplicates() {
        let run = parked();
        assert!(run.claim("other-session", "subject-a", results()).is_err());
        assert!(run.claim("caller/session", "subject-b", results()).is_err());
        let mut mixed = results();
        mixed[1].0 = "aubcc_unknown".into();
        assert!(run.claim("caller/session", "subject-a", mixed).is_err());
        let mut duplicate = results();
        duplicate[1].0 = duplicate[0].0.clone();
        assert!(run.claim("caller/session", "subject-a", duplicate).is_err());
        assert!(run.data.lock().unwrap().claimed.is_empty());
    }
    #[test]
    fn renderer_emits_text_and_tool_json_deltas_with_native_stop_reason() {
        let message = json!({"id":"msg", "model":"sonnet", "role":"assistant", "stop_reason":"max_tokens", "content":[
            {"type":"text", "text":"answer"},
            {"type":"tool_use", "id":"aubcc_one", "name":"read", "input":{"path":"/random"}}
        ]});
        let bytes = message_sse(&message);
        let wire = String::from_utf8(bytes).unwrap();
        assert!(wire.contains("text_delta"));
        assert!(wire.contains("input_json_delta"));
        assert!(wire.contains("max_tokens"));
        assert!(!wire.contains("end_turn"));
    }
    #[test]
    fn normal_delivery_keeps_parked_run_while_dropped_delivery_cancels() {
        let run = parked();
        let response = run.claim("caller/session", "subject-a", results()).unwrap();
        response.extensions().get::<Lease>().unwrap().delivered();
        drop(response);
        assert!(!run.finished());
    }
}

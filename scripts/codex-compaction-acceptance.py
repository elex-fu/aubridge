#!/usr/bin/env python3
"""Real installed Codex, private HOME, production gateway via metadata-only recorder.
Run with rtk proxy python3 scripts/codex-compaction-acceptance.py --config PATH.
No credentials or conversation bodies are written into the evidence JSON.
"""
import argparse, hashlib, http.client, http.server, json, os, pathlib, queue, re, secrets
import subprocess, tempfile, threading, time, urllib.parse
try:
    import tomllib
except ImportError:
    tomllib = None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=pathlib.Path, required=True, help="Production managed aubridge Codex profile")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--auth", type=pathlib.Path, default=pathlib.Path.home()/".codex"/"auth.json")
    parser.add_argument("--gateway-receipt", type=pathlib.Path, help="Runner's /admin/build plus running binary SHA receipt")
    args = parser.parse_args()
    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
    protected = {str(path):sha(path) for path in (args.config,args.auth)}
    original = args.config.read_text()
    if tomllib:
        config = tomllib.loads(original)
    else:
        # The generated managed profile uses JSON-compatible quoted TOML strings.
        # Apple Python 3.9 has no tomllib; parse only these documented owned fields.
        section = re.search(r"(?ms)^\[model_providers\.aubridge\]\s*$(.*?)(?=^\[|\Z)", original)
        if not section: raise ValueError("Missing managed aubridge provider")
        def field(text, name):
            match = re.search(r"(?m)^"+re.escape(name)+r"\s*=\s*(\"(?:[^\"\\]|\\.)*\")\s*$", text)
            if not match: raise ValueError("Unsupported managed profile field: "+name)
            return json.loads(match.group(1))
        config = {"model":field(original.split("[",1)[0],"model"),"model_providers":{"aubridge":{name:field(section.group(1),name) for name in ("name","base_url")}}}
    provider = config["model_providers"]["aubridge"]
    assert provider["name"] == "aubridge", "Must preserve production provider name"
    target = urllib.parse.urlsplit(provider["base_url"])
    assert target.hostname in ("127.0.0.1", "localhost"), "Expected production loopback gateway"
    wires = []
    nonce = secrets.token_hex(16)
    stage = "setup"

    class Recorder(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *_):
            pass
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            entry = {"path": self.path, "bytes": len(body), "stage":stage, "input_contains_marker":nonce.encode() in body}
            try:
                request = json.loads(body)
                entry["input_types"] = [item.get("type", "message") for item in request.get("input", []) if isinstance(item, dict)]
                entry["trigger"] = any(kind == "compaction_trigger" for kind in entry["input_types"])
            except (ValueError, TypeError):
                entry["encoding"] = self.headers.get("Content-Encoding", "unknown")
            conn = http.client.HTTPConnection(target.hostname, target.port, timeout=180)
            headers = {key: value for key, value in self.headers.items() if key.lower() not in ("host", "connection", "transfer-encoding")}
            conn.request("POST", self.path, body, headers)
            response = conn.getresponse()
            entry.update(status=response.status, provider=response.getheader("x-aubridge-provider"), request_id=response.getheader("x-aubridge-request-id"))
            wires.append(entry)
            self.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                    self.send_header(key, value)
            self.send_header("Connection", "close")
            self.end_headers()
            marker_tail = ""
            pending_sse = b""
            entry["response_contains_marker"] = False
            def observe_text(text, delta=False):
                nonlocal marker_tail
                if not isinstance(text, str): return
                combined = marker_tail + text if delta else text
                entry["response_contains_marker"] |= nonce in combined
                if delta: marker_tail = combined[-len(nonce):]
            def observe_event(event):
                kind = event.get("type")
                if kind == "response.output_text.delta": observe_text(event.get("delta"), True)
                elif kind == "response.output_text.done": observe_text(event.get("text"))
                items = [event.get("item", {})] if kind == "response.output_item.done" else event.get("response", {}).get("output", []) if kind == "response.completed" else []
                for item in items:
                    if item.get("type") == "message" and item.get("role") == "assistant":
                        for part in item.get("content", []): observe_text(part.get("text"))
            try:
                while chunk := response.read1(8192):
                    pending_sse = (pending_sse + chunk).replace(b"\r\n", b"\n")
                    while b"\n\n" in pending_sse:
                        frame, pending_sse = pending_sse.split(b"\n\n", 1)
                        data = b"\n".join(line[5:].lstrip() for line in frame.splitlines() if line.startswith(b"data:"))
                        try: observe_event(json.loads(data))
                        except (ValueError, TypeError, AttributeError): pass
                    if len(pending_sse) > 2 * 1024 * 1024: raise ValueError("Diagnostic SSE frame exceeds 2 MiB")
                    self.wfile.write(chunk)
                    self.wfile.flush()
            finally:
                conn.close()
                self.close_connection = True

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Recorder)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    report = {"version": subprocess.check_output([args.codex, "--version"], text=True).strip(), "provider_name": "aubridge", "wire": wires, "passed": False, "protected_hashes_before":protected, "gateway_receipt_sha256":sha(args.gateway_receipt) if args.gateway_receipt else None}
    with tempfile.TemporaryDirectory(prefix="aubridge-compact-") as private:
        home = pathlib.Path(private)
        codex_home = home / ".codex"
        codex_home.mkdir(mode=0o700)
        # Preserve the exact managed production profile except the recorder port.
        forwarded = original.replace(provider["base_url"], f"http://127.0.0.1:{server.server_port}{target.path}")
        (codex_home / "config.toml").write_text(forwarded)
        workspace = home / "workspace"
        workspace.mkdir()
        env = dict(os.environ, HOME=str(home), CODEX_HOME=str(codex_home))
        # The observed system proxy rejects the loopback recorder URL. Limit the
        # bypass to this disposable Codex child; never change host proxy settings.
        for proxy_bypass in ("NO_PROXY", "no_proxy"):
            env[proxy_bypass] = ",".join(filter(None, [env.get(proxy_bypass), "127.0.0.1", "localhost", "::1"]))
        stderr = tempfile.TemporaryFile()
        process = subprocess.Popen([args.codex, "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, env=env, cwd=workspace)
        incoming = queue.Queue()
        def reader():
            for line in process.stdout:
                try: incoming.put(json.loads(line))
                except ValueError: pass
        threading.Thread(target=reader, daemon=True).start()
        sequence = 0
        notices = []
        def send(method, params, notification=False):
            nonlocal sequence
            sequence += 1
            msg = {"method": method, "params": params}
            if not notification: msg["id"] = sequence
            process.stdin.write(json.dumps(msg) + "\n")
            process.stdin.flush()
            return sequence
        def wait(predicate, timeout=180):
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                msg = incoming.get(timeout=max(0.01, end-time.monotonic()))
                if "method" in msg and "id" in msg:
                    # Production profile is read-only; allow the explicitly requested
                    # tool task only in this disposable private workspace.
                    if msg["method"].endswith("requestApproval"):
                        process.stdin.write(json.dumps({"id":msg["id"],"result":{"decision":"accept"}})+"\n")
                        process.stdin.flush()
                if "method" in msg: notices.append(msg)
                if predicate(msg): return msg
            raise TimeoutError("app-server did not reach required state")
        def rpc(method, params):
            rid = send(method, params)
            msg = wait(lambda m: m.get("id") == rid)
            if "error" in msg: raise RuntimeError(str(msg["error"]))
            return msg["result"]
        def turn(thread_id, text):
            mark = len(notices)
            result = rpc("turn/start", {"threadId":thread_id,"input":[{"type":"text","text":text}]})
            tid = result["turn"]["id"]
            done = lambda m: m.get("method") == "turn/completed" and m["params"]["turn"]["id"] == tid
            ended = next((m for m in notices[mark:] if done(m)), None) or wait(done)
            assert ended["params"]["turn"]["status"] == "completed", "Turn failed"
            return [m for m in notices[mark:] if m.get("params",{}).get("threadId") == thread_id and m.get("params",{}).get("turnId",tid) == tid]
        try:
            rpc("initialize", {"clientInfo":{"name":"aubridge_compaction_acceptance","version":"1"},"capabilities":{"experimentalApi":True}})
            send("initialized", {}, True)
            thread = rpc("thread/start", {"cwd":str(workspace),"modelProvider":"aubridge","model":config["model"],"approvalPolicy":"never","sandbox":"workspace-write"})["thread"]["id"]
            seed = workspace / "seed.txt"
            seed.write_text(nonce)
            def agent_result(events):
                messages = [m["params"]["item"] for m in events if m.get("method") == "item/completed" and m["params"]["item"].get("type") == "agentMessage"]
                finals = [m for m in messages if m.get("phase") == "final_answer"]
                final = finals[-1] if finals else messages[-1] if messages else {}
                exact = final.get("text", "").strip() == nonce
                return exact, [{"phase":m.get("phase"),"exact":m.get("text", "").strip() == nonce,"contains":nonce in m.get("text", ""),"length":len(m.get("text", ""))} for m in messages]
            stage = "first"
            first = turn(thread, "Use a shell tool to read seed.txt. Remember its project marker for later continuation. Reply with only its exact contents.")
            first_exact, first_messages = agent_result(first)
            report.update(thread_id=thread, first_final_exact=first_exact, first_agent_messages=first_messages)
            assert first_exact, "Initial assistant did not read marker"
            seed.unlink()
            start = len(notices)
            wire_start = len(wires)
            stage = "compact"
            rpc("thread/compact/start", {"threadId":thread})
            def compact_done(m):
                return m.get("method") == "item/completed" and m["params"]["item"]["type"] == "contextCompaction"
            completed = next((m for m in notices[start:] if compact_done(m)), None) or wait(compact_done)
            compact_id = completed["params"]["item"]["id"]
            assert any(m.get("method") == "item/started" and m["params"]["item"].get("id") == compact_id for m in notices[start:]), "Missing matching compaction start"
            compact_turn_id = completed["params"].get("turnId")
            compact_turn_done = lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == thread and (compact_turn_id is None or m["params"]["turn"]["id"] == compact_turn_id)
            ended = next((m for m in notices[start:] if compact_turn_done(m)), None) or wait(compact_turn_done)
            assert ended["params"]["turn"]["status"] == "completed", "Compaction turn failed"
            compact_wires = wires[wire_start:].copy()
            assert compact_wires, "Compaction produced no observable gateway request"
            assert all("encoding" not in w for w in compact_wires), "Cannot classify compressed compact request"
            report.update(compaction_id=compact_id, compact_turn_id=compact_turn_id, compact_started_completed_match=True, compact_turn_status=ended["params"]["turn"]["status"],
                actual_wire="remote-v2" if any(w.get("trigger") for w in compact_wires) else "remote-v1" if any(w["path"].endswith("/compact") for w in compact_wires) else "local-summary-responses")
            stage = "follow"
            follow = turn(thread, "Recall the exact project marker from before compaction. Then use a shell tool to write only that marker to marker.txt in the current workspace. Reply with the exact marker.")
            final_exact, final_messages = agent_result(follow)
            marker_file = workspace / "marker.txt"
            file_exact = marker_file.exists() and marker_file.read_bytes().strip() == nonce.encode()
            report.update(marker_recalled=final_exact, final_agent_messages=final_messages,
                tool_file_exists=marker_file.exists(), tool_file_verified=file_exact,
                compact_input_contains_marker=any(w["input_contains_marker"] for w in compact_wires),
                compact_response_contains_marker=any(w["response_contains_marker"] for w in compact_wires),
                follow_first_input_contains_marker=next((w["input_contains_marker"] for w in wires if w["stage"] == "follow"), False))
            assert final_exact, "Final assistant message lost or changed marker after compaction"
            assert file_exact, "Continuation tool task failed"
            report["passed"] = True
        except Exception as error:
            report["failed_stage"] = stage
            report["failure_type"] = type(error).__name__
            raise
        finally:
            process.terminate()
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
            server.shutdown()
            after = {str(path):sha(path) for path in (args.config,args.auth)}
            report["protected_hashes_after"] = after
            if after != protected:
                report["passed"] = False
                report["protection_error"] = "Original config/auth hash changed"
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2)+"\n")
            stderr.close()
    print(json.dumps(report, indent=2))
    if not report["passed"]: raise SystemExit(1)

if __name__ == "__main__":
    main()

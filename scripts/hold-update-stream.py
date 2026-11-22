#!/usr/bin/env python3
"""Hold one controlled SSE through an already-running private App; never launch it.

serve --case-dir <factory case> stays alive until a separate release command.
Only a new private case with initial, unset cost intent is accepted. This local
synthetic usage proves admission/stream lifetime, never real-provider usage.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import secrets
import threading
import time
import urllib.parse
import urllib.request

from release_common import executable, write_json
from release_install import listener_pids, process_executable

PROVIDER_PREFIX = "update-idle-fixture-"
MODEL = "aubridge-update-hold"
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def read_json(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 4 * 1024 * 1024:
        raise RuntimeError("Expected a bounded regular private JSON file")
    return json.loads(path.read_text())


def private_case(path):
    case = path.resolve(strict=True)
    plan = read_json(case.parent.parent / "fixture-plan.json")
    expected = next((value for value in plan["cases"].values()
                     if Path(value["data_dir"]).parent == case), None)
    if not plan.get("test_only") or expected is None:
        raise RuntimeError("Case must be listed in a generated test-only fixture plan")
    return case, plan, expected


def gateway(case, plan, expected):
    session = read_json(case / "data/gateway-session.json")
    url = urllib.parse.urlsplit(session["endpoint"])
    if (url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port
            or url.username or url.password or url.path not in ("", "/")
            or url.query or url.fragment):
        raise RuntimeError("Private gateway endpoint must be plain loopback")

    def admin(path, body=None, method=None):
        payload = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(session["endpoint"].rstrip("/") + "/admin/" + path,
            payload, {"Authorization": "Bearer " + session["admin_token"],
                      "Content-Type": "application/json"}, method=method)
        with opener.open(request, timeout=10) as response:
            return json.load(response)

    build = admin("build")
    pid = build.get("pid")
    app = Path(expected["install_app"])
    if (not isinstance(pid, int) or process_executable(pid) != str(executable(app))
            or listener_pids(url.port) != {pid} or build.get("source_verified") is not True
            or any(build.get(key) != plan["source_base"][key]
                   for key in ("version", "git_revision", "git_tree"))):
        raise RuntimeError("Session/process/socket/build is not this private App")
    if admin("settings").get("cost_intent") is not None:
        raise RuntimeError("Use a fresh private case with initial unset cost intent; policy is never relaxed")
    return session["endpoint"].rstrip("/"), pid, str(executable(app)), admin


def owned_provider(control, case):
    owner = read_json(control / "owned-provider.json")
    provider = owner.get("provider", {})
    identifier = provider.get("id", "")
    suffix = identifier.removeprefix(PROVIDER_PREFIX)
    base = urllib.parse.urlsplit(provider.get("base", ""))
    if (owner.get("test_only") is not True or owner.get("schema") != 1
            or owner.get("case_dir") != str(case)
            or not identifier.startswith(PROVIDER_PREFIX) or len(suffix) != 32
            or any(char not in "0123456789abcdef" for char in suffix)
            or base.scheme != "http" or base.hostname != "127.0.0.1" or not base.port
            or base.username or base.password or base.path != "/v1" or base.query or base.fragment
            or provider.get("protocol") != "anthropic" or provider.get("models") != [MODEL]
            or provider.get("auth") != "bearer" or provider.get("kind") != "free_tier"):
        raise RuntimeError("Fixture provider ownership manifest does not match this private case")
    return owner


def cleanup_owned_provider(control, case, admin):
    owner = owned_provider(control, case)
    if owner.get("created") is not True:
        raise RuntimeError("Fixture provider was not confirmed created by this run")
    expected = owner["provider"]
    settings = read_json(case / "data/config.json")
    matches = [item for item in settings.get("providers", []) if item.get("id") == expected["id"]]
    if len(matches) != 1 or any(matches[0].get(key) != value for key, value in expected.items()):
        raise RuntimeError("Fixture provider changed after creation; preserving it")
    rows = [item for item in admin("providers")["providers"] if item.get("id") == expected["id"]]
    if (len(rows) != 1 or rows[0].get("source") != "custom" or rows[0].get("key_set") is not False
            or rows[0].get("base") != expected["base"] or rows[0].get("models") != expected["models"]):
        raise RuntimeError("Fixture provider ownership or credential state changed; preserving it")
    admin("providers/" + expected["id"], method="DELETE")


class Hold(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        if self.path != "/v1/messages" or not 0 < length <= 65536:
            self.send_error(400)
            return
        payload = json.loads(self.rfile.read(length))
        if payload.get("model") != MODEL or payload.get("stream") is not True:
            self.send_error(400)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def frame(value):
            self.wfile.write(("event: " + value["type"] + "\ndata: " + json.dumps(value) + "\n\n").encode())
            self.wfile.flush()

        frame({"type": "message_start", "message": {"id": "msg_update_hold", "type": "message",
            "role": "assistant", "model": MODEL, "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 17, "output_tokens": 0}}})
        frame({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        frame({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "controlled-update-hold"}})
        deadline = time.monotonic() + 300
        while not self.server.release_file.exists():
            if time.monotonic() >= deadline:
                self.server.timed_out = True
                return
            self.wfile.write(b": update-hold\n")
            self.wfile.flush()
            time.sleep(0.25)
        self.server.released = True
        frame({"type": "content_block_stop", "index": 0})
        frame({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
               "usage": {"output_tokens": 4}})
        frame({"type": "message_stop"})


def serve(path):
    case, plan, expected = private_case(path)
    endpoint, pid, exe, admin = gateway(case, plan, expected)
    control = case / "stream-control"
    control.mkdir(mode=0o700)  # Refuse reuse of stale flags or evidence.
    provider_id = PROVIDER_PREFIX + secrets.token_hex(16)
    evidence = {"test_only": True, "real_provider_usage": False, "old_app_pid": pid,
                "old_app_executable": exe, "provider_id": provider_id, "phase": "starting",
                "request_id": None, "initial_usage": None, "tail_usage": None,
                "received_stop": False, "eof": False}
    started = time.monotonic()
    created = False
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Hold) as server:
        owner = {"schema": 1, "test_only": True, "case_dir": str(case),
                 "created_by_pid": os.getpid(), "created": False,
                 "provider": {"id": provider_id, "base": f"http://127.0.0.1:{server.server_port}/v1",
                              "protocol": "anthropic", "auth": "bearer", "kind": "free_tier", "models": [MODEL]}}
        write_json(control / "owned-provider.json", owner)
        server.release_file = control / "release"
        server.timed_out = False
        server.released = False
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            provider = owned_provider(control, case)["provider"]
            if any(item.get("id") == provider["id"] for item in admin("providers")["providers"]):
                raise RuntimeError("Generated fixture ID already exists; refusing to alter it")
            admin("providers", provider)
            created = True
            owner["created"] = True
            write_json(control / "owned-provider.json", owner)
            request = urllib.request.Request(endpoint + "/v1/messages", json.dumps({"model": MODEL,
                "stream": True, "max_tokens": 8, "messages": [{"role": "user", "content": "Private controlled admission replay."}]}).encode(),
                {"Authorization": "Bearer aubridge-update-fixture", "Content-Type": "application/json"})
            with opener.open(request, timeout=330) as response:
                evidence["request_id"] = response.headers.get("x-aubridge-request-id")
                if not evidence["request_id"]:
                    raise RuntimeError("Gateway did not return its correlation ID")
                for line in response:
                    if not line.startswith(b"data:"):
                        continue
                    frame = json.loads(line[5:])
                    kind = frame.get("type")
                    if kind == "message_start":
                        evidence["initial_usage"] = frame["message"].get("usage")
                    elif kind == "content_block_delta" and evidence["phase"] == "starting":
                        evidence["phase"] = "holding"
                        evidence["request_limits_while_holding"] = admin("gateway").get("request_limits")
                        write_json(control / "holding.json", evidence)
                        print(f"Holding private App SSE; release through {control}", flush=True)
                    elif kind == "message_delta":
                        evidence["tail_usage"] = frame.get("usage")
                    elif kind == "message_stop":
                        evidence["received_stop"] = True
                evidence["eof"] = True
            if (not server.released or server.timed_out or not evidence["received_stop"]
                    or evidence["tail_usage"].get("output_tokens") != 4
                    or process_executable(pid) != exe):
                raise RuntimeError("Held stream did not finish naturally on the original App")
            evidence["phase"] = "complete"
        except Exception as error:
            evidence["phase"] = "failed"
            evidence["error_type"] = type(error).__name__
            raise
        finally:
            server.release_file.touch(exist_ok=True)
            server.shutdown()
            if created:
                try:
                    cleanup_owned_provider(control, case, admin)
                    evidence["fixture_provider_removed"] = True
                except Exception as error:
                    evidence["fixture_provider_removed"] = False
                    evidence["cleanup_error_type"] = type(error).__name__
            evidence["elapsed_seconds"] = round(time.monotonic() - started, 3)
            write_json(control / "stream-evidence.json", evidence)
    print(f"Stream complete; evidence: {control / 'stream-evidence.json'}", flush=True)


def release(path):
    case, _, _ = private_case(path)
    control = case / "stream-control"
    if read_json(control / "holding.json").get("phase") != "holding":
        raise RuntimeError("No acknowledged held stream")
    flag = control / "release"
    with flag.open("xb") as output:
        os.chmod(flag, 0o600)
        output.write(b"release\n")
    print("Requested natural fixture stream completion")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("serve", "release"))
    parser.add_argument("--case-dir", required=True, type=Path)
    args = parser.parse_args()
    (serve if args.action == "serve" else release)(args.case_dir)


if __name__ == "__main__":
    main()

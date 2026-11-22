#!/usr/bin/env python3
"""Production HTTP -> external client file read -> result -> exact answer.
Usage: AUBRIDGE_TEST_TOKEN=... python3 scripts/accept-claude-cli.py http://127.0.0.1:3427
The server must enable its native Claude CLI channel. No secret is printed.
"""
import json
import os
import secrets
import sys
import tempfile
import urllib.request
from pathlib import Path

base = sys.argv[1].rstrip("/")
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
receipts = []
token = os.environ.get("AUBRIDGE_TEST_TOKEN", "aubridge")
with tempfile.TemporaryDirectory(prefix="aubridge-external-client-") as directory:
    paths = [Path(directory) / "first.txt", Path(directory) / "second.txt"]
    expected = [secrets.token_hex(24), secrets.token_hex(24)]
    for path, value in zip(paths, expected):
        path.write_text(value)
    tools = [{"name": "read_random_file", "description": "Read the requested local file through the external client.",
              "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}]
    messages = [{"role": "user", "content": f"First call read_random_file to read {paths[0]}. After receiving that result, make a separate read_random_file call for {paths[1]}. Do not combine the two calls. After both results reply with exactly the two values separated by a colon, no punctuation or explanation."}]
    def post():
        body = json.dumps({"model": "sonnet", "max_tokens": 256, "stream": False,
                           "metadata": {"user_id": "claude-cli-acceptance"}, "tools": tools, "messages": messages}).encode()
        req = urllib.request.Request(base + "/v1/messages", body, {"content-type": "application/json", "x-api-key": token})
        with opener.open(req, timeout=150) as response:
            assert response.headers["x-aubridge-provider"] == "claude-cli"
            body = json.load(response)
            receipts.append({"request_id": response.headers.get("x-aubridge-request-id"), "provider": response.headers.get("x-aubridge-provider"), "served_model": body.get("model")})
            return body
    response = post()
    call_count = 0
    seen = set()
    for turn in range(2):
        calls = [block for block in response["content"] if block["type"] == "tool_use"]
        assert len(calls) == 1, "expected two separate sequential tool rounds"
        messages.append({"role": "assistant", "content": response["content"]})
        results = []
        for call in reversed(calls):
            assert call["id"] not in seen
            seen.add(call["id"])
            assert call["name"] == "read_random_file"
            assert Path(call["input"]["path"]) == paths[turn]
            results.append({"type": "tool_result", "tool_use_id": call["id"], "content": paths[turn].read_text()})
            call_count += 1
        messages.append({"role": "user", "content": results})
        response = post()
    answer = "".join(block.get("text", "") for block in response["content"] if block["type"] == "text").strip()
    assert answer == ":".join(expected), "final answer did not match the external file contents"
    print(json.dumps({"provider": "claude-cli", "external_tool_rounds": 2, "external_tool_calls": call_count, "exact_final_answer": True, "receipts": receipts}))

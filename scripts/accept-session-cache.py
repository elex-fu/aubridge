#!/usr/bin/env python3
"""Repeat a synthetic long prefix through an already isolated native Responses route.

Luna: first record candidate version/digest and original user config SHA. Use a
private gateway/HOME with copied credentials (never live refresh); this script
only reads gateway key files and never changes a client config. Only a native
Responses route preserves the vendor usage object for this acceptance.

AUBRIDGE_CALLER_KEY_FILE=/private/caller AUBRIDGE_ADMIN_KEY_FILE=/private/admin
rtk proxy python3 scripts/accept-session-cache.py --endpoint http://127.0.0.1:PORT
    --model ACTUAL_MODEL --out PRIVATE_EVIDENCE_DIRECTORY

Exit 0 requires a positive upstream cache read on a repeat, matching persisted
counts, and the same source/model. Exit 2 means honest zero/unknown evidence,
not successful cache optimization. No prompt, response text or key is saved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import urllib.error
import urllib.parse
import urllib.request


def key(name):
    return Path(os.environ[name]).read_text().strip()


def call(endpoint, path, token, payload=None, session=None):
    headers = {"Authorization": "Bearer " + token}
    if session:
        headers["x-session-id"] = session
        headers["x-aubridge-request-purpose"] = "normal"
    data = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode()
    request = urllib.request.Request(endpoint + path, data=data, headers=headers)
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=180) as response:
        return response.read(), response.headers.get("x-aubridge-request-id")


def response_usage(raw):
    objects = []
    try:
        objects.append(json.loads(raw))
    except (ValueError, UnicodeDecodeError):
        for line in raw.decode().splitlines():
            if line.startswith("data:"):
                try:
                    objects.append(json.loads(line[5:].strip()))
                except ValueError:
                    pass
    for obj in reversed(objects):
        response = obj.get("response", obj)
        if isinstance(response, dict) and isinstance(response.get("usage"), dict):
            return response.get("model"), response["usage"]
    return None, None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    endpoint = args.endpoint.rstrip("/")
    parsed = urllib.parse.urlparse(endpoint)
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
        parser.error("use an isolated loopback gateway")
    if not 2 <= args.repeats <= 8:
        parser.error("repeats must be 2..8")
    args.out.mkdir(parents=True, exist_ok=False, mode=0o700)
    caller, admin = key("AUBRIDGE_CALLER_KEY_FILE"), key("AUBRIDGE_ADMIN_KEY_FILE")
    session = "cache-accept-" + secrets.token_hex(12)
    prefix = "You are checking a synthetic reference. Reply only OK.\n" + "\n".join(
        f"Reference row {i:04d}: amber birch cedar delta elm frost grove hazel."
        for i in range(1024)
    )
    evidence = {"session_id": session, "requested_model": args.model,
                "prefix_sha256": hashlib.sha256(prefix.encode()).hexdigest(),
                "prefix_bytes": len(prefix.encode()), "calls": [],
                "scope": "native Responses vendor usage + persisted diagnostics; synthetic text only"}
    for index in range(args.repeats):
        raw, request_id = call(endpoint, "/v1/responses", caller, {
            "model": args.model, "instructions": prefix, "prompt_cache_key": session, "store": False, "stream": True,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": f"Check {index + 1}."}]}],
        }, session)
        actual_model, usage = response_usage(raw)
        if not request_id:
            raise RuntimeError("missing trusted gateway request ID")
        history, _ = call(endpoint, "/admin/gateway?" + urllib.parse.urlencode({"request_id": request_id}), admin)
        rows = json.loads(history).get("requests") or []
        if len(rows) != 1:
            raise RuntimeError("request not found exactly once")
        row = rows[0]
        if row.get("session_id") != session:
            raise RuntimeError("explicit session identity did not survive diagnostics")
        details = (usage or {}).get("input_tokens_details") or {}
        upstream_read = details.get("cached_tokens")
        if upstream_read is not None and upstream_read != row.get("cache_read_tokens"):
            raise RuntimeError("raw native upstream cache count differs from ledger")
        evidence["calls"].append({"request_id": request_id, "native_usage": usage,
            "response_model": actual_model, "provider": row.get("provider"),
            "served_model": row.get("served_model"), "outcome": row.get("outcome"),
            "cache_read_tokens": row.get("cache_read_tokens"),
            "cache_write_tokens": row.get("cache_write_tokens"),
            "attempts": row.get("attempts"), "upstream_read": upstream_read})
        (args.out / "receipt.json").write_text(json.dumps(evidence, indent=2))
    calls = evidence["calls"]
    stable = (len({(r["provider"], r["served_model"]) for r in calls}) == 1
              and all(r["provider"] and r["served_model"] and
                      r["response_model"] == r["served_model"] for r in calls))
    positive = stable and all(r["outcome"] == "ok" for r in calls) and any(
        isinstance(r["upstream_read"], int) and r["upstream_read"] > 0 for r in calls[1:])
    evidence["same_actual_source_model"] = stable
    evidence["positive_cache_read_proven"] = positive
    evidence["result"] = "positive native upstream cache read" if positive else "zero, unknown, failed, or source/model changed; no cache optimization claim"
    summary, _ = call(endpoint, "/admin/gateway/sessions?" + urllib.parse.urlencode({"session_id": session}), admin)
    evidence["session_summary"] = json.loads(summary)
    (args.out / "receipt.json").write_text(json.dumps(evidence, indent=2))
    print(json.dumps({"evidence": str(args.out / "receipt.json"), "result": evidence["result"]}))
    return 0 if positive else 2


if __name__ == "__main__":
    raise SystemExit(main())

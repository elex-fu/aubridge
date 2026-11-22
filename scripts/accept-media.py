#!/usr/bin/env python3
"""Real vision acceptance through an already running production gateway.
Environment: AUBRIDGE_GATEWAY_URL, AUBRIDGE_MEDIA_MODEL, AUBRIDGE_CLIENT_KEY.
Optional AUBRIDGE_MEDIA_ORACLE_PATH reuses an owned 0600 PNG/oracle fixture.
Ingress: messages (default), chat/completions, responses (SSE), generateContent.
Optional AUBRIDGE_MEDIA_SESSION_ID is a caller correlation label, not a native ID.
Does not read provider credentials, modify login/config, or change system proxy.
Stdlib PNG drawing; only the image holds the random answer sent to the model.
"""
import base64
import hashlib
import json
import os
import secrets
import stat
from pathlib import Path
import struct
import urllib.parse
import urllib.request
import zlib

DIGITS = [
    ["11111","10001","10001","10001","10001","10001","11111"],
    ["00100","01100","00100","00100","00100","00100","01110"],
    ["11111","00001","00001","11111","10000","10000","11111"],
    ["11111","00001","00001","11111","00001","00001","11111"],
    ["10001","10001","10001","11111","00001","00001","00001"],
    ["11111","10000","10000","11111","00001","00001","11111"],
    ["11111","10000","10000","11111","10001","10001","11111"],
    ["11111","00001","00010","00100","01000","01000","01000"],
    ["11111","10001","10001","11111","10001","10001","11111"],
    ["11111","10001","10001","11111","00001","00001","11111"],
]

def png(answer):
    scale, margin = 14, 28
    width, height = 4 * 6 * scale + margin * 2, 7 * scale + margin * 2
    pixels = bytearray([255]) * (width * height * 3)
    for n, digit in enumerate(answer):
        for y, row in enumerate(DIGITS[int(digit)]):
            for x, value in enumerate(row):
                if value != "1":
                    continue
                for dy in range(scale):
                    for dx in range(scale):
                        at = ((margin + y * scale + dy) * width + margin + (n * 6 + x) * scale + dx) * 3
                        pixels[at:at+3] = b"\x00\x00\x00"
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    scanlines = b"".join(b"\x00" + pixels[y*width*3:(y+1)*width*3] for y in range(height))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB",width,height,8,2,0,0,0)) + chunk(b"IDAT",zlib.compress(scanlines)) + chunk(b"IEND",b"")

def image_and_oracle():
    oracle_path = os.environ.get("AUBRIDGE_MEDIA_ORACLE_PATH")
    if not oracle_path:
        answer = str(secrets.randbelow(9000) + 1000)
        return answer, png(answer)
    path = Path(oracle_path).expanduser()
    info = path.stat()
    if stat.S_IMODE(info.st_mode) != 0o600 or info.st_uid != os.getuid() or info.st_size > 4 * 1024 * 1024:
        raise SystemExit("Oracle must be an owned 0600 file no larger than 4 MiB")
    with path.open() as source:
        oracle = json.load(source)
    answer = oracle["answer"]
    if not isinstance(answer, str) or len(answer) != 4 or not answer.isascii() or not answer.isdigit():
        raise SystemExit("Oracle must contain a four digit answer")
    image = base64.b64decode(oracle["png_base64"], validate=True)
    if not image.startswith(b"\x89PNG\r\n\x1a\n") or hashlib.sha256(image).hexdigest() != oracle["sha256"]:
        raise SystemExit("Oracle PNG hash does not match")
    return answer, image


def responses_sse(response):
    if response.headers.get_content_type() != "text/event-stream":
        raise ValueError("responses_content_type")
    raw = response.read(4 * 1024 * 1024 + 1)
    if len(raw) > 4 * 1024 * 1024:
        raise ValueError("responses_size_limit")
    frames = raw.decode("utf-8-sig").replace("\r\n", "\n").replace("\r", "\n").split("\n\n")
    if frames.pop().strip():
        raise ValueError("responses_truncated_frame")
    completed, ended = None, False
    for frame in frames:
        lines = frame.splitlines()
        data = "\n".join(line[5:].lstrip(" ") for line in lines if line.startswith("data:"))
        if not data:
            continue
        if data == "[DONE]" and completed is not None and not ended:
            ended = True
            continue
        if ended or completed is not None:
            raise ValueError("responses_after_terminal")
        event = json.loads(data)
        kind = event.get("type", "")
        names = [line[6:].strip() for line in lines if line.startswith("event:")]
        if names and names != [kind]:
            raise ValueError("responses_event_mismatch")
        if event.get("error") is not None or kind in ("error", "response.failed", "response.incomplete") or any(word in kind for word in ("function_call", "custom_tool_call", "refusal")):
            raise ValueError("responses_unsuccessful_event")
        nested = event.get("response", {})
        if nested.get("error") is not None or nested.get("incomplete_details") is not None or nested.get("status") in ("failed", "incomplete") or event.get("part", {}).get("type") == "refusal":
            raise ValueError("responses_unsuccessful_payload")
        if event.get("item", {}).get("type", "").endswith("_call") or event.get("item", {}).get("status") in ("failed", "incomplete"):
            raise ValueError("responses_tool_call")
        if kind == "response.completed":
            completed = event["response"]
    if completed is None:
        raise ValueError("responses_missing_terminal")
    return completed


def safe_usage(value):
    keys = ("input_tokens", "output_tokens", "total_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "prompt_tokens", "completion_tokens", "promptTokenCount", "candidatesTokenCount", "totalTokenCount", "cachedContentTokenCount", "thoughtsTokenCount", "toolUsePromptTokenCount", "cached_tokens", "cache_write_tokens", "reasoning_tokens")
    return {key: item for key, item in value.items() if key in keys and type(item) is int and item >= 0} if isinstance(value, dict) else {}


def main():
    base = os.environ["AUBRIDGE_GATEWAY_URL"].rstrip("/")
    model = os.environ["AUBRIDGE_MEDIA_MODEL"]
    key = os.environ["AUBRIDGE_CLIENT_KEY"]
    ingress = os.environ.get("AUBRIDGE_MEDIA_INGRESS", "messages")
    answer, image = image_and_oracle()
    data = base64.b64encode(image).decode()
    max_tokens = 2048
    prompt = "Read the four digit number shown in this image. Reply with the digits only."
    if ingress == "chat/completions":
        body = {"model":model,"stream":False,"max_tokens":max_tokens,"messages":[{"role":"user","content":[{"type":"text","text":prompt},{"type":"image_url","image_url":{"url":"data:image/png;base64,"+data}}]}]}
    elif ingress == "messages":
        body = {"model":model,"stream":False,"max_tokens":max_tokens,"messages":[{"role":"user","content":[{"type":"text","text":prompt},{"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]}]}
    elif ingress == "responses":
        body = {"model":model,"stream":True,"max_output_tokens":max_tokens,"input":[{"role":"user","content":[{"type":"input_text","text":prompt},{"type":"input_image","image_url":"data:image/png;base64,"+data}]}]}
    elif ingress == "generateContent":
        body = {"contents":[{"role":"user","parts":[{"text":prompt},{"inlineData":{"mimeType":"image/png","data":data}}]}],"generationConfig":{"maxOutputTokens":max_tokens}}
    else:
        raise SystemExit("AUBRIDGE_MEDIA_INGRESS must be messages, chat/completions, responses or generateContent")
    path = "/v1beta/models/"+urllib.parse.quote(model, safe="/:")+":generateContent" if ingress == "generateContent" else "/v1/"+ingress
    headers = {"Content-Type":"application/json","Authorization":"Bearer "+key}
    session = os.environ.get("AUBRIDGE_MEDIA_SESSION_ID")
    if session:
        headers["x-session-id"] = session
    if ingress == "responses":
        headers["Accept"] = "text/event-stream"
    request = urllib.request.Request(base+path, data=json.dumps(body).encode(), headers=headers)
    # Gateway endpoint is local; upstream networking remains owned by the gateway.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    receipt = {}
    try:
        with opener.open(request, timeout=90) as response:
            receipt = {name: response.headers.get(name) for name in ["x-aubridge-request-id", "x-aubridge-provider", "x-aubridge-model"]}
            result = responses_sse(response) if ingress == "responses" else json.load(response)
    except Exception as error:
        if getattr(error, "headers", None):
            receipt = {name: error.headers.get(name) for name in ["x-aubridge-request-id", "x-aubridge-provider", "x-aubridge-model"]}
        print(json.dumps({"pass":False,"ingress":ingress,"failure":type(error).__name__,"http_status":getattr(error,"code",None),"receipt":receipt}))
        raise SystemExit(1) from None
    if ingress == "messages":
        text = "".join(part.get("text", "") for part in result.get("content", []) if part.get("type") == "text")
        terminal_reason = result.get("stop_reason")
        terminal_complete = terminal_reason == "end_turn" and result.get("type") == "message" and not any("tool" in part.get("type", "") for part in result.get("content", []))
    elif ingress == "chat/completions":
        choices = result.get("choices", [])
        choice = choices[0] if len(choices) == 1 else {}
        text = choice.get("message", {}).get("content") or ""
        terminal_reason = choice.get("finish_reason")
        terminal_complete = terminal_reason == "stop" and not choice.get("message", {}).get("tool_calls") and not choice.get("message", {}).get("function_call") and not choice.get("error")
    elif ingress == "responses":
        items = result.get("output", [])
        parts = [part for item in items if item.get("type") == "message" for part in item.get("content", [])]
        text = "".join(part.get("text", "") for part in parts if part.get("type") == "output_text")
        terminal_reason = result.get("status")
        terminal_complete = result.get("object") == "response" and terminal_reason == "completed" and result.get("incomplete_details") is None and all((item.get("type") == "reasoning" and item.get("status") in (None, "completed")) or (item.get("type") == "message" and item.get("status") == "completed" and item.get("role") == "assistant") for item in items) and all(part.get("type") == "output_text" for part in parts)
    else:
        candidates = result.get("candidates", [])
        candidate = candidates[0] if len(candidates) == 1 else {}
        parts = candidate.get("content", {}).get("parts", [])
        text = "".join(part.get("text", "") for part in parts if not part.get("thought"))
        terminal_reason = candidate.get("finishReason")
        terminal_complete = terminal_reason == "STOP" and candidate.get("content", {}).get("role") == "model" and not result.get("promptFeedback", {}).get("blockReason") and all("functionCall" not in part and "functionResponse" not in part for part in parts)
    matched = isinstance(text, str) and text.strip() == answer
    passed = matched and terminal_complete and result.get("error") is None
    raw_usage = result.get("usageMetadata" if ingress == "generateContent" else "usage") or {}
    usage = safe_usage(raw_usage)
    for name in ("input_tokens_details", "output_tokens_details", "prompt_tokens_details", "completion_tokens_details"):
        if safe_usage(raw_usage.get(name)):
            usage[name] = safe_usage(raw_usage[name])
    print(json.dumps({"model":model,"ingress":ingress,"max_tokens":max_tokens,"image_sha256":hashlib.sha256(image).hexdigest(),"matched":matched,"terminal_reason":terminal_reason,"terminal_complete":terminal_complete,"pass":passed,"served_model":result.get("modelVersion" if ingress == "generateContent" else "model"),"receipt":receipt,"usage":usage,"caller_session_id":session,"session_source":"caller-explicit" if session else None},ensure_ascii=False))
    if not passed:
        raise SystemExit(1)

if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"pass":False,"failure":type(error).__name__}))
        raise SystemExit(1) from None

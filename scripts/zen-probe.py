#!/usr/bin/env python3
"""Zen 通道可行性探测(pre-design 验证,非产品代码)。

验证结论(2026-11-14 实测,详见脚本输出):
  协议层 ✅ 可行 —— 鉴权头、网关路径、模型目录全部打通;
  配额层 ❌ 当前账号无可用额度(免费额度耗尽 / 无余额 / 无 Go 订阅)。

依据(OpenCode Desktop 2.2.5 二进制提取 + 实测核实):
  - 网关:openai 兼容  https://opencode.ai/inference/openai/v1/chat/completions
          Responses    https://opencode.ai/inference/openai/v1/responses
          Anthropic    https://opencode.ai/inference/anthropic/v1/messages
  - 模型目录:GET https://opencode.ai/console/api/v1/models(模型条目
    自带 api.{chat-completions|messages|responses|generate-content}.base_url,
    即协议→网关的映射就在目录里,无需硬编码)。
  - 鉴权:Authorization: Bearer <access>(39 字节 opaque token,非 JWT)
    + x-opencode-org-id: <metadata.orgID>。
  - 凭据:SQLite credential 表 integration_id='opencode',type='oauth',
    value JSON {access, refresh, expires(ms), metadata{server, orgID,...}}。

Cloudflare 说明:opencode.ai 有浏览器完整性检查,Python urllib 默认 UA
被 1010 拒绝;带普通 UA(如 curl)即可通过,无需 TLS 指纹伪装。

纪律:
  - 只读打开 opencode.db(URI mode=ro),绝不写;
  - 凭据值不打印、不落盘(输出只含长度/到期时间/服务器 URL/错误消息);
  - token 只发往其签发方 opencode.ai(用户本人订阅的合法使用);
  - 不触发刷新(刷新协议未验证,token 到期前只做直连验证)。
"""

import json
import sqlite3
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

DB = Path.home() / ".local" / "share" / "opencode" / "opencode.db"
# 任意非浏览器完整性黑名单 UA 即可;与 OpenCode 客户端无指纹关系。
UA = "aubridge-zen-probe/0.1"
CHAT_URL = "https://opencode.ai/inference/openai/v1/chat/completions"
MODELS_URL = "https://opencode.ai/console/api/v1/models"
TIMEOUT = 20
# 目录里 price=0 的 chat-completions 模型轮询到第一个非 429 的为止。
FREE_CHAT_MODELS = [
    "big-pickle", "iceman-free", "flint-40b-free", "glm-5.3-flash-free",
]


def load_zen_credential():
    """只读读取 credential 表中 integration_id='opencode' 的 oauth 条目。"""
    if not DB.exists():
        sys.exit(f"FAIL: {DB} 不存在")
    conn = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    try:
        rows = conn.execute(
            "SELECT active, value FROM credential WHERE integration_id = 'opencode'"
        ).fetchall()
    finally:
        conn.close()
    # 多行共存时(如旧登录残留),active=1 是客户端当前使用的行;
    # 旧行可能是已耗尽/失效的另一 token 实例(2026-11-14 实测差异根因)。
    oauth = [(active, json.loads(v)) for active, v in rows
             if json.loads(v).get("type") == "oauth"]
    if not oauth:
        sys.exit("FAIL: credential 表无 opencode oauth 条目")
    active_rows = [c for active, c in oauth if active == 1]
    with_org = [c for c in active_rows if c.get("metadata", {}).get("orgID")]
    return (with_org or active_rows or [c for _, c in oauth])[0]


def request(method, url, cred, body=None):
    h = {
        "Authorization": f"Bearer {cred['access']}",
        "x-opencode-org-id": cred["metadata"]["orgID"],
        "User-Agent": UA,
    }
    if body is not None:
        h["Content-Type"] = "application/json"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            return resp.status, resp.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")[:800]
    except Exception as e:  # noqa: BLE001 - 探测脚本,报告即可
        return None, f"{type(e).__name__}: {e}"


def main():
    cred = load_zen_credential()
    expires = cred.get("expires")
    server = cred.get("metadata", {}).get("server")
    print(f"metadata.server: {server}")
    print(f"access 长度: {len(cred.get('access', ''))}(opaque,非 JWT), "
          f"refresh 存在: {bool(cred.get('refresh'))}")
    if expires:
        remaining = (expires / 1000) - time.time()
        print(f"token 剩余: {remaining / 3600:.1f}h")
        if remaining < 0:
            print("FAIL: access 已过期;刷新协议未验证,放弃直连探测")
            return
    print()

    # 1. 模型目录(console API,鉴权 + org 头)。
    status, body = request("GET", MODELS_URL, cred)
    print(f"[1] GET console/api/v1/models -> {status}")
    models = {}
    if status == 200:
        try:
            catalog = json.loads(body)
            for info in catalog.values():
                models.update(info.get("models", {}))
            free = [m for m, v in models.items()
                    if v.get("cost", {}).get("text", {}).get("input", {}).get("price") == 0]
            print(f"    模型总数 {len(models)},免费 {len(free)};样例: {free[:4]}")
        except json.JSONDecodeError:
            print(f"    非 JSON: {body[:200]}")
    else:
        print(f"    {body[:300]}")
    print()

    # 2. 真实 chat completion(openai 兼容网关)。
    print(f"[2] POST {CHAT_URL}")
    for model in FREE_CHAT_MODELS:
        payload = {"model": model,
                   "messages": [{"role": "user", "content": "Reply with exactly: zen-ok"}],
                   "max_tokens": 32}
        status, body = request("POST", CHAT_URL, cred, payload)
        if status == 200:
            try:
                out = json.loads(body)["choices"][0]["message"]["content"]
                print(f"    model={model} -> 200,回复: {out!r}")
                print("VERDICT: Zen 通道完整可用(鉴权+推理+openai-chat 协议)")
            except (json.JSONDecodeError, KeyError, IndexError):
                print(f"    model={model} -> 200,非标准响应: {body[:400]}")
            return
        # 4xx JSON 错误体 = 已过鉴权与路由,属业务层(配额/权限/区域),
        # 协议本身已验证;换下一个免费模型继续找非 429 的。
        err = ""
        try:
            err = json.loads(body).get("error", {}).get("message", body[:120])
        except json.JSONDecodeError:
            err = body[:120]
        print(f"    model={model} -> {status}: {err}")
        if status not in (429, 403, 402):
            print("VERDICT: 未知错误形态,需人工分析")
            return
    print("VERDICT: 协议层可行(错误均为业务层:配额/权限/区域);"
          "本账号当前无可用额度,成功补全未演示")


if __name__ == "__main__":
    main()

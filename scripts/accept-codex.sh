#!/bin/zsh
# chatgpt(Codex 订阅,Responses 协议)代理能力验收(设计文档 §6.3/§7 订阅源)。
# 协议:Responses→Anthropic 翻译(OAuth 凭据 ~/.codex/auth.json,401 代刷新)。
#
# 前置:
#   1. 已跑 codex login(~/.codex/auth.json 为 chatgpt OAuth 文件模式)
#   2. 本机 chatgpt.com DNS 污染,默认走本地代理 127.0.0.1:7890
#      (reqwest 读 HTTPS_PROXY;环境变量或 .env 可覆盖)
# 用法:./scripts/accept-codex.sh [模型id,默认 gpt-6.1-sol]
set -euo pipefail
cd "${0:A:h:h}"
source scripts/accept-gateway.zsh

MODEL="${1:-gpt-6.1-sol}"
T=$(mktemp -d /tmp/aubridge-codex.XXXXXX)
trap cleanup_accept_gateway EXIT
SECRET="codex-accept-$RANDOM"

AUTH_JSON="$HOME/.codex/auth.json"
[[ -f "$AUTH_JSON" ]] || { echo "缺少 $AUTH_JSON(先 codex login)"; exit 2; }
# 只探测凭据形状,不打印 token 值(安全红线)。
python3 - <<'PY' || { echo "auth.json 无 OAuth tokens(keyring 模式 v1 不支持)"; exit 2; }
import json, os, sys
t = json.load(open(os.path.expanduser("~/.codex/auth.json"))).get("tokens", {})
sys.exit(0 if t.get("access_token") and t.get("refresh_token") else 1)
PY

# 代理:环境变量 > .env > 本地默认(7890)。
if [[ -z "${HTTPS_PROXY:-}" ]] && grep -q '^HTTPS_PROXY=' .env 2>/dev/null; then
  export HTTPS_PROXY=$(grep '^HTTPS_PROXY=' .env | cut -d= -f2-)
fi
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:7890}"

build_accept_gateway
start_accept_gateway "$T"

# 1) 协议面直测:Messages 请求 → Responses 翻译 → Anthropic SSE 应答。
echo "== 直测 /v1/messages(model ${MODEL},responses 翻译)=="
RESP=$(curl -s --noproxy '*' -D "$T/hdrs" --max-time 120 "$GW/v1/messages" \
  -H "authorization: Bearer aubridge-claude-code" \
  -H "content-type: application/json" \
  -d "{\"model\":\"${MODEL}\",\"max_tokens\":128,
       \"messages\":[{\"role\":\"user\",\"content\":\"只回复两个字:收到\"}]}")
PROVIDER=$(grep -i x-aubridge-provider "$T/hdrs" | tr -d '\r' | awk '{print $2}')
echo "应答源: ${PROVIDER:-无}"
echo "$RESP" | head -c 400; echo
[[ "$PROVIDER" == "chatgpt" ]] || { echo "❌ 应答源不是 chatgpt(候选循环未选中)"; cat "$T/gateway.log"; exit 1; }
echo "$RESP" | grep -q "message_start" || { echo "❌ 无 Anthropic 事件流"; cat "$T/gateway.log"; exit 1; }
echo "$RESP" | grep -q "message_stop" || { echo "❌ 流未正常终止"; exit 1; }
echo "$RESP" | grep -q '"error"' && { echo "❌ 出现错误事件"; exit 1; } || true

# 2) 真实会话:工具调用循环(隔离 CLAUDE_CONFIG_DIR)。
mkdir -p "$T/claude"
cat >"$T/claude/settings.json" <<EOF
{"env": {"ANTHROPIC_BASE_URL": "${GW}", "ANTHROPIC_AUTH_TOKEN": "aubridge-claude-code"},
 "model": "${MODEL}",
 "permissions": {"allow": ["Read"]}}
EOF
echo "codex 验收秘密:${SECRET}" > "$T/secret.txt"
echo "== claude -p 会话(${MODEL} 经 aubridge → chatgpt)=="
(cd "$T"; CLAUDE_CONFIG_DIR="$T/claude" DISABLE_NONESSENTIAL_TRAFFIC=1 \
  claude -p --model "$MODEL" --tools Read --allowedTools Read --strict-mcp-config --no-session-persistence -- \
  "先用 Read 工具读取 $T/secret.txt,然后原样报告其中的秘密内容。" \
  ) > "$T/session.txt" 2>&1 || { cat "$T/session.txt"; cat "$T/gateway.log"; exit 1; }
cat "$T/session.txt"
grep -q "$SECRET" "$T/session.txt" \
  && echo "✅ chatgpt 验收通过:Responses 翻译 + OAuth 凭据 + 真实工具调用循环" \
  || { echo "❌ 会话未回传文件内容"; cat "$T/gateway.log"; exit 1; }

#!/bin/zsh
# kimi-code(Kimi Coding Plan)代理能力验证(设计文档 §7 C 类订阅源)。
# 协议:Anthropic 原生透传(model-router profiles.ts kimi-code 字段核实)。
#
# 前置:.env 或环境变量 KEY_KIMI_CODE=<你的 Kimi Coding Plan key>
# 用法:./scripts/accept-kimi.sh [模型id,默认 kimi-k3]
set -euo pipefail
cd "${0:A:h:h}"
source scripts/accept-gateway.zsh

MODEL="${1:-kimi-k3}"
T=$(mktemp -d /tmp/aubridge-kimi.XXXXXX)
trap cleanup_accept_gateway EXIT
SECRET="kimi-accept-$RANDOM"

KEY="${KEY_KIMI_CODE:-}"
[[ -z "$KEY" ]] && [[ -f .env ]] && KEY=$(grep '^KEY_KIMI_CODE=' .env | cut -d= -f2-)
if [[ -z "$KEY" ]]; then
  echo "缺少 KEY_KIMI_CODE(.env 模板见 .env.example)" >&2
  exit 2
fi

build_accept_gateway
KEY_KIMI_CODE="$KEY" start_accept_gateway "$T"

# 1) 协议面直测:Messages 请求 → 透传 → Anthropic SSE 应答。
echo "== 直测 /v1/messages(model ${MODEL},anthropic 透传)=="
RESP=$(curl -s --noproxy '*' -D "$T/hdrs" "$GW/v1/messages" \
  -H "authorization: Bearer aubridge-claude-code" \
  -H "content-type: application/json" \
  -d "{\"model\":\"${MODEL}\",\"max_tokens\":64,
       \"messages\":[{\"role\":\"user\",\"content\":\"只回复两个字:收到\"}]}")
PROVIDER=$(grep -i x-aubridge-provider "$T/hdrs" | tr -d '\r' | awk '{print $2}')
echo "应答源: ${PROVIDER:-无}"
echo "$RESP" | head -c 400; echo
[[ "$PROVIDER" == "kimi-code" ]] || { echo "❌ 应答源不是 kimi-code(候选循环未选中)"; cat "$T/gateway.log"; exit 1; }
echo "$RESP" | grep -q "message_start" || { echo "❌ 无 Anthropic 事件流"; exit 1; }

# 2) 真实会话:工具调用循环(隔离 CLAUDE_CONFIG_DIR)。
mkdir -p "$T/claude"
cat >"$T/claude/settings.json" <<EOF
{"env": {"ANTHROPIC_BASE_URL": "${GW}", "ANTHROPIC_AUTH_TOKEN": "aubridge-claude-code"},
 "model": "${MODEL}",
 "permissions": {"allow": ["Read"]}}
EOF
echo "kimi 验收秘密:${SECRET}" > "$T/secret.txt"
echo "== claude -p 会话(${MODEL} 经 aubridge → Kimi)=="
(cd "$T"; CLAUDE_CONFIG_DIR="$T/claude" DISABLE_NONESSENTIAL_TRAFFIC=1 \
  claude -p --model "$MODEL" --tools Read --allowedTools Read --strict-mcp-config --no-session-persistence -- \
  "先用 Read 工具读取 $T/secret.txt,然后原样报告其中的秘密内容。" \
  ) > "$T/session.txt" 2>&1 || { cat "$T/session.txt"; exit 1; }
cat "$T/session.txt"
grep -q "$SECRET" "$T/session.txt" \
  && echo "✅ kimi-code 验收通过:Anthropic 透传 + 真实工具调用循环" \
  || { echo "❌ 会话未回传文件内容"; exit 1; }

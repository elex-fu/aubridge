#!/bin/zsh
# aubridge M1 真实验收(设计文档 §10:Claude Code 真实编码会话,
# 含工具调用循环,经免费上游 GLM-4.5-Flash)。
#
# 前置:KEY_ZHIPU=<你的智谱 API key>(open.bigmodel.cn 免费注册)
# 用法:./scripts/accept-m1.sh
set -euo pipefail
cd "${0:A:h:h}"
source scripts/accept-gateway.zsh

KEY="${KEY_ZHIPU:-}"
if [[ -z "$KEY" ]]; then
  echo "缺少 KEY_ZHIPU(智谱开放平台免费注册:https://open.bigmodel.cn)" >&2
  exit 2
fi

MODEL="glm-4.5-flash"
T=$(mktemp -d /tmp/aubridge-accept.XXXXXX)
trap cleanup_accept_gateway EXIT
SECRET="aubridge-accept-$RANDOM"

# 1) 起网关(真实 preset,后台),退出时清理。
build_accept_gateway
KEY_ZHIPU="$KEY" start_accept_gateway "$T"

# 2) 隔离接线 Claude Code(临时 CLAUDE_CONFIG_DIR,不碰 ~/.claude)。
mkdir -p "$T/claude"
cat >"$T/claude/settings.json" <<EOF
{
  "env": {
    "ANTHROPIC_BASE_URL": "${GW}",
    "ANTHROPIC_AUTH_TOKEN": "aubridge-claude-code"
  },
  "model": "${MODEL}",
  "permissions": {"allow": ["Read"]}
}
EOF
echo "M1 验收秘密内容:${SECRET}" > "$T/secret.txt"

# 3) 真实会话:工具调用循环(Read 工具读文件再回答)。
echo "== claude -p 会话(模型 ${MODEL},经 aubridge → 智谱)=="
(cd "$T"; CLAUDE_CONFIG_DIR="$T/claude" DISABLE_NONESSENTIAL_TRAFFIC=1 \
  claude -p --model "$MODEL" --tools Read --allowedTools Read --strict-mcp-config --no-session-persistence -- \
  "先用 Read 工具读取 $T/secret.txt 文件,然后原样报告其中的秘密内容。" \
  ) > "$T/session.txt" 2>&1 || { cat "$T/session.txt"; exit 1; }
cat "$T/session.txt"

# 4) 断言:秘密内容回传(证明 Read 工具调用回路真实走通)。
grep -q "$SECRET" "$T/session.txt" \
  && echo "✅ M1 验收通过:真实工具调用循环(claude → aubridge → 智谱 ${MODEL})" \
  || { echo "❌ 会话未回传文件内容(工具调用循环未走通)"; exit 1; }

#!/bin/zsh
# 跨平台门禁的 zsh 入口；RTK 可选。
set -euo pipefail
cd "${0:A:h:h}"
exec python3 scripts/check.py

#!/bin/zsh
# 源码与本机发布共用门禁；RTK 可选，不影响干净检出的可构建性。
set -euo pipefail
cd "${0:A:h:h}"

run() {
  if (( $+commands[rtk] )); then
    rtk proxy "$@"
  else
    command "$@"
  fi
}

run npm --prefix crates/smoke/web ci
run npm --prefix crates/smoke/web run build
run cargo fmt --all -- --check
run cargo clippy --locked --workspace --all-targets -- -D warnings
run cargo test --locked --workspace --all-targets

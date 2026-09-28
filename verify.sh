#!/bin/sh
# 单入口门禁：格式 → clippy → 测试 → glue 语法 → 行数预算。全绿才允许构建部署。
set -e
cd "$(dirname "$0")"
echo "[verify] fmt"
cargo fmt --all -- --check
echo "[verify] clippy"
cargo clippy --workspace --all-targets -- -D warnings
echo "[verify] test"
cargo test --workspace
echo "[verify] glue syntax"
if command -v node >/dev/null 2>&1; then
  node --check crates/molan-server/src/glue.js
  node --check crates/molan-server/src/pipeline_ui.js
  node --check crates/molan-server/src/agent_ui.js
  if [ -f ../production-flow/ui-check.cjs ]; then
    echo "[verify] agent UI contract"
    node ../production-flow/ui-check.cjs
  fi
else
  echo "[verify] 无 node，跳过 glue/UI 语法检查"
fi
echo "[verify] line budget"
sh tools/check_line_budget.sh .
echo "[verify] ALL GREEN"

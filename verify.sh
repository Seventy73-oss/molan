#!/bin/sh
# 单入口门禁：Rust（格式 → clippy → 测试）→ 注入脚本语法/安全 → 行数预算 → 前端（安装 → 类型 → 测试 → 构建）。
# 全绿才允许构建部署。浏览器端到端验证需本机 Edge/Chrome，设置 MOLAN_E2E=1 时执行。
set -e
cd "$(dirname "$0")"
echo "[verify] fmt"
cargo fmt --all -- --check
echo "[verify] clippy"
cargo clippy --workspace --all-targets -- -D warnings
echo "[verify] test"
cargo test --workspace
echo "[verify] legacy glue syntax（旧界面回退路径仍在使用这些注入脚本）"
if command -v node >/dev/null 2>&1; then
  node --check crates/molan-server/src/glue.js
  node --check crates/molan-server/src/pipeline_ui.js
  node --check crates/molan-server/src/agent_ui.js
  if [ -f ../production-flow/ui-check.cjs ]; then
    echo "[verify] agent UI contract"
    node ../production-flow/ui-check.cjs
  fi
else
  echo "[verify] 无 node，跳过 glue/UI 语法检查与前端门禁"
fi
echo "[verify] line budget"
sh tools/check_line_budget.sh .
echo "[verify] inline-script safety"
# 内联进 HTML 的脚本绝不允许含 "</script"（会提前闭合 script 标签，整页源码外露）
if grep -l '</script' crates/molan-server/src/glue.js crates/molan-server/src/pipeline_ui.js crates/molan-server/src/agent_ui.js 2>/dev/null; then
  echo "[verify] FAIL: 注入脚本含 </script 序列"
  exit 1
fi
if command -v node >/dev/null 2>&1; then
  echo "[verify] frontend install（package-lock.json）"
  (cd frontend && { [ -d node_modules ] || npm ci; })
  echo "[verify] frontend typecheck"
  (cd frontend && npm run --silent typecheck)
  echo "[verify] frontend test（含与 Rust fixture 的契约测试）"
  (cd frontend && npm test --silent)
  echo "[verify] frontend build"
  (cd frontend && npm run --silent build)
  grep -q '<meta name="molan-app" content="paper-studio">' frontend/dist/index.html || { echo "[verify] FAIL: 新入口缺少 paper-studio 标记（会被注入旧脚本）"; exit 1; }
  if [ "${MOLAN_E2E:-0}" = "1" ]; then
    echo "[verify] browser e2e（隔离临时数据目录 + mock 渠道）"
    cargo build -p molan-server
    node frontend/e2e/local.mjs
  fi
fi
echo "[verify] ALL GREEN"

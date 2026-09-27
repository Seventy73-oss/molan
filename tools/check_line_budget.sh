#!/bin/sh
# 行数预算门禁（学 DeepWrite check-source-line-budget）：
#  - 登记的大文件「只减不增」：当前行数 > 登记值即失败；
#  - 未登记文件（新模块）不得超过 CAP_NEW；
# 用法：sh tools/check_line_budget.sh [root]
if [ -n "$1" ]; then ROOT="$1"; else ROOT=$(cd "$(dirname "$0")/.." && pwd); fi
LIST="$(cd "$(dirname "$0")" && pwd)/line_budget.txt"
CAP_NEW=800
fail=0
TAB=$(printf '\t')
if [ -f "$LIST" ]; then
  while IFS="$TAB" read -r path max; do
    [ -z "$path" ] && continue
    case "$path" in \#*) continue;; esac
    f="$ROOT/$path"
    if [ ! -f "$f" ]; then echo "[line-budget] 登记文件缺失: $path"; fail=1; continue; fi
    cur=$(wc -l < "$f" | tr -d ' ')
    if [ "$cur" -gt "$max" ]; then
      echo "[line-budget] 超预算(只减不增): $path 当前=$cur 登记=$max"
      fail=1
    fi
  done < "$LIST"
  cut -f1 "$LIST" > /tmp/lb_reg.$$
else
  : > /tmp/lb_reg.$$
fi
for f in $(find "$ROOT/crates" \( -name '*.rs' -o -name '*.js' \) 2>/dev/null | sort); do
  rel=$(printf '%s' "$f" | sed "s|^$ROOT/||")
  if grep -F -x -q "$rel" /tmp/lb_reg.$$ 2>/dev/null; then continue; fi
  cur=$(wc -l < "$f" | tr -d ' ')
  if [ "$cur" -gt "$CAP_NEW" ]; then
    echo "[line-budget] 新文件超上限: $rel 当前=$cur 上限=$CAP_NEW（请拆模块或登记）"
    fail=1
  fi
done
rm -f /tmp/lb_reg.$$
if [ "$fail" -ne 0 ]; then echo "[line-budget] FAIL"; exit 1; fi
echo "[line-budget] OK"

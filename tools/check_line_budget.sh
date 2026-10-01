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
CR=$(printf '\r')
if [ -f "$LIST" ]; then
  while IFS="$TAB" read -r path max; do
    # Windows 检出（core.autocrlf=true）每行带 CR：先剥掉再比较。
    # 旧实现在 CRLF 下每条登记都报 "integer expression expected" 并被静默跳过，门禁形同虚设。
    path=${path%"$CR"}
    max=${max%"$CR"}
    [ -z "$path" ] && continue
    case "$path" in \#*) continue;; esac
    case "$max" in ''|*[!0-9]*) echo "[line-budget] 登记值非法: $path=[$max]"; fail=1; continue;; esac
    f="$ROOT/$path"
    if [ ! -f "$f" ]; then echo "[line-budget] 登记文件缺失: $path"; fail=1; continue; fi
    cur=$(wc -l < "$f" | tr -d ' ')
    if [ "$cur" -gt "$max" ]; then
      echo "[line-budget] 超预算(只减不增): $path 当前=$cur 登记=$max"
      fail=1
    fi
  done < "$LIST"
  cut -f1 "$LIST" | tr -d '\r' > /tmp/lb_reg.$$
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

#!/bin/sh
cd /src/rust || exit 1
echo "== line budget =="
sh tools/check_line_budget.sh || exit 1
echo "== molan-llm tests =="
cargo test -p molan-llm --lib > /tmp/t1.log 2>&1; c1=$?; tail -n 15 /tmp/t1.log; [ $c1 -eq 0 ] || exit $c1
echo "== molan-server focused tests =="
cargo test -p molan-server --lib suppress_body_never_writes_body_group > /tmp/t2.log 2>&1; c2=$?; tail -n 12 /tmp/t2.log; [ $c2 -eq 0 ] || exit $c2
cargo test -p molan-server --lib outline > /tmp/t3.log 2>&1; c3=$?; tail -n 15 /tmp/t3.log; [ $c3 -eq 0 ] || exit $c3
echo "== molan-server lib full tests =="
cargo test -p molan-server --lib > /tmp/t4.log 2>&1; c4=$?; tail -n 20 /tmp/t4.log; [ $c4 -eq 0 ] || exit $c4
echo "ALL_TESTS_DONE"

#!/bin/sh
cd /src/rust || exit 1
cargo test -p molan-core > /tmp/tc.log 2>&1; c1=$?
tail -n 12 /tmp/tc.log
[ $c1 -eq 0 ] || exit $c1
cargo test -p molan-server > /tmp/ts.log 2>&1; c2=$?
tail -n 8 /tmp/ts.log
exit $c2

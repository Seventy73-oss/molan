#!/bin/sh
cd /src/rust || exit 1
cargo test -p molan-server > /tmp/tall.log 2>&1; c=$?; tail -n 70 /tmp/tall.log; exit $c

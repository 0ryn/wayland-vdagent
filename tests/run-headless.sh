#!/bin/sh
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VDAGENT_TEST_RUNTIME=$(mktemp -d /tmp/vdagent-test.XXXXXX)
export VDAGENT_TEST_RUNTIME
export XDG_RUNTIME_DIR="$VDAGENT_TEST_RUNTIME"
export WLR_BACKENDS=headless WLR_RENDERER=pixman
labwc > "$VDAGENT_TEST_RUNTIME/compositor.log" 2>&1 &
compositor=$!
trap 'kill "$compositor" 2>/dev/null || true; wait "$compositor" 2>/dev/null || true; rm -rf "$VDAGENT_TEST_RUNTIME"' EXIT INT TERM
attempt=0
while [ ! -S "$VDAGENT_TEST_RUNTIME/wayland-0" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ] || ! kill -0 "$compositor" 2>/dev/null; then
        cat "$VDAGENT_TEST_RUNTIME/compositor.log"
        exit 1
    fi
    sleep .1
done
for backend in wlr ext; do
    echo "Testing $backend data control"
    WAYLAND_VDAGENT_DATA_CONTROL="$backend" python3 "$project/tests/primary_bridge.py"
    grep 'using .*data-control' "$VDAGENT_TEST_RUNTIME/agent-test.log"
done

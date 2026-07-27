#!/usr/bin/env bash
set -euo pipefail

RESULT_DIR="${RESULT_DIR:-target/benchmark-results}"
PERF_CPU="${PERF_CPU:-}"
SKIP_GPUI="${SKIP_GPUI:-0}"

cd "$(dirname "$0")/.."

pin=()
if [ -n "$PERF_CPU" ]; then
    if command -v taskset >/dev/null 2>&1; then
        pin=(taskset --cpu-list "$PERF_CPU")
    else
        echo "taskset is unavailable, running unpinned" >&2
        PERF_CPU=""
    fi
fi

mkdir -p "$RESULT_DIR"

run() {
    local out="$1"
    shift
    echo "==> $out"
    ${pin[@]+"${pin[@]}"} "$@" -- --json >"$RESULT_DIR/$out"
}

read_first() {
    if [ -r "$1" ]; then
        head -1 "$1"
    else
        echo unknown
    fi
}

{
    echo "os $(uname -s)"
    echo "kernel $(uname -r)"
    echo "architecture $(uname -m)"
    echo "rustc $(rustc --version)"
    echo "cpu_affinity ${PERF_CPU:-unpinned}"
    if [ -r /proc/cpuinfo ]; then
        echo "cpu $(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | sort -u | head -1)"
    elif command -v sysctl >/dev/null 2>&1; then
        echo "cpu $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
    else
        echo "cpu unknown"
    fi
    if [ -n "$PERF_CPU" ]; then
        echo "governor $(read_first "/sys/devices/system/cpu/cpu${PERF_CPU}/cpufreq/scaling_governor")"
        echo "max_freq_khz $(read_first "/sys/devices/system/cpu/cpu${PERF_CPU}/cpufreq/cpuinfo_max_freq")"
    fi
    echo "intel_no_turbo $(read_first /sys/devices/system/cpu/intel_pstate/no_turbo)"
} >"$RESULT_DIR/profile.txt"

echo "recorded machine profile:"
sed 's/^/    /' "$RESULT_DIR/profile.txt"

run atlas-cull.json cargo bench --locked -p k10s-atlas --bench cull --features testing
run atlas-fanout.json cargo bench --locked -p k10s-atlas --bench fanout --features testing
run world-fanout-cull.json cargo bench --locked -p k10s-world --bench fanout_cull
run world-publish.json cargo bench --locked -p k10s-world --bench publish

if [ "$SKIP_GPUI" = "1" ]; then
    echo "skipping the three suites that need gpui system libraries"
else
    run map-walk.json cargo bench --locked -p k10s-map --bench walk --features testing
    run map-alloc.json cargo bench --locked -p k10s-map --bench walk --features testing,bench-alloc
    echo "==> map-paint.json"
    RUST_MIN_STACK=67108864 ${pin[@]+"${pin[@]}"} cargo run --release --locked \
        --manifest-path tools/k10s-paint-bench/Cargo.toml -- --json >"$RESULT_DIR/map-paint.json"
fi

echo
echo "done, send the contents of $RESULT_DIR"
ls -1 "$RESULT_DIR"

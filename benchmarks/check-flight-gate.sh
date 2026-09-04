#!/bin/sh
# The one measurement that is the app.
#
# The nine headless suites in .github/workflows/performance.yml (and in
# collect.sh) gate culls, walks, edits, and allocations. They are not the
# running application. The scripted flight (`k10s --bench`) and process start
# to first useful photon (`k10s --startup-bench`) are. Startup has a committed
# baseline (app-manifest.json beside the headless one) and this script is its
# gate; the flight still has neither. It may only run on the labelled perf
# host, and it does not refresh any baseline.
#
# Default: print the contract, check the host, print the commands. Never
# opens a window.
#
# On the labelled host, to actually measure:
#   benchmarks/check-flight-gate.sh --run
#
# Do not pass --run on a desktop session you care about. The flight opens a
# real window and takes the GPU.

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/.." && pwd)

# Must stay in lockstep with .github/workflows/performance.yml
# "Verify benchmark core" and the runner label k10s-perf-i5-12600k.
EXPECTED_ARCH=x86_64
EXPECTED_KERNEL=7.1.8-arch1-3
EXPECTED_RUSTC="rustc 1.97.1 (8bab26f4f 2026-07-14)"
EXPECTED_CPU="12th Gen Intel(R) Core(TM) i5-12600K"
EXPECTED_MACHINE=linux-x86_64-i5-12600k
EXPECTED_GOVERNOR=powersave
EXPECTED_NO_TURBO=0
PERF_CPU=4
STARTUP_CPUS=0-11

run=false
case "${1-}" in
    --run) run=true ;;
    "") ;;
    *)
        echo "$0: unknown argument $1; usage: $0 [--run]" >&2
        exit 2
        ;;
esac

echo "k10s: the flight/startup bench is the gate for the one measurement that is the app"
echo "k10s: headless cargo-bench suites in performance.yml do not stand in for it"
echo "k10s: this script does not refresh baselines"

host_ok=true
check() {
    name=$1
    expected=$2
    got=$3
    if [ "$got" = "$expected" ]; then
        echo "  $name: $got"
    else
        echo "  $name: $got (expected $expected)" >&2
        host_ok=false
    fi
}

cpu=$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | sort -u | head -1)
cpu=${cpu:-unknown}
governor=unknown
no_turbo=unknown
if [ -r "/sys/devices/system/cpu/cpu${PERF_CPU}/cpufreq/scaling_governor" ]; then
    governor=$(cat "/sys/devices/system/cpu/cpu${PERF_CPU}/cpufreq/scaling_governor")
fi
if [ -r /sys/devices/system/cpu/intel_pstate/no_turbo ]; then
    no_turbo=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo)
fi
rustc_ver=unknown
if command -v rustc >/dev/null 2>&1; then
    rustc_ver=$(rustc --version)
fi

echo "host:"
check arch "$EXPECTED_ARCH" "$(uname -m)"
check kernel "$EXPECTED_KERNEL" "$(uname -r)"
check rustc "$EXPECTED_RUSTC" "$rustc_ver"
check cpu "$EXPECTED_CPU" "$cpu"
check governor "$EXPECTED_GOVERNOR" "$governor"
check no_turbo "$EXPECTED_NO_TURBO" "$no_turbo"

bin=$root/target/release/k10s
echo "commands (labelled host $EXPECTED_MACHINE, pinned to CPU $PERF_CPU):"
echo "  cargo build --release --locked --bin k10s"
echo "  taskset --cpu-list $PERF_CPU $bin --bench --json --machine $EXPECTED_MACHINE --churn 0"
echo "  taskset --cpu-list $STARTUP_CPUS $bin --startup-bench --json --machine $EXPECTED_MACHINE   (x10, the chooser)"
echo "  taskset --cpu-list $STARTUP_CPUS $bin --startup-bench --json --machine $EXPECTED_MACHINE --objects 25000 --churn 0   (x10)"
echo "  taskset --cpu-list $STARTUP_CPUS $bin --startup-bench --json --machine $EXPECTED_MACHINE --objects 1000000 --churn 0   (x10)"

if [ "$host_ok" != true ]; then
    echo "$0: this is not the labelled perf host; refusing" >&2
    exit 2
fi

if [ "$run" != true ]; then
    echo "k10s: host matches. pass --run on an idle session to measure; will not refresh baselines"
    exit 0
fi

if [ ! -f "$bin" ]; then
    echo "$0: missing $bin" >&2
    echo "$0: build it with: cargo build --release --locked --bin k10s" >&2
    exit 1
fi
if ! command -v taskset >/dev/null 2>&1; then
    echo "taskset: Absent" >&2
    exit 2
fi

result_dir=${RESULT_DIR:-$root/target/app-benchmark-results}
runs=${STARTUP_RUNS:-10}
mkdir -p "$result_dir"
reports=$result_dir/startup-reports.jsonl
: > "$reports"

echo "k10s: running the flight bench on $EXPECTED_MACHINE (opens a window)"
taskset --cpu-list "$PERF_CPU" "$bin" --bench --json --machine "$EXPECTED_MACHINE" --churn 0

# One process is one sample. The three launch shapes are interleaved so any
# drift over the sampling spreads across all three cases equally. The binary
# prints its human summary to stderr and the report to stdout; only the
# report is kept. The bare launch takes no flag at all: any scene flag,
# --churn included, names a generated scene, and the chooser is the point.
#
# Startup is pinned to the P-cores as a set, not to the one core the headless
# suites use. Those suites time single-threaded nanosecond loops and pin to
# keep them off an E-core; a start is the generator, the world and the
# window on their own threads, and on one core they queue behind each other
# and measure the queue. On this host the P-cores are CPUs 0 to 11.
echo "k10s: sampling process start $runs times per launch shape (opens a window each time)"
i=0
while [ "$i" -lt "$runs" ]; do
    i=$((i + 1))
    taskset --cpu-list "$STARTUP_CPUS" "$bin" --startup-bench --json --machine "$EXPECTED_MACHINE" | grep '^{' >> "$reports"
    taskset --cpu-list "$STARTUP_CPUS" "$bin" --startup-bench --json --machine "$EXPECTED_MACHINE" --objects 25000 --churn 0 | grep '^{' >> "$reports"
    taskset --cpu-list "$STARTUP_CPUS" "$bin" --startup-bench --json --machine "$EXPECTED_MACHINE" --objects 1000000 --churn 0 | grep '^{' >> "$reports"
done

# The aggregator refuses to report when a launch a person expects to be
# immediate presents its useful frame past 100 ms, so a slow start fails
# here rather than becoming a number the comparator then accepts.
cargo run --locked -q -p k10s-bench --bin startup-aggregate -- --min-samples "$runs" < "$reports" > "$result_dir/startup.json"
echo "k10s: startup.json written to $result_dir"
cargo run --locked -q -p k10s-bench --bin compare -- "$root/benchmarks/baselines/$EXPECTED_MACHINE/app-manifest.json" "$result_dir"
echo "k10s: flight/startup ran. do not copy these numbers into baselines/ to make a gate pass"

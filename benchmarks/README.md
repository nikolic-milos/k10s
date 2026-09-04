# Performance baselines

Committed baselines are acceptance tests, not historical samples. Each directory names the exact
machine class used to record it and contains a manifest with the operating system, kernel, CPU,
frequency policy, turbo state, logical-CPU affinity, and Rust compiler. The performance workflow
runs only on a runner with the matching hardware label and pins every measured process to the
declared P-core so Linux cannot migrate a sample onto an efficiency core.

The release profile pins `codegen-units = 1`, and the baselines are only valid under it. The hot
loops are generic and monomorphize in their calling crates, so with multiple codegen units any
change in a consumer crate reshuffles partitioning and swings nanosecond-scale medians 15 to 380 percent
with zero semantic change, measured three separate times before the pin. A recording made under
a different codegen configuration is not comparable and must not be committed.

The benchmark harness batches short operations until one independent sample spans at least 20 µs.
Every timing case records the sample count, batch size, total iterations, and median relative
absolute deviation. One suite is the exception: world publication measures operations in the
hundreds of microseconds to milliseconds, so it times each operation individually under a wall
budget with a 100-sample floor instead of going through the shared batching harness. The
comparator rejects structural changes, noisy medians, timing or allocation regressions, and a
sample count that collapses below the tail-gating floor, since a tail regression must not be able to
disable its own gate. A p99 is compared only when the baseline recording contains at least 100
independent samples; older short recordings report their maximum but do not treat it as a
percentile.

Two suites gate absolutely as well as relatively. A comparator that only checks the ratio to the
last recording accepts whatever the last recording happened to be, which is how a 33 ms schema
completion sat inside a passing gate for a keystroke path. The editor and shell suites therefore
also refuse to report at all, exiting non-zero and aborting the collection, when a case a person
waits on has a median outside one 60 Hz frame: inside a keystroke, and also the diff and payload a
person waits through after asking what an apply would change. Whole-document work that is
deliberately debounced or asynchronous (a from-scratch parse, a full validation, a recursive
directory read) carries no such budget, and says so by carrying none.

To validate results recorded in `target/benchmark-results`:

```sh
cargo run --locked -p k10s-bench --bin compare -- \
  benchmarks/baselines/linux-x86_64-i5-12600k/manifest.json \
  target/benchmark-results
```

Run the nine commands in [the performance workflow](../.github/workflows/performance.yml) to
produce that directory. The full-paint benchmark is an isolated workspace because GPUI's
`test-support` feature substantially expands the dependency graph.

Those nine suites are not the running application. The two measurements that are the app
are the scripted flight (`k10s --bench`) and process start to first useful photon
(`k10s --startup-bench`). Startup has a committed baseline: `check-flight-gate.sh --run`
samples each launch shape ten times on the labelled host, `startup-aggregate` folds the
reports into cases, and `app-manifest.json` gates the first frame, the useful frame and the
window at 1.15x plus 5 ms against them. The aggregator itself refuses to report when the
chooser or the 25,000-object scene presents its useful frame past 100 ms, the point past
which a launch stops reading as immediate; the million-object scene carries no absolute
budget. The flight has neither a baseline nor a suite entry yet. Do not refresh the
headless numbers to stand in for either. Do not run `--run` on a desktop session you care
about; the flight and every startup sample open a real window.

Do not refresh a baseline to make a regression pass. Update one only after explaining the expected
structural or performance change, recording every suite on the labelled host under the manifest's
frequency policy, and reviewing the old/new case-level deltas. Keep the old profile directory when
moving to different hardware or a materially different operating-system/toolchain configuration.

## Current recording

`linux-x86_64-i5-12600k`, recorded 2026-09-04 from a clean build of d1b2371: nine suites,
two collections on an idle machine pinned to CPU 4, gated against each other at 5,029 checks
with one flag on a case this host has always wobbled on. The recording reflects two changes
since the previous one. Bounded Service and PVC edges are drawn from the published parent,
which multiplies the edges in every generated scene by 4.1 to 5.0. LOD bands are exclusive
under a named Z0 region cap, which collapses a dense Z0 region from 2,002 quads to 2. On this
recording a one-object structural publish patch costs 488 us at 50k, because a structural
batch rebuilds those edges; incremental adjacency maintenance is the open item that brings it
down. Every refresh is explained in the commit that made it.

Startup, recorded 2026-09-04 from a clean build of 2ebd33c with `check-flight-gate.sh --run`:
ten samples per launch shape on the six P-cores, idle. The chooser presents its first and
useful frame at 63.7 ms median and 73.0 max, the 25,000-object scene at 64.7 and 70.4, and the
million-object scene its first frame at 69.9 and its useful frame at 229.9 ms with a 231.5 max;
the window is built at 50 to 56 ms in all three. Relative deviation is under 0.02 on every
case. `app-manifest.json` gates these at 1.15x plus 5 ms, and the aggregator holds the first
two shapes at 100 ms absolutely.


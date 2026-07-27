# Performance baselines

Committed baselines are acceptance tests, not historical samples. Each directory names the exact
machine class used to record it and contains a manifest with the operating system, kernel, CPU,
frequency policy, turbo state, logical-CPU affinity, and Rust compiler. The performance workflow
runs only on a runner with the matching hardware label and pins every measured process to the
declared P-core so Linux cannot migrate a sample onto an efficiency core.

The benchmark harness batches short operations until one independent sample spans at least 20 µs.
Every timing case records the sample count, batch size, total iterations, and median relative
absolute deviation. The comparator rejects structural changes, noisy medians, and timing or
allocation regressions. A p99 is compared only when both recordings contain at least 100
independent samples; shorter runs report their maximum but do not treat it as a percentile.

To validate results recorded in `target/benchmark-results`:

```sh
cargo run --locked -p k10s-bench --bin compare -- \
  benchmarks/baselines/linux-x86_64-i5-12600k/manifest.json \
  target/benchmark-results
```

Run the seven commands in [the performance workflow](../.github/workflows/performance.yml) to
produce that directory. The full-paint benchmark is an isolated workspace because GPUI's
`test-support` feature substantially expands the dependency graph.

Do not refresh a baseline to make a regression pass. Update one only after explaining the expected
structural or performance change, recording every suite on the labelled host under the manifest's
frequency policy, and reviewing the old/new case-level deltas. Keep the old profile directory when
moving to different hardware or a materially different operating-system/toolchain configuration.

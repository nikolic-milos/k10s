# Performance baselines

Committed baselines are acceptance tests, not historical samples. Each directory names the exact
machine class used to record it and contains a manifest with the operating system, kernel, CPU,
frequency policy, turbo state, logical-CPU affinity, and Rust compiler. The performance workflow
runs only on a runner with the matching hardware label and pins every measured process to the
declared P-core so Linux cannot migrate a sample onto an efficiency core.

The release profile pins `codegen-units = 1`, and the baselines are only valid under it. The hot
loops are generic and monomorphize in their calling crates, so with multiple codegen units any
change in a consumer crate reshuffles partitioning and swings nanosecond-scale medians 15–380%
with zero semantic change — measured three separate times before the pin. A recording made under
a different codegen configuration is not comparable and must not be committed.

The benchmark harness batches short operations until one independent sample spans at least 20 µs.
Every timing case records the sample count, batch size, total iterations, and median relative
absolute deviation. One suite is the exception: world publication measures operations in the
hundreds of microseconds to milliseconds, so it times each operation individually under a wall
budget with a 100-sample floor instead of going through the shared batching harness. The
comparator rejects structural changes, noisy medians, timing or allocation regressions, and a
sample count that collapses below the tail-gating floor — a tail regression must not be able to
disable its own gate. A p99 is compared only when the baseline recording contains at least 100
independent samples; older short recordings report their maximum but do not treat it as a
percentile.

Two suites gate absolutely as well as relatively. A comparator that only checks the ratio to the
last recording accepts whatever the last recording happened to be, which is how a 33 ms schema
completion sat inside a passing gate for a keystroke path. The editor and shell suites therefore
also refuse to report at all — they exit non-zero and abort the collection — when a case a person
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

Do not refresh a baseline to make a regression pass. Update one only after explaining the expected
structural or performance change, recording every suite on the labelled host under the manifest's
frequency policy, and reviewing the old/new case-level deltas. Keep the old profile directory when
moving to different hardware or a materially different operating-system/toolchain configuration.

## 2026-08-03: the write path, and one 12% shift that is code layout

The recording of this date adds nine editor cases and two shell cases for the three-way diff, the
two-way dry-run diff, and the apply payload prune. It also moves `editor edit` `cursor-context` from
342 µs to 385 µs at 256 KiB and 1.348 ms to 1.537 ms at 1 m — a 12–14% shift with **no change on
that code path at all**, and it is recorded rather than fixed. What was measured, on the labelled
host, before believing that:

- the pre-change tree reproduces its own baseline exactly: 341.6 µs and 342.3 µs over two runs,
  against a recorded 342.5 µs, and 1.340 / 1.353 ms against a recorded 1.348 ms;
- adding the new fixtures to the *bench binary* alone, library untouched, keeps it there: 346.8 µs;
- adding `Syntax::pair_at` to the library moves it to 380.5 µs — with `resolve_path` restored
  byte-for-byte to its original form, so the shared-descent refactor is not the cause;
- `#[inline]` on the shared descent does not recover it either;
- `context_at`, which is what this case measures, calls none of the changed code.

So the mechanism is instruction layout inside this crate's single codegen unit, which is the class
of effect `codegen-units = 1` was pinned to make *deterministic* rather than absent — it is stable
across runs and it is not semantic. The op stays an order of magnitude inside its absolute frame
budget (385 µs against 16.67 ms), and that budget, not the ratio, is what protects the keystroke
path here. A future 12% on this case will therefore not be caught by its ratio gate; it will be
caught by the absolute one, which is the honest division of labour between them.

Two cases flagged during this cycle were **not** recorded and are not regressions: `atlas cull`
p99 on one camera and `world fan-out cull` p50 on one fan-out shape each tripped in exactly one
collection out of four and returned to baseline on targeted re-runs (167.7 / 159.2 ns against a
recorded 155.8 ns; 841.2 / 848.7 ns against a recorded 846.9 ns). Single-run outliers on this host
are a known phenomenon at nanosecond scale; a targeted re-run is what tells them apart from a
regression, and the recording that was installed is one with none of them in it.

A recording that cannot gate does not belong in `baselines/`. In-app flight runs and contributed runs
from machines the project does not control live in [observations](observations/), which states what
each one establishes and what it does not. The flight harness pins a letterboxed logical viewport of
1600×1000 and stamps `machine` + `churn` into schema v4 reports; see the observations README for how
to contribute a comparable run.

## 2026-08-04: the apply-payload baseline never measured the guard it was gating

Three `editor edit` `apply-payload` cases are re-recorded here, and the reason is not a change in
what the operation does. The recorded numbers — 25.7 µs, 46.9 µs and 130.6 µs — were written at
18:07 on 2026-08-03. `k10s-edit/src/apply.rs` and `k10s-edit/src/syntax.rs` were last written at
19:01 that evening, and what landed in that window was the fail-closed guard that refuses to prune
a document whose keys tree-sitter and the API server could read differently: encoded, tagged,
merged or aliased. The baseline therefore predates the guard by 54 minutes and never measured it.
Built from the recording commit's own tree, the same case measures **27.33 ms**, not 130.6 µs — and
fails its absolute frame budget, which is how this was found. It is the second dirty recording this
project has caught, and the first one the absolute gate caught rather than the ratio.

The guard walks every node of the document, so each per-node cost is paid about 150,000 times on
the 31,000-line fixture. Three of them were avoidable and none was semantic:

- `node.walk()` inside the loop allocated a tree-sitter cursor **per node**; one cursor now walks
  the whole document.
- `node.kind()` compared strings where the grammar has already assigned numbers; the four kinds
  this guard cares about are resolved to ids once per call and compared as `u16`.
- every mapping key was copied out of the rope with `slice_to_string` only to be rejected; a key
  is now copied only if its bytes contain one of `< ! & * " ' \n \r`, which is the necessary
  condition for any shape the guard rejects.

Measured on the labelled host, pinned to CPU 4, against the recording commit's own tree:

| case | recorded 18:07 | recording commit, rebuilt | now | vs. rebuilt |
| --- | --- | --- | --- | --- |
| 16k apply-payload | 25.7 µs | 387.2 µs | 191.5 µs | 2.02x |
| 256k apply-payload | 46.9 µs | 6.377 ms | 2.525 ms | 2.53x |
| 1m apply-payload | 130.6 µs | 27.331 ms | 11.288 ms | 2.42x |

The 1 m case is what matters: 27.33 ms was one and a half frames on a path a person waits through
after ctrl-s, and 11.29 ms is inside one. It is inside by 1.5x rather than the order of magnitude
the keystroke cases enjoy, so it is worth stating what is left. The remaining cost is the tree walk
itself and nothing else — disabling the key scan entirely moves it by under 3%, so there is no
further constant to remove. Going materially faster means visiting fewer nodes, which means scoping
the guard to the subtrees the prune actually splices (the document's root mapping and `metadata`)
rather than to the whole document. That is a narrowing of a fail-closed security guard, so it wants
its own change and its own review, not a footnote in a performance pass.

Only these three cases were re-recorded. The other 5,040 checks in this collection passed against
the existing baselines unchanged, including every map, atlas, world and shell case — which is also
the evidence that extracting the theme into its own crate cost the map's paint path nothing.

## 2026-08-04: the diff got faster, and the baseline has deliberately not been refreshed

The G2 follow-ons changed how `k10s-edit::diff` builds hunks. It used to emit every row and then
group the finished rows into hunks in a second pass; it now opens or extends a hunk inside the
alignment loop, which is also the only place that knows the buffer coordinate a hunk needs to carry
so that taking the cluster's side of one is an edit rather than a retype. Removing the second
O(rows) walk is worth more than the per-hunk span costs:

| case | baseline | now | ratio | rows |
| --- | --- | --- | --- | --- |
| 16k diff-three-way | 100.4 µs | 92.4 µs | 0.92x | 521 = 521 |
| 256k diff-three-way | 1.648 ms | 1.505 ms | 0.91x | 7,886 = 7,886 |
| 1m diff-three-way | 6.162 ms | 5.587 ms | 0.91x | 31,291 = 31,291 |
| 16k diff-two-way | 57.6 µs | 44.1 µs | 0.77x | 519 = 519 |
| 256k diff-two-way | 922.5 µs | 700.2 µs | 0.76x | 7,884 = 7,884 |
| 1m diff-two-way | 3.381 ms | 2.623 ms | 0.78x | 31,289 = 31,289 |

The row counts are identical to the byte, which is the useful half of the structure gate here: the
alignment and the classification produced exactly the same document, so this is the same work done
once instead of twice rather than less work done. Two-way gains most because it skips the second
alignment entirely and is therefore dominated by row emission. The whole collection passed —
5,040 checks, nine suites, no failures — so nothing else moved.

One collection in the middle of this needed the documented discriminator. Taken immediately after a
full clippy-plus-test-plus-bench-build pass, it reported **fourteen** `atlas cull` p99 regressions in
a crate this change set does not touch and cannot reach; a targeted idle re-run of that one suite
reported **one**, on a case that was not among the fourteen, and a second reported **none**. The p50s
never moved at all (median ratio 1.000 against the collection before it), so no slope changed. That
is the same signature this file and the project's notes have recorded twice before: `atlas cull`'s
p99 tail is thermally sensitive, one to nine cases wobble per collection, they are different cases
every time, and they return to baseline on an idle re-run. Pre-building the bench binaries is not
enough — the machine has to be *idle*, not merely done compiling.

**The baseline is not refreshed by this note, on purpose.** The measurement was taken from an
uncommitted working tree, and this file's own procedure says a recording may only come from a clean
build of a committed tree; the two dirty-baseline incidents above are what that rule is made of.
The consequence is stated in ROADMAP §8: until these six cases are re-recorded from a commit, the
gate is conservative by 7–23% on them, so a future regression back to the old numbers would pass.
Re-record them, from a clean build, before trusting the `editor edit` suite to catch a diff
regression again.

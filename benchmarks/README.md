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

## 2026-08-05: those six cases re-recorded from a commit, plus four the diff moved without touching

The note above ends by saying re-record these before trusting the `editor edit` suite again. That is
what this is. The change set it was measuring is now three commits — the G2 follow-ons, the Helm read
path, and the seam work — and the tree was clean at `08ca95f` when both collections were taken, which
is the provenance the previous recording lacked and the whole reason it was withheld.

Ten cases move. Six are the diff, and the numbers land where the working-tree measurement said they
would:

| case | baseline | recorded | ratio | rows |
| --- | --- | --- | --- | --- |
| 16k diff-three-way | 100.4 µs | 93.3 µs | 0.93x | 521 = 521 |
| 256k diff-three-way | 1.648 ms | 1.493 ms | 0.91x | 7,886 = 7,886 |
| 1m diff-three-way | 6.162 ms | 5.572 ms | 0.90x | 31,291 = 31,291 |
| 16k diff-two-way | 57.6 µs | 44.5 µs | 0.77x | 519 = 519 |
| 256k diff-two-way | 922.5 µs | 699.9 µs | 0.76x | 7,884 = 7,884 |
| 1m diff-two-way | 3.381 ms | 2.617 ms | 0.77x | 31,289 = 31,289 |

The other four are the whole `cursor-context` family — 256k 387.1 → 362.9 µs, 1m 1.545 → 1.433 ms,
json-16k 198.0 → 191.3 µs, json-256k 3.228 → 3.125 ms — and **nothing on that path changed.**
`context_at` calls none of the diff code. This is the third recorded instance of the effect the
2026-08-03 section is about: `k10s-edit` is one codegen unit under the pinned `codegen-units = 1`, so
rewriting `diff.rs` re-laid out instructions in a crate-mate that never calls it. Last time the same
mechanism moved this same case 12% the *other* way and was recorded rather than chased. It is
recorded here for the symmetric reason: an improvement that is not recorded means a regression back
to the old number passes, and 8% of headroom nobody earned is 8% of regression nobody would see.

What was deliberately **not** re-recorded, and why, because a refresh is only as honest as what it
leaves alone. `world fan-out cull` `platform/12000/63/200` at the Z0 fit camera reports 121.8 ns
against a recorded 134.7 ns — in both collections, to the tenth of a nanosecond, so it is not wobble.
It is also not explicable: nothing in `k10s-world` or `k10s-atlas` changed, and neither depends on
`k10s-edit`. The likeliest reading is that the *baseline* for that one case was recorded high, which
is the failure mode this file has already documented twice. A gate that is conservative by 9.5% on
one case accepts a regression it should catch; a baseline refreshed on a number nobody can explain
accepts something worse. It stays, named here, until a mechanism or a third independent collection
settles it. `atlas fan-out` `E/padded/5000` and `/10000` moved 13% in the second collection and
0.1% in the first, at 34 ns, which is the nanosecond-scale wobble the ratio-plus-absolute-slack gate
exists to absorb; they are unchanged.

Method, since the two dirty-baseline incidents both came from skipping a step. Every bench binary was
pre-built first, then two full collections were taken on an otherwise idle machine — no subagents, no
compilation during either — with sixty seconds of settle between them. The two agree within 0.5% to
2.2% per case with p50 rMAD between 0.003 and 0.012, and every re-recorded case carries at least 100
independent samples, so the p99s are gated rather than merely reported. The second collection is what
was installed, because it is the one with no compilation anywhere inside it. The first was then run
against it as an independent check and passed all 5,040 checks — which is precisely the property the
2026-08-02 baseline turned out not to have, and the only evidence that a recording is reproducible
rather than a snapshot of one afternoon.

## 2026-08-05: two new cases, and a keystroke that was a third of a frame

`shell state` gains `map-50k index` and `map-50k keystroke`: building the
searchable list of everything on the map, and ranking it for one keystroke, at
51,600 objects. Only the keystroke carries the 60 Hz budget, and the split
between the two is what the budget is for. Building walks every object and
allocates a string per row; ranking runs while somebody is still typing. They are
separate cases because they are separate problems, and a single number covering
both would hide whichever one moved.

The keystroke case was **5.50 ms** when first measured — inside the budget the
gate enforces, and a third of a frame spent scanning on behalf of an answer that
is thirty-two rows long. That is the shape of number this file exists to make
visible: nothing was failing, and a third of a frame per keystroke is still a
third of a frame. The mechanism was in `fuzzy_match`, which builds a character
vector and a range vector for every candidate it looks at — a hundred thousand
allocations per keystroke, for highlight ranges belonging to rows nobody will
see. Scoring in one allocation-free pass and matching only the survivors took it
to **1.57 ms**, under a tenth of a frame, with an equivalence test holding the
two scorers together so they cannot drift into sorting a list one way and
highlighting it another.

`map-50k index` is recorded at 195 samples rather than the 97 its first
configuration produced. The comparator compares a p99 only when the baseline
carries at least a hundred independent samples, so a case recorded just under
that floor has a tail number nothing checks — which is the quietest way for a
gate to be decorative.

Recorded from a clean build of the committed tree, twice, on an idle machine:
the two runs agree to 0.7% and 0.1% on the new cases, and the run that was *not*
installed then passed against the one that was — 5,050 checks, no failures.

## 2026-08-05 — splitting a module can cost 4x with no change on the measured path

`k10s-world/src/lib.rs` was 2,819 lines, 1,690 of which were one `mod tests`.
Both halves were split up: the tests into `publish_test`, `stability_test`,
`spawn_test` and a shared `test_support`, and the implementation into
`publish`, `rollup`, `spawn` and `harness`. The code moved verbatim — a
per-item diff of the result against the original is empty modulo `pub(crate)`
and rustfmt reflow, and all 84 `world fan-out cull` cases agree on every
structural field, so the snapshots the benchmark culls are identical in content.

The implementation half was reverted anyway, because it cost this:

| case | metric | before | after |
|---|---|---|---|
| ns-fanout 50k, Z1 widest ns | `p50_flat_spatial_ns` | 8,493 ns | 33,978 ns (**4.00x**) |
| ns-fanout 50k, Z2 wide ns | `p50_flat_spatial_ns` | 6,838 ns | 25,859 ns (**3.78x**) |
| platform 4k, Z3 deepest wl | `p50_no_edges_ns` | 165.6 ns | 200.6 ns (1.21x) |
| ns-fanout 12k, Z3 deepest wl | `p50_ns` | 198.4 ns | 227.0 ns (1.14x) |
| wl-fanout 4k, Z3 deepest wl | `p50_ns` | 992.9 ns | 1,127.5 ns (1.14x) |

None of the moved code runs inside the measured region — the timed closure is
`k10s-atlas`'s cull over an already-built snapshot. What changed is how the
bench binary links: the cull is generic and monomorphizes in its calling crate,
and five more modules in `k10s-world` moved ThinLTO's import decisions for that
one binary. This is the effect `codegen-units = 1` was pinned to make
*deterministic* rather than absent, at the upper end of the 15–380% the
workspace manifest warns about.

It does not reach the shipped app. The flight bench — the real binary, the real
cull, per frame — is unchanged at worst 1.04x with identical draw counts, and
`map walk` and `atlas cull` sit at a median ratio of 1.000 with no case over the
gate. The regression exists only in `k10s-world`'s own bench binary.

Two things were verified rather than argued. The pre-split tree was rebuilt from
a snapshot and measured on the same idle machine minutes later: 8,493 ns,
confirming the difference was the change and not the hour. And the tests-only
split was then measured on its own: 8,447 ns, 6,828 ns, 198.5 ns, 165.6 ns,
993.6 ns — every case within 0.5% of pre-split. That was predictable in advance,
since `#[cfg(test)]` modules compile into no binary a benchmark links, and it is
why the test split shipped and the implementation split did not.

The lesson worth keeping: for a crate whose consumers monomorphize its
neighbours' hot generics, a module boundary is not free, and "the code is
identical" is not evidence that the machine code is. Measure the split, not the
diff.

Recorded on the same idle machine as the surrounding sections, with the
`editor edit` suite re-run to confirm the machine itself was not the variable —
it had drifted 17–31% high during one collection and returned to baseline
(5,573,321 ns at 1 m against a recorded 5,571,538) on an unchanged binary.

## 2026-08-06 — the Starmap redesign: one intended structural change, and one cost that is the feature

The Starmap's visual language was rebuilt. Namespaces are islands with unequal, identity-stable
corners instead of 6 px rectangles; every drawn workload wears its vendor or kind glyph, sized to the
space it owns instead of pinned at 12 px in a corner; labels come from a type ladder derived from the
user's typography, set in the display face for namespace names, centred and clipped inside the thing
they name; pods are rounded and drawn hollow while terminating; and the pointer and selection rings
are new. Separately, the live layout engine learned to widen a pod grid and to repack one after a
scale-down.

### What did not move

`world publication` is unchanged end to end: every case between 0.97x and 1.01x, and
`structural_patches` / `full_materializes` identical, so the pod-grid rework — `place_pod` widening
the grid, `repack_pod_grid` compacting it, satellites re-orbiting — costs the measured path nothing
and never tips a batch into a full materialize.

`map allocation` passes every check: the walk still allocates nothing, and no label string is
synthesized. `full GPUI paint allocation` holds `allocations_per_paint` exactly, with
`bytes_per_paint` up 337 bytes for the second font and the label content masks.

Every `exact` structural field in every suite matched except `icons`, and the 360,000-case
cull-oracle sweep passed unchanged. That was the constraint the redesign was shaped around:
appearance is carried by the *parameters* of quads the walk already emitted, and the two rings are
painted outside `frame::walk` from a `PickPath` and the camera, so neither enters `CullStats` nor has
to be mirrored across the five cull functions.

### The one intended structural change

`WL_ICON_MIN_PX` is now `4.0`, the same threshold as `WL_MIN_PX`: if a workload is drawn at all, it
wears the mark that says what it is. `icons` therefore rises wherever workloads were previously drawn
as bare rectangles — 0 → 476 on `map walk` at the Z1 region camera, and 44 cases across the suites.
That is the change the redesign exists to make, not a side effect of it.

It cost 1.61x on `walk_count` and 1.30x on `walk_paint` at that camera: 476 glyphs where there were
none, about 6 ns each in the walk. `paint_svg` keys its sprite atlas on rasterized pixel size, so
every camera-derived size on the map goes through `k10s_theme::quantize` — a fifteen-step geometric
ladder — or a zoom would mint a fresh tile and a fresh GPU upload every frame.

### The Z0 fit camera, and the four rounds it took

`map walk` at the fit camera is the one walk §6.1 budgets at O(regions). Rounds on
`uniform r400 b15 / Z0 fit`, `walk_count_p50_ns` against the committed 5,109 ns:

| tree | ns | ratio | what changed |
| --- | --- | --- | --- |
| first cut | 12,698 | 2.49x | gradient fill and hashed corners on every island |
| shading gated on size | 10,451 | 2.05x | a gradient nobody can see at forty pixels is not drawn |
| corner hash gated on size | 9,830 | 1.92x | so the hash was not the cost either |
| per-branch colour types | 6,505 | 1.27x | **this was the cost** |
| screen sizes from world x zoom | 6,392 | 1.25x | no round trip through the `Bounds` just written |
| thresholds pre-divided into world units | 6,347 | 1.24x | one comparison, no conversion, per island |

The fourth row is the finding worth keeping. Folding `paint_region`'s three stage arms into one
`(fill, border)` pair unifies the border's type across them, and the settled arm's border is an
`Hsla` while the other two are `Rgba` — so the tidier code inserted an `Hsla -> Rgba -> Hsla` round
trip per region on a conversion that is branchy. It cost more than the gradient and the hash
together. Three explicit arms, each with its own natural types, is the shape that is fast.

The same class of cost appeared once more and was designed out rather than accepted. The glyph gate
briefly asked about the workload's halo (`block.rect.w`) instead of its card; every caller has
already loaded `inner.w` for `block_painted`, so that added a second load to a loop that runs once
per block, and it cost 1.13–1.17x across the `Z3 deepest wl` and `Z2 widest ns` cull cases **with the
icon count held identical**. Lowering the card threshold instead reaches the same workloads for
nothing, and those cases returned to 0.97–1.00x over three same-binary runs.

What is left at Z0 is one comparison, one multiply and a four-way corner store per region — the
island silhouette itself, about 3 ns per namespace. At four hundred namespaces the whole counting
walk is 6.3 µs against a 16,666 µs frame. There is no way to draw a rounded island for less.

Two thresholds keep it bounded rather than free: below 96 screen pixels an island's four corners
collapse to one radius and its fill is flat, because at that size the asymmetry is two pixels and the
gradient is invisible; above it, the number of islands that large is bounded by the viewport rather
than by the cluster, which is §6.1 applied to a detail instead of to a node.

### Final ratios, and what is not a regression

| case | walk_count | walk_paint |
| --- | --- | --- |
| uniform r200/r400 / Z0 fit | 1.19x, 1.24x | 1.14x, 1.18x |
| uniform r200/r400 / Z1 region | 1.64x | 1.34x |
| every other map-walk case | 1.02–1.09x | 1.04–1.07x |

`Scene` gained one field in the course of this, `card_header`: the band the layout reserves above a
pod grid, in world units. It is the one piece of the layout's shape a painter cannot infer — the two
layout modes reserve 26 and 16 units, no card's geometry reveals which, and guessing it from the
card's height draws the header over the first row of pods in whichever mode was not guessed for.
`assert_published_matches_full` compares it, so a publish path that forgets it fails loudly. It cost
nothing measurable: every case above moved by at most 0.03x, which is this suite's run-to-run
spread.

Three fan-out cases remain flagged and are the host's documented instruction-layout sensitivity, not
a mechanism. On one scene, `ns-fanout 50000`, `p50_flat_spatial_ns` reads **0.88x at `Z1 widest ns`
and 2.15x at `Z2 wide ns`** — two neighbouring cameras, one code path, opposite signs, reproducible
across three same-binary runs. This bench binary links `k10s-world`, whose implementation changed,
and the 2026-08-05 note above records the same case moving 4.0x for a module split with no semantic
change at all. `atlas cull` `Z4 extreme` p99 and `atlas fan-out` degree-64 p50 are the usual
scattered singletons.

**The baseline has deliberately NOT been refreshed.** These numbers come from an uncommitted working
tree, which is the provenance both dirty-baseline incidents had. The gate rejects the Z0 and Z1 map
cases and the `icons` counts; that rejection is correct, and it should be resolved by re-recording
every suite from the commit that carries this change — not by editing the manifest.

## 2026-08-11 — interaction chrome stays out of the canvas hot path

The Starmap now has a semantic scale rail, summary, health legend, hover inspector and keyboardable
camera controls. The shell hosts that furniture as a sibling GPUI entity rather than making it a
child of `MapView`. Camera damage therefore rebuilds only the canvas; the fixed chrome is notified
only when its bounded state changes: a scale-band crossing, resize, toggle, hover target or summary.

Point picking no longer scans every workload or pod in a visible parent. It asks the existing
hierarchical indexes for point candidates and then applies the same geometry tests as before.
Deterministic 50,000-object tests cap both workload and pod probes at 64 candidates and verify that
the expected target still wins.

The allocation ratchets were rerun after this separation. Every map-walk case still reports zero
label allocations. Forced full `MapView` paints report 71 cached / 74 uncached allocations against
the committed 67.621 / 69.762 baseline and its 5% + 1 allocation allowance; reallocations fall to
79 from 94.297 / 93.609, and bytes fall to 1,627,124 / 1,627,268 from 1,819,734 / 1,819,554. Text
cache hit, miss and eviction counts remain exact. No baseline was refreshed.

## 2026-08-11 — process start to first useful photon is a named contract

Cold start now has its own one-process benchmark:

```text
cargo build --release --locked --bin k10s
target/release/k10s --startup-bench --json --machine linux-x86_64-i5-12600k
target/release/k10s --startup-bench --json --machine linux-x86_64-i5-12600k --objects 25000 --churn 0
target/release/k10s --startup-bench --json --machine linux-x86_64-i5-12600k --objects 1000000 --churn 0
```

The versioned report starts its clock on the first line of Rust `main`, matching
Zed's process-start convention and deliberately excluding the dynamic loader.
GPUI exposes no renderer-submit or compositor feedback callback. A reported
`*_presented` milestone is therefore the first platform frame callback after
GPUI submitted the observed frame, not a claim about physical scan-out. The
instrumentation lives at the application and `MapView` boundary; no timer,
serialization, lock or benchmark branch enters `frame::walk`.

`first_presented` is the first submitted frame. `useful_presented` is that same
frame for the launch chooser, but a named source must first publish the scene it
requested. Readiness is the later of content preparation and publication of a
matching immutable snapshot, so generator/world callback order cannot change
the number. The report separates argument parsing, source and content work,
world spawn, platform launch, font/configuration setup, native window creation,
first presentation, matching-scene publication and useful presentation.
An incomplete run exits non-zero instead of printing a partial success.

Named scenes no longer block native window creation. The generator and an
explicit command-line cluster connection both run through the launch service
while an empty world and the window start. Connection failure retains the CLI's
non-zero exit contract; success adopts the provider in place. The world control
wait is interruptible, so a ready scene does not sit behind the 50 ms simulation
tick. Scene revisions remain monotonic across replacements, making the initial
empty shell distinguishable from a later, legitimately empty cluster without a
timing or object-count heuristic. Synthetic data crosses that control seam as a
typed, owned `PreparedScene`: real clusters retain their native event contract,
while a generator no longer allocates roughly one `ResourceEvent` per object
merely for the world to fold those events back into the hierarchy it already
owned.

Initial world construction has a separate no-renderer phase benchmark:

```text
cargo bench --locked -p k10s-world --bench build
cargo bench --locked -p k10s-clustergen --bench prepare
```

It led to three measured changes. UID maps in this crate use the same
`rustc_hash::FxHashMap` policy as Zed's collections layer; every known topology
cardinality is reserved once; and the production builder consumes its prepared
input, moving labels, details and dependency vectors instead of cloning them
just before the source is dropped. `rustc-hash` was already in the locked graph,
so this changes no package count. An exact equality test holds the prepared
generator output against the event-folded representation.

The isolated million-object event build moved from 325.76 ms to 226.38 ms after
the hasher change, then to 175.48 ms after capacity-correct construction. In the
same pre-consuming binary, bypassing the event fold measured 138.73 ms. The
then-current consuming benchmark reported source disposal inside assembly, so its
160.35 ms prepared total is intentionally not compared as though the timing
boundary were unchanged.

Process-level observations on Wayland at 1600x1000, release profile,
`linux-x86_64-i5-12600k`, are the comparable result:

| state | runs | first presented | content ready | scene after content | useful presented |
| --- | ---: | ---: | ---: | ---: | ---: |
| synchronous 1M starting point | 1 | 319.64 ms | — | — | 659.04 ms |
| async 1M before world tuning, schema 2 | 3 | 77.30 ms | 283.55 ms | 410.83 ms | 701.43 ms |
| second-ratchet release 1M | 5 | 78.62 ms | 201.88 ms | 140.68 ms | 350.87 ms |
| snapshot-sharing checkpoint 1M | 5 | 78.77 ms | 201.38 ms | 123.15 ms | 335.41 ms |
| current release 1M | 5 | 82.30 ms | 200.98 ms | 100.42 ms | 311.29 ms |
| current release 25k | 5 | 75.12 ms | 8.53 ms | 3.38 ms | 75.12 ms |

The current million-object first useful frame is 55.6% earlier than the
instrumented async pre-tuning median and 52.8% earlier than the original
blocking-path sample. First photon is 74.3% earlier than the original blocking
path; the current five-run first-present median is slightly higher than the
earlier release because window/compositor timing varied, not because it waits
for scene construction. All five current 25k runs presented useful content on
their first submitted frame. Once the immutable million-object scene exists,
its useful presentation takes a 9.70 ms median; construction, not viewport
paint, remains the dominant large-scene cost. Each release row was collected
from the exact artifact described by that row.

The second construction ratchet removed work rather than adding threads. UID
formatting now writes decimal indices into a fixed stack buffer before the one
required `Arc<str>` allocation, avoiding a temporary heap `String` for nearly
every object. The 22-value satellite-detail vocabulary is interned once instead
of allocating the same value for 277,268 attachments. Cross-namespace edge
planning is a stable sparse list rather than 124k mostly-empty `Vec` headers,
and workload-name uniqueness counts `(service, role)` pairs instead of retaining
a cloned copy of every generated name. The isolated 1M generation-plus-prepare
median moved from 176.26 ms to 152.99 ms; its prepare phase moved from 77.82 ms
to 61.07 ms. At 25k the corresponding total is 3.59 ms.

World assembly then resolved initial edges through the permanent workload slot
map instead of building and discarding a second 124k-entry UID map, folded
severity counts into the pass already consuming pods, and replaced per-pod
components with one typed `PodHealth` resource vector. The component form had
stored the same state in aggregates and in 591,688 Bevy entities even though no
system queried those entities; every access first went through a parallel
entity-index vector. Keeping the state as a resource preserved the ECS world
and scheduled rollup/extract boundary while removing that duplicate storage and
indirection. At that checkpoint, a dedicated high-water test pinned slot reuse
and the isolated 1M prepared world build reported 45.41 ms layout, 57.40 ms
assembly, 30.46 ms publication and 133.28 ms total, down from the prior 157.84
ms total. Its exact release's 350.87 ms useful median was another 16.6% earlier
than the 420.66 ms release immediately before that ratchet.

The third construction ratchet removes the remaining repeated work while
keeping small scenes sequential. Spread layout now streams orbit rings without
temporary vectors, caches each golden-spiral square root and trigonometric pair
once per layout, and uses a contiguous collision list for namespace packs up to
the measured 128-item crossover. The large world pack retains a pre-sized grid
index, so the fallback remains sub-quadratic. Committed bit-exact fingerprints
pin every layout mode.

Snapshot identity now uses `SlotIds`: each scene shares topology's contiguous
immutable base and a live mutation copies only its touched 1,024-entry page.
Topology stores one canonical UID per object; its map points compact
fingerprints at full-identity collision chains, with a dense liveness byte for
adjacency scans. Forced-collision tests cover lookup and removal at every chain
position. `Aggregates::pod_state` is now the sole pod-state vector, and explicit
old/new deltas update derived rollups correctly even when one pod changes
several times in a tick. Finally, full node publication splits into three coarse
lanes only at 250k nodes and on hosts with at least three available CPUs. A
forced sequential/parallel equality test pins the result, while page sharing,
truncation, slot reuse, snapshot isolation and derived-state oracles cover the
mutation paths independently.

Across four clean runs, the isolated 1M prepared-world median is 27.74 ms
layout, 51.91 ms assembly, 14.30 ms publication and 94.34 ms total. That is
29.2% below the preceding 133.28 ms checkpoint; layout is 38.9% lower and
publication is 53.1% lower. The event-folded path is 132.66 ms and the prepared
25k path is 1.92 ms. At process level, content preparation remained essentially
flat while the 1M scene-after-content interval moved from 140.68 ms to 100.42
ms. Useful presentation moved from 350.87 ms to 311.29 ms, an 11.3% reduction.

Several plausible variants stayed out after measurement: 256- and 8,192-entry
identity pages, an unstable layout sort with an explicit tie-break, paired
`sin_cos`, and explicit bounds in the grid collision index were neutral or
slower. The retained thresholds and representations are therefore empirical,
not speculative complexity.

One plausible optimization was measured and rejected: parallelizing prepared
conversion by namespace raised median useful startup from 409.01 ms to
431.52 ms and increased first-frame variance. These tasks allocate too finely;
allocator contention and cache disruption cost more than the parallel work
saved, so the release path remains sequential.

These are working-tree observations, not a committed baseline. No baseline was
refreshed; a real-cluster three-size recording still belongs on a clean,
committed tree with the machine manifest and repetition discipline described at
the top of this file.

## 2026-08-11 — re-recorded from a clean commit, now that the map draws what it means

The 2026-08-05 recording predates the icon gate, so it held icon counts of
zero at zooms the map now marks: 27 structural `icons` diffs across the cull,
fan-out, world and walk suites (0 → 476 on the uniform scenes, 0 → 1024 on
the fan-outs), plus the timing that follows from drawing what those counts
count. A gate that rejects the shipped behaviour on every honest collection
is a gate rejecting the wrong thing, so every suite was re-recorded — which
also retires the known conservatism on the six `editor edit` diff cases,
faster than their recording since the alignment-loop change and re-measured
here from a commit as the procedure demands.

Provenance: nine suites recorded from a clean build of 65c9fa6, twice, on an
idle machine pinned to CPU 4. The run that was not installed gates against
the one that was at one flag in 5,000+ checks: `map walk` `uniform r400 b15`
`Z1 region` `walk_count_p50_ns`, 7,818 → 9,261 ns (1.18x). A third targeted
sample answered 7,790 ns — 0.4% from the installed number — so the odd run
out was the second collection's one-case wobble, the same class this file has
recorded twice before, and the installed number is the one two of three
samples agree on. `map-paint.json` alone was recorded at 4cb8f1c, because the
excluded tool would not build before that commit handed its lockfile the
`rustc-hash` line k10s-world was owed — nothing in any suite's dependency
graph differs between the two commits.

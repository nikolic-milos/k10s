# Recorded observations

Contributed in-app flight runs (`k10s --bench`) from machines the project does not control. These are
not baselines and nothing here gates anything. The comparator consumes the nine headless suite JSON
files described in [the baseline README](../README.md); none of those are in this directory.

The flight harness pins a letterboxed logical viewport of 1600×1000 (`FLIGHT_VIEWPORT`) for planning
and paint, independent of the real window size. Schema v5 reports stamp `machine` and `churn`, plus
the real `window` size and a `resizes` counter as provenance: a resize no longer restarts the flight
(the letterboxed counters are unaffected), but a nonzero `resizes` taints the run's wall-clock
timings and says so in the table. Each segment also carries a `counters` envelope — min, max, and
p99 per work counter over the segment's painted frames — so pan and fly-in segments report the path
they traced rather than the one frame they ended on. `--bench` requires a real `--machine` label
(placeholders such as `my-box` are rejected). Idle `proc_cpu_ms` is `null` / `n/a` on non-Linux
rather than a vacuous `0.0`.

Each file below is a pre-gateability contributor dump kept for history. Every run passed the
placeholder `--machine my-box`, and the churn setting is absent from the output entirely; both were
inferred, and both are recorded as unverified below. New contributions should use a hardware-labelled
`--machine`, an explicit `--churn` (use `0` when testing the idle invariant), and expect viewport
`1600x1000` in the report regardless of window chrome.

| File                                      | Reported machine        | Viewport  | Churn                | Frame p50 |
| ----------------------------------------- | ----------------------- | --------- | -------------------- | --------- |
| `linux-x86_64-i5-12600k-churn0.json`      | i5-12600K (labelled)    | 1600x1000 | 0, stamped           | 6.1 ms    |
| `macos-aarch64-macbook-pro-m1.txt`        | MacBook Pro, M1         | 1512x837  | default, inferred    | 8.3 ms    |
| `windows-x86_64-7800x3d-rtx5070.txt`      | 7800X3D, RTX 5070       | 1600x1000 | default, inferred    | 5.5 ms    |
| `windows-x86_64-9800x3d-rtx5080.txt`      | 9800X3D, RTX 5080       | 1600x1000 | default, inferred    | 6.9 ms    |

The first row is the schema v5 reference run from the baseline host itself (real window 1251x1350,
letterboxed to 1600x1000, `resizes` 0): idle is 0 paints over 5 s at `--churn 0` with 10 ms of
process CPU and a provably steady counter envelope, Z0 static is 198 quads (envelope collapsed to
198..198) at cpu p50 0.30 ms on the 25k platform scene, and every static segment's text cache reads
all hits, zero misses, zero evictions. The envelopes are why the animated segments are now worth
recording at all: the Z2→Z3 fly-in traces glyphs 357..3647, a peak text load no segment-end sample
ever showed. It is what a contributed run should look like.

The macOS chip variant is unconfirmed. A 1512x837 viewport and a 120 Hz frame interval both point at
a 14-inch M1 Pro rather than a 13-inch M1, but neither is recorded in the run. Under the current
harness that physical size would letterbox to logical 1600×1000, so a re-run would be counter-
comparable with the Windows pair.

## What these runs establish

**Visible work is identical across two x86_64 Windows machines with different GPUs.** Both Windows
runs landed on 1600x1000, the viewport the headless suites pin, and every counter in all ten
segments matches byte for byte: `quads`, `lines`, `glyphs`, `icons`, `sats`, `curves`, `edges`,
`hex`, the drawn namespace/workload/pod triple, the three dropped counters, and the text-cache
triple. The two pan segments match as well, and those are single-frame samples read wherever the pan
happened to stop, so the sampling point is deterministic too.

That cross-machine determinism, plus the letterboxed logical viewport now in the harness, is what
makes a gateable cross-platform flight recording possible. These files themselves remain observations:
they predate schema v4 provenance and were not collected under `--churn 0`.

## What these runs do not establish

- **Nothing about the idle invariant.** The three idle segments report 54, 66 and 64 paints over
  5 s, which is the default churn behaving correctly, not a violation: `--churn 0` is the flag that
  tests zero paints at idle, and none of these runs passed it. Damage-driven repaint has still never
  been verified off Wayland.
- **Nothing comparable from the macOS run's counters (as recorded).** The viewport differs, and the
  hex backdrop snaps its radius to powers of two (`crates/k10s-map/src/hex.rs`), so `bg_cells` can
  move by ~4x on a small change in fit zoom. `hex 198` at Z0 against the Windows pair's `504` is
  that snapping, and the coincidence with the 198-region count is exactly a coincidence. Re-run on
  current k10s to get letterboxed counters.
- **No usable cross-machine timing comparison.** The 9800X3D reports 1.8x to 2.4x lower `cpu_ms`
  than the 7800X3D, far more than those parts differ, so at least one run carries an environmental
  or GPU back-pressure term -- `cpu_ms` is wall-clock around `paint_quads` (§6.3). Generation is also
  slower on both Windows machines (4.8 ms) than the older Linux i5 figure (2.9 ms) despite faster
  CPUs; the generator has changed since that figure was taken, so re-measure before treating it as a
  platform effect.

## Defects these runs exposed (fixed on current `dev`)

- `proc_cpu_ms` returned `0.0` on every non-Linux target; it is now `Option` / `n/a` when unmeasurable
  (`crates/k10s-atlas/src/flight.rs`).
- `--machine` accepted a placeholder, and neither the machine nor the churn setting was stamped into
  the output; `--bench` now requires a usable `--machine`, the report stamps both (from schema v4
  onward), and the idle line names the churn it ran under.
- The idle line reported paints without naming the churn value it ran under.

# k10s

Navigate Kubernetes clusters like a map. Pure Rust, GPU rendered.

```sh
cargo run --release --bin k10s
# or: cargo run --release --bin k10s -- --objects 50000 --seed 55 --scenario platform
```

```sh
cargo test
cargo run --release --bin k10s -- --bench --machine linux-x86_64-i5-12600k
cargo run --release --bin k10s -- --bench --json --machine linux-x86_64-i5-12600k
cargo bench -p k10s-atlas --features testing --bench cull  # headless cull bench, no GPU
cargo bench -p k10s-world --bench publish                  # headless layout and publish bench
```

## Architecture

Directory names describe responsibilities. Cargo package names keep the `k10s-`
prefix so commands and dependency names remain unambiguous.

| Directory | Responsibility |
| --- | --- |
| `crates/core` | Dense resource identities, bounded ingestion and immutable scene contracts. |
| `crates/atlas` | Domain-independent geometry, culling, camera motion and frame accounting. |
| `crates/world` | Bevy ECS scheduling over flat topology arrays and pooled snapshot publication. |
| `crates/provider` | UI-independent service requests, labelled outcomes and connection selection. |
| `crates/data` | Discovery, transport, watches and bounded upstream API clients. |
| `crates/app` | Runtime composition and conversion from data answers to provider outcomes. |
| `crates/map`, `crates/shell` | Starmap painting and operator-facing views. |
| `crates/theme`, `crates/assets` | Presentation values, indexed glyphs, fonts and artwork. |
| `crates/edit` | Text, syntax and document state without a window dependency. |
| `crates/clustergen`, `crates/bench`, `tools/paint-bench` | Deterministic fixtures and measurement. |

The provider depends only on the core model; the data plane does not depend on
views. The dependency gate enforces these boundaries. Rendering consumes shared
snapshots and prepared outcomes, and never waits for network I/O. The world uses
stable slots and contiguous arrays inside ECS resources; an entity abstraction
does not sit between the painter and each visible object.

An upstream integration belongs in `crates/data/src/integrations`. Keep discovery,
bounded decoding and request construction there, scheduling in `data::read`,
operator-facing requests and outcomes in `provider`, and conversions in
`app::provider`. Absence, denied access, partial results and server failures must
remain distinguishable. Every request shape needs a scripted API-server test;
writes, proxies, upgrades and controller behavior also need live lab proof.
Confirmed writes use the interactive transport, which does not resend a request
after transmission. Existing adapters expose specific supported operations;
their presence does not imply complete coverage of an upstream API.

For presentation changes, `map::frame` owns the shared traversal, `map::paint`
draws its prepared output, and `map::view` owns input and frame scheduling.
Selection identity and presentation milestones have separate modules. Glyph
artwork and its dense-ID lookup live in `assets`, so replacing an icon does not
require changing traversal or input handling. Keep the independently written
cull oracle and the allocation, idle, startup and timing gates intact.

## Build and assets

gpui is pinned to [my Zed fork](https://github.com/nikolic-milos/zed)
(`k10s/batched-paint-quads`, rev `1997311daedbd90333b6710634d2cfbd81b33306`) for
batched quad submission. `Cargo.toml` carries the same full revision.

Building on Linux needs the GPU, Wayland/X11 and font system libraries the CI
job installs; `.github/workflows/ci.yml` holds that list, and it is the one that
is kept current.

Icons: workload kind glyphs derive from the Kubernetes icon set (CC BY 4.0),
tool logos come from [simple-icons](https://github.com/simple-icons/simple-icons)
(CC0). Details in `crates/assets/assets/icons/ATTRIBUTION.md`.

Typography: Inter, Lilex and League Spartan are embedded from
`crates/assets/assets/fonts` under the SIL Open Font License; each family's
licence file sits beside it. Nothing is fetched at runtime.

Desktop integration: `crates/assets/assets/linux/README.md` says where the
desktop entry and the hicolor icon set install, and why the window's app id has
to equal the desktop file's basename.

Contributions: see [AI_POLICY.md](AI_POLICY.md). The performance budgets a
change is held to, and how to reproduce them, are in
[benchmarks/README.md](benchmarks/README.md); the dependency ceilings and
licence policy are `benchmarks/dependency-budget.json` and `deny.toml`.

## License

Copyright (C) 2026 Miloš Nikolić. [AGPL-3.0-or-later](LICENSE).

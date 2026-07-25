# k10s

Navigate Kubernetes clusters like a map. Pure Rust, GPU rendered.

```sh
cargo run --release
# or: cargo run --release -- --objects 50000 --seed 55 --scenario platform
```

```sh
cargo test
cargo run --release -- --bench                            # local flight bench
cargo run --release -- --bench --json --machine my-box     # JSON report on stdout
cargo bench -p k10s-atlas --features testing --bench cull  # headless cull bench, no GPU
cargo bench -p k10s-world --bench publish                  # headless layout and publish bench
```

## Notes

gpui is pinned to [my Zed fork](https://github.com/nikolic-milos/zed)
(`k10s/batched-paint-quads`, rev `1997311dae`) for batched quad submission.

Icons: workload kind glyphs derive from the Kubernetes icon set (CC BY 4.0),
tool logos come from [simple-icons](https://github.com/simple-icons/simple-icons)
(CC0). Details in `crates/k10s-map/assets/icons/ATTRIBUTION.md`.

Contributions: see [AI_POLICY.md](AI_POLICY.md).

## License

Copyright (C) 2026 Miloš Nikolić. [AGPL-3.0-or-later](LICENSE).

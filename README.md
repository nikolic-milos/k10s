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

## Notes

gpui is pinned to [my Zed fork](https://github.com/nikolic-milos/zed)
(`k10s/batched-paint-quads`, rev `1997311dae`) for batched quad submission.

Icons: workload kind glyphs derive from the Kubernetes icon set (CC BY 4.0),
tool logos come from [simple-icons](https://github.com/simple-icons/simple-icons)
(CC0). Details in `crates/k10s-map/assets/icons/ATTRIBUTION.md`.

Typography: Inter, Lilex and League Spartan are embedded from
`crates/k10s-assets/assets/fonts` under the SIL Open Font License; each family's
licence file sits beside it. Nothing is fetched at runtime.

Desktop integration: `crates/k10s-assets/assets/linux/README.md` says where the
desktop entry and the hicolor icon set install, and why the window's app id has
to equal the desktop file's basename.

Contributions: see [AI_POLICY.md](AI_POLICY.md).

## License

Copyright (C) 2026 Miloš Nikolić. [AGPL-3.0-or-later](LICENSE).

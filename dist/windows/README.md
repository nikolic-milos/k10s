# Windows MSI

`build-msi.sh` is a stub. It prints WiX and Authenticode tools as present or
`Absent` and exits 2. Nothing here invokes cargo-wix or cargo-bundle.

A signed installer would need, on Windows:

1. `cargo build --release --locked --bin k10s`
2. WiX (or equivalent) authoring that installs the exe, a Start Menu shortcut,
   and `ATTRIBUTION.md` next to the binary.
3. Authenticode signing (`signtool` or `osslsigncode`) of both the exe and the
   MSI.
4. winget metadata is a later step, after the MSI exists.

Until those exist, distribute the release binary plus the notices from
`k10s --attribution`. Do not add cargo-wix or cargo-bundle to the workspace:
either would raise `benchmarks/dependency-budget.json`.

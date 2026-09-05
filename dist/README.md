# Distribution

Packaging is shell scripts over a release binary. cargo-deb and cargo-bundle
are not workspace members: either one would raise the dependency budget
(830 / 817 / 29). The scripts call `dpkg-deb` and `appimagetool` when those
tools are on PATH, and print `tool: Absent` then exit 2 when they are not.

## What a human runs to pack

From the repository root, on the machine that should own the artifact:

```sh
cargo build --release --locked --bin k10s
dist/linux/build-deb.sh
dist/linux/build-appimage.sh
```

Outputs land in `target/dist/`. The scripts never invoke cargo, never start
the GUI, and never run a benchmark. They require `target/release/k10s` and
`crates/k10s-assets/assets/linux/k10s.desktop`.

Icon notices travel with the binary (`k10s --attribution`) and are copied
into `/usr/share/doc/k10s/ATTRIBUTION.md` inside the Linux packages.

## Other platforms

| Script | Status |
| --- | --- |
| `dist/macos/build-signed-dmg.sh` | stub; echoes what is missing and exits 2 |
| `dist/windows/build-msi.sh` | stub; echoes what is missing and exits 2 |

See the README beside each stub. Signed notarized `.dmg`, MSI, winget,
`.rpm`, and Flatpak are not produced here.

## Flight gate

Linux packages do not prove the running app. The one measurement that is
the app is the scripted flight and the startup bench, on the labelled
perf host only: `benchmarks/check-flight-gate.sh`. Do not run that script
with `--run` on a desktop session you care about.

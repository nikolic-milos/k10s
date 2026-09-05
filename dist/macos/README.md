# macOS .dmg

`build-signed-dmg.sh` is a stub. It prints the local signing tools as present
or `Absent` and exits 2. Nothing here invokes cargo-bundle.

A signed, notarized disk image would need, on a Mac:

1. `cargo build --release --locked --bin k10s`
2. An app bundle with `k10s` as `CFBundleIdentifier` matching the window
   `app_id` (`k10s_assets::APP_ID`).
3. Developer ID Application codesign of the bundle and every nested lib.
4. `notarytool submit` and `stapler staple`.
5. `hdiutil` (or create-dmg) wrapping the stapled bundle.

Until those exist, distribute the release binary plus the notices from
`k10s --attribution`. Do not add cargo-bundle to the workspace: it would
raise `benchmarks/dependency-budget.json`.

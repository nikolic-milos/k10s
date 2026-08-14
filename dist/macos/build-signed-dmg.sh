#!/bin/sh
# Signed, notarized .dmg. Not implemented: this tree has no Apple signing
# identity, no notarization profile, and no cargo-bundle dependency (that
# crate would raise the workspace dependency budget).
#
# Usage:
#   dist/macos/build-signed-dmg.sh

set -eu

echo "$0: signed and notarized .dmg is not implemented" >&2
echo "codesign: $(command -v codesign 2>/dev/null || echo Absent)" >&2
echo "notarytool: $(command -v notarytool 2>/dev/null || echo Absent)" >&2
echo "hdiutil: $(command -v hdiutil 2>/dev/null || echo Absent)" >&2
echo "create-dmg: $(command -v create-dmg 2>/dev/null || echo Absent)" >&2
exit 2

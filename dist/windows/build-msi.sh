#!/bin/sh
# Signed MSI. Not implemented: this tree has no WiX authoring, no Authenticode
# certificate, and no cargo-wix / cargo-bundle dependency (either crate would
# raise the workspace dependency budget).
#
# Usage:
#   dist/windows/build-msi.sh

set -eu

echo "$0: signed MSI is not implemented" >&2
echo "wix: $(command -v wix 2>/dev/null || echo Absent)" >&2
echo "candle: $(command -v candle 2>/dev/null || echo Absent)" >&2
echo "light: $(command -v light 2>/dev/null || echo Absent)" >&2
echo "signtool: $(command -v signtool 2>/dev/null || echo Absent)" >&2
echo "osslsigncode: $(command -v osslsigncode 2>/dev/null || echo Absent)" >&2
exit 2

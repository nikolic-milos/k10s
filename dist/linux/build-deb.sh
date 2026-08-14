#!/bin/sh
# Build a .deb from target/release/k10s and the shipped desktop entry.
#
# Does not compile, does not start the GUI, does not run a benchmark.
# Requires dpkg-deb on PATH; prints "dpkg-deb: Absent" and exits 2 without it.
#
# Usage (from the repository root, after a release build):
#   dist/linux/build-deb.sh

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)
bin=$root/target/release/k10s
desktop=$root/crates/k10s-assets/assets/linux/k10s.desktop
icons=$root/crates/k10s-assets/assets/linux/icons
notice=$root/crates/k10s-map/assets/icons/ATTRIBUTION.md
license=$root/LICENSE
out_dir=$root/target/dist
sizes="16 24 32 48 64 128 256 512"

if [ ! -f "$bin" ]; then
    echo "$0: missing $bin" >&2
    echo "$0: build it with: cargo build --release --locked --bin k10s" >&2
    exit 1
fi
if [ ! -f "$desktop" ]; then
    echo "$0: missing $desktop" >&2
    exit 1
fi

if command -v dpkg-deb >/dev/null 2>&1; then
    echo "dpkg-deb: $(command -v dpkg-deb)"
else
    echo "dpkg-deb: Absent" >&2
    exit 2
fi

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
if [ -z "$version" ]; then
    echo "$0: could not read workspace version from $root/Cargo.toml" >&2
    exit 1
fi

case $(uname -m) in
    x86_64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *)
        echo "$0: unsupported architecture $(uname -m)" >&2
        exit 1
        ;;
esac

pkg=k10s_${version}_${arch}
stage=$out_dir/$pkg
rm -rf "$stage"
mkdir -p "$stage/DEBIAN"
mkdir -p "$stage/usr/bin"
mkdir -p "$stage/usr/share/applications"
mkdir -p "$stage/usr/share/doc/k10s"

install -Dm755 "$bin" "$stage/usr/bin/k10s"
install -Dm644 "$desktop" "$stage/usr/share/applications/k10s.desktop"
if [ -f "$notice" ]; then
    install -Dm644 "$notice" "$stage/usr/share/doc/k10s/ATTRIBUTION.md"
fi
if [ -f "$license" ]; then
    install -Dm644 "$license" "$stage/usr/share/doc/k10s/copyright"
fi

for size in $sizes; do
    icon=$icons/k10s-$size.png
    if [ -f "$icon" ]; then
        install -Dm644 "$icon" \
            "$stage/usr/share/icons/hicolor/${size}x${size}/apps/k10s.png"
    else
        echo "$0: warning: $icon is missing, skipping ${size}x${size}" >&2
    fi
done

# Runtime .so depends (wayland, vulkan, libxkbcommon, ...) are not declared
# yet; the package is a binary plus desktop metadata.
cat >"$stage/DEBIAN/control" <<EOF
Package: k10s
Version: $version
Section: devel
Priority: optional
Architecture: $arch
Maintainer: Miloš Nikolić <milosnikolic@milosnikolic.de>
Homepage: https://github.com/nikolic-milos/k10s
Description: Navigate a Kubernetes cluster as a map
 GPU-rendered Kubernetes Starmap. Workload kind glyphs derived from the
 Kubernetes icon set are CC BY 4.0; see /usr/share/doc/k10s/ATTRIBUTION.md.
EOF

mkdir -p "$out_dir"
deb=$out_dir/$pkg.deb
dpkg-deb --build --root-owner-group "$stage" "$deb"
echo "k10s: wrote $deb"

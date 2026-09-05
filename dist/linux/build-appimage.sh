#!/bin/sh
# Build an AppImage from target/release/k10s and the shipped desktop entry.
#
# Does not compile, does not start the GUI, does not run a benchmark.
# Requires appimagetool on PATH (or $APPIMAGETOOL); prints "appimagetool: Absent"
# and exits 2 without it.
#
# Usage (from the repository root, after a release build):
#   dist/linux/build-appimage.sh

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)
bin=$root/target/release/k10s
desktop=$root/crates/k10s-assets/assets/linux/k10s.desktop
icons=$root/crates/k10s-assets/assets/linux/icons
notice=$root/crates/k10s-map/assets/icons/ATTRIBUTION.md
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

appimagetool=${APPIMAGETOOL-}
if [ -z "$appimagetool" ] && command -v appimagetool >/dev/null 2>&1; then
    appimagetool=$(command -v appimagetool)
fi
if [ -n "$appimagetool" ]; then
    echo "appimagetool: $appimagetool"
else
    echo "appimagetool: Absent" >&2
    exit 2
fi

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
if [ -z "$version" ]; then
    echo "$0: could not read workspace version from $root/Cargo.toml" >&2
    exit 1
fi
arch=$(uname -m)

appdir=$out_dir/k10s.AppDir
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
mkdir -p "$appdir/usr/share/applications"
mkdir -p "$appdir/usr/share/doc/k10s"

install -Dm755 "$bin" "$appdir/usr/bin/k10s"
install -Dm644 "$desktop" "$appdir/usr/share/applications/k10s.desktop"
install -Dm644 "$desktop" "$appdir/k10s.desktop"
if [ -f "$notice" ]; then
    install -Dm644 "$notice" "$appdir/usr/share/doc/k10s/ATTRIBUTION.md"
fi

root_icon=
for size in $sizes; do
    icon=$icons/k10s-$size.png
    if [ -f "$icon" ]; then
        install -Dm644 "$icon" \
            "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/k10s.png"
        root_icon=$icon
    else
        echo "$0: warning: $icon is missing, skipping ${size}x${size}" >&2
    fi
done
if [ -n "$root_icon" ]; then
    install -Dm644 "$root_icon" "$appdir/k10s.png"
else
    echo "$0: warning: no hicolor PNG found; appimagetool may refuse the AppDir" >&2
fi

cat >"$appdir/AppRun" <<'EOF'
#!/bin/sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$here/usr/bin/k10s" "$@"
EOF
chmod +x "$appdir/AppRun"

mkdir -p "$out_dir"
image=$out_dir/k10s-${version}-${arch}.AppImage
ARCH=$arch "$appimagetool" "$appdir" "$image"
echo "k10s: wrote $image"

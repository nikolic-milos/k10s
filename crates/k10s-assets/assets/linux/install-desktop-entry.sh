#!/bin/sh
# Install k10s's desktop entry and icons for the current user.
#
# Nothing here is needed to *run* k10s. It is needed for the window to have an
# icon on Wayland -- which has no per-window icon protocol and matches the app id
# against a desktop entry's basename instead -- and for k10s to appear in an
# application launcher.
#
# Idempotent: run it again after a rebuild and it overwrites what it wrote before
# and nothing else. It writes only under $XDG_DATA_HOME (default ~/.local/share)
# and never outside it. Pass --uninstall to remove exactly what it installed.
#
# Usage:
#   crates/k10s-assets/assets/linux/install-desktop-entry.sh [--uninstall]

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
applications=$data/applications
hicolor=$data/icons/hicolor
sizes="16 24 32 48 64 128 256 512"
entry=k10s.desktop
# One value in four places: this basename, StartupWMClass, k10s_assets::APP_ID,
# and WindowOptions::app_id. A mismatch shows a generic icon and logs nothing,
# by anybody.
id=k10s

uninstall=false
case "${1-}" in
    --uninstall) uninstall=true ;;
    "") ;;
    *)
        echo "$0: unknown argument $1; usage: $0 [--uninstall]" >&2
        exit 2
        ;;
esac

refresh() {
    # Both caches are optional. A desktop that has neither reads the directories
    # directly, and a launcher that wants them will rebuild on its own schedule,
    # so a missing tool is not a failed install.
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$applications" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$hicolor" 2>/dev/null || true
    fi
}

if [ "$uninstall" = true ]; then
    rm -f "$applications/$entry"
    for size in $sizes; do
        rm -f "$hicolor/${size}x${size}/apps/$id.png"
    done
    refresh
    echo "k10s: removed $applications/$entry and its icons"
    exit 0
fi

if [ ! -f "$here/$entry" ]; then
    echo "$0: $here/$entry is missing; run this from a checkout" >&2
    exit 1
fi

# The entry names `Exec=k10s`, and a desktop entry does not consult a shell: no
# ~ and no $HOME expansion. If the binary is not on PATH the launcher will find
# nothing, so say so here rather than leaving a menu item that does nothing.
if ! command -v "$id" >/dev/null 2>&1; then
    echo "$0: warning: $id is not on PATH, so the launcher entry will not start" >&2
    echo "$0: warning: either install the binary on PATH or make Exec= an absolute path" >&2
fi

install -Dm644 "$here/$entry" "$applications/$entry"
for size in $sizes; do
    icon=$here/icons/$id-$size.png
    if [ -f "$icon" ]; then
        install -Dm644 "$icon" "$hicolor/${size}x${size}/apps/$id.png"
    else
        echo "$0: warning: $icon is missing, skipping ${size}x${size}" >&2
    fi
done
refresh

echo "k10s: installed $applications/$entry"
echo "k10s: installed icons under $hicolor"

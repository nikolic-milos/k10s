# Installing the desktop entry and icons

The binary carries these files, but a desktop environment only reads them from
disk. Nothing here is needed to *run* k10s; it is needed for the window to have
an icon on Wayland, and for k10s to appear in an application launcher.

## Why the icon needs any of this

X11 lets a window carry its own icon, and k10s sets one (`WindowOptions::icon`,
decoded from `brand/k10s.png`). Wayland has no such protocol. A Wayland
compositor takes the window's *app id*, looks for `<app id>.desktop`, and uses
that entry's `Icon=` line. So the icon depends on four strings being identical:

    k10s_assets::APP_ID        = "k10s"
    WindowOptions::app_id      = "k10s"
    this file's basename        k10s.desktop
    StartupWMClass=            k10s

If they diverge the window gets a generic icon and nothing is logged, by
anyone. A test in `k10s-assets` pins the desktop entry against `APP_ID` for
exactly that reason.

## Per user

    install -Dm644 crates/k10s-assets/assets/linux/k10s.desktop \
      ~/.local/share/applications/k10s.desktop

    for size in 16 24 32 48 64 128 256 512; do
      install -Dm644 crates/k10s-assets/assets/linux/icons/k10s-$size.png \
        ~/.local/share/icons/hicolor/${size}x${size}/apps/k10s.png
    done

    update-desktop-database ~/.local/share/applications
    gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor

## System-wide

The same two trees under `/usr/share` instead of `~/.local/share`:

    install -Dm644 .../k10s.desktop /usr/share/applications/k10s.desktop
    install -Dm644 .../icons/k10s-$size.png \
      /usr/share/icons/hicolor/${size}x${size}/apps/k10s.png

`Exec=k10s` assumes the binary is on `PATH`. If it is not, make that line an
absolute path -- a desktop entry does not consult a shell, so `~` and `$HOME`
do not expand in it.

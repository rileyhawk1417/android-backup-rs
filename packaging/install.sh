#!/bin/sh
# Install (or remove) the portable Linux build for the current user.
#
# Ships inside the Linux tarball. No root, no package manager: it copies the
# binary into ~/.local/bin and registers the launcher entry and icon so the app
# shows up in the desktop's application menu.
#
#   ./install.sh              # ~/.local  (PREFIX overrides)
#   ./install.sh --uninstall

set -eu

prefix="${PREFIX:-$HOME/.local}"
here="$(dirname "$(readlink -f "$0")")"

uninstall=0
case "${1:-}" in
    --uninstall) uninstall=1 ;;
    "") ;;
    *) echo "usage: $0 [--uninstall]" >&2; exit 2 ;;
esac

bin="$prefix/bin/android-backup"
desktop="$prefix/share/applications/android-backup.desktop"
icon="$prefix/share/icons/hicolor/256x256/apps/android-backup.png"

if [ "$uninstall" = 1 ]; then
    for path in "$bin" "$desktop" "$icon"; do
        if [ -e "$path" ]; then
            rm -f "$path"
            echo "removed $path"
        fi
    done
else
    for source in android-backup android-backup.desktop icon.png; do
        if [ ! -f "$here/$source" ]; then
            echo "install.sh: $here/$source is missing — unpack the whole archive" >&2
            exit 1
        fi
    done

    install -Dm755 "$here/android-backup" "$bin"
    install -Dm644 "$here/android-backup.desktop" "$desktop"
    install -Dm644 "$here/icon.png" "$icon"
    echo "installed $bin"
    echo "installed $desktop"
    echo "installed $icon"
fi

# Refresh the menu cache when the tools are present; both are optional and a
# missing one is not an error (the desktop picks the entry up on its own).
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *)
        echo
        echo "$prefix/bin is not on your PATH. Add this to your shell profile:"
        echo "    export PATH=\"$prefix/bin:\$PATH\""
        ;;
esac

#!/bin/sh
# SPDX-FileCopyrightText: 2026 WarpCoreDev
# SPDX-License-Identifier: GPL-3.0-or-later
# Makes the development build show its own icon.
#
# Wayland has no protocol for handing the compositor a picture; the icon comes
# from a .desktop file matched against the window's application id. A packaged
# build ships one, so this only matters when running out of target/.
#
# Safe to re-run; removes itself with --uninstall.
set -eu

# The app id the compositor sees. GTK takes it from the binary's name, not from
# the identifier in tauri.conf.json, so the .desktop file is named after that.
APP_ID="shadow-tentacles"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DESKTOP="${XDG_DATA_HOME:-$HOME/.local/share}/applications/$APP_ID.desktop"
ICONS="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$DESKTOP"
    find "$ICONS" -name "$APP_ID.png" -delete 2>/dev/null || true
    echo "removed $DESKTOP and its icons"
    exit 0
fi

BINARY="$ROOT/target/debug/shadow-tentacles"
[ -x "$BINARY" ] || BINARY="$ROOT/target/release/shadow-tentacles"
[ -x "$BINARY" ] || { echo "build it first: cargo build" >&2; exit 1; }

for size in 32 64 128 256 512; do
    dir="$ICONS/${size}x${size}/apps"
    mkdir -p "$dir"
    rsvg-convert -w "$size" -h "$size" "$ROOT/icons/icon.svg" -o "$dir/$APP_ID.png"
done

mkdir -p "$(dirname "$DESKTOP")"
cat > "$DESKTOP" <<DESKTOP_FILE
[Desktop Entry]
Type=Application
Name=Shadow Tentacles
Comment=Local news reader with on-device summarisation
Exec=$BINARY
Icon=$APP_ID
Terminal=false
Categories=Network;News;
StartupWMClass=$APP_ID
DESKTOP_FILE

command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -qtf "$ICONS" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$(dirname "$DESKTOP")" 2>/dev/null || true

echo "installed $DESKTOP"
echo "restart the application to see the icon"

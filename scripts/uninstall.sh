#!/bin/sh
# Remove a Murmur install made by install.sh.
set -eu
prefix="${PREFIX:-$HOME/.local}"
appid=io.github.markmiddo.Murmur
systemctl --user disable --now murmurd.service 2>/dev/null || true
rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/murmurd.service"
rm -f "$prefix/bin/murmurd" "$prefix/bin/murmur-applet" "$prefix/bin/murmur-settings"
rm -f "$prefix/share/applications/$appid.desktop" "$prefix/share/applications/$appid.Settings.desktop"
rm -f "$prefix/share/metainfo/$appid.metainfo.xml"
rm -f "$prefix/share/icons/hicolor/scalable/apps/$appid.svg" "$prefix/share/icons/hicolor/scalable/apps/$appid-symbolic.svg"
echo "Murmur removed. Your settings (~/.config/murmur) and models (~/.local/share/murmur) were kept."

#!/bin/sh
# Install a Murmur release for the current user (no root needed).
set -eu
cd "$(dirname "$0")"

prefix="${PREFIX:-$HOME/.local}"
appid=io.github.markmiddo.Murmur

for tool in wtype wl-copy pw-record; do
    command -v "$tool" >/dev/null 2>&1 || missing="${missing:-} $tool"
done
if [ -n "${missing:-}" ]; then
    echo "Missing:$missing"
    echo "On Pop!_OS / Ubuntu: sudo apt install wtype wl-clipboard pipewire-bin"
    exit 1
fi
if ! id -nG | grep -qw input; then
    echo "Note: you're not in the 'input' group, so Murmur can't see the hotkey yet."
    echo "      Run: sudo usermod -aG input \$USER   then log out and back in."
fi

install -Dm0755 murmurd "$prefix/bin/murmurd"
install -Dm0755 murmur-applet "$prefix/bin/murmur-applet"
install -Dm0755 murmur-settings "$prefix/bin/murmur-settings"
install -Dm0644 res/$appid.desktop "$prefix/share/applications/$appid.desktop"
install -Dm0644 res/$appid.Settings.desktop "$prefix/share/applications/$appid.Settings.desktop"
install -Dm0644 res/$appid.metainfo.xml "$prefix/share/metainfo/$appid.metainfo.xml"
install -Dm0644 res/icons/hicolor/scalable/apps/$appid.svg "$prefix/share/icons/hicolor/scalable/apps/$appid.svg"
install -Dm0644 res/icons/hicolor/scalable/apps/$appid-symbolic.svg "$prefix/share/icons/hicolor/scalable/apps/$appid-symbolic.svg"

unitdir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$unitdir"
sed "s|%h/.local/bin/murmurd|$prefix/bin/murmurd|" res/murmurd.service > "$unitdir/murmurd.service"
systemctl --user daemon-reload
systemctl --user enable --now murmurd.service
systemctl --user restart murmurd.service

echo "Murmur installed."
echo "Add it to your panel: COSMIC Settings -> Desktop -> Panel -> Configure panel applets."

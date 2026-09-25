name := 'murmur'
appid := 'io.github.markmiddo.Murmur'
prefix := env('HOME') / '.local'
unitdir := env('HOME') / '.config/systemd/user'
target := env('CARGO_TARGET_DIR', 'target') / 'release'

default: build

build:
    cargo build --release

test:
    cargo test --workspace

# Install for the current user (no root needed).
install: build
    install -Dm0755 {{target}}/murmurd {{prefix}}/bin/murmurd
    install -Dm0755 {{target}}/murmur-applet {{prefix}}/bin/murmur-applet
    install -Dm0755 {{target}}/murmur-settings {{prefix}}/bin/murmur-settings
    install -Dm0644 res/{{appid}}.Settings.desktop {{prefix}}/share/applications/{{appid}}.Settings.desktop
    install -Dm0644 res/{{appid}}.desktop {{prefix}}/share/applications/{{appid}}.desktop
    install -Dm0644 res/{{appid}}.metainfo.xml {{prefix}}/share/metainfo/{{appid}}.metainfo.xml
    install -Dm0644 res/icons/hicolor/scalable/apps/{{appid}}.svg {{prefix}}/share/icons/hicolor/scalable/apps/{{appid}}.svg
    install -Dm0644 res/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg {{prefix}}/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg
    -gtk-update-icon-cache -q {{prefix}}/share/icons/hicolor
    -update-desktop-database -q {{prefix}}/share/applications
    install -Dm0644 res/murmurd.service {{unitdir}}/murmurd.service
    systemctl --user daemon-reload
    systemctl --user enable murmurd.service
    # Restart the engine and the panel so the new build takes over.
    systemctl --user restart murmurd.service
    -pkill -x cosmic-panel

uninstall:
    -systemctl --user disable --now murmurd.service
    rm -f {{unitdir}}/murmurd.service
    -pkill -x murmurd
    rm -f {{prefix}}/bin/murmurd {{prefix}}/bin/murmur-applet {{prefix}}/bin/murmur-settings
    rm -f {{prefix}}/share/applications/{{appid}}.Settings.desktop
    rm -f {{prefix}}/share/applications/{{appid}}.desktop {{prefix}}/share/metainfo/{{appid}}.metainfo.xml
    rm -f {{prefix}}/share/icons/hicolor/scalable/apps/{{appid}}.svg {{prefix}}/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg

# Transcribe a 16 kHz mono wav with the configured model (handy for testing).
transcribe wav:
    {{target}}/murmurd --transcribe {{wav}}

# Build and install the Flatpak for the current user.
flatpak:
    flatpak-builder --user --install --force-clean --install-deps-from=flathub build-flatpak flatpak/{{appid}}.json

# Regenerate flatpak/cargo-sources.json after changing dependencies.
flatpak-sources:
    uv run https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py Cargo.lock -o flatpak/cargo-sources.json

# Validate the desktop entries and store metadata.
validate:
    desktop-file-validate res/*.desktop
    appstreamcli validate --no-net res/{{appid}}.metainfo.xml

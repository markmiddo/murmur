# Publishing to the COSMIC Store

COSMIC applets are distributed through the
[pop-os/cosmic-flatpak](https://github.com/pop-os/cosmic-flatpak) repository,
which the COSMIC Store reads.

## Release checklist

1. Bump the version in `Cargo.toml` and add a `<release>` to
   `res/io.github.markmiddo.Murmur.metainfo.xml` and `CHANGELOG.md`.
2. `just validate && just test`
3. If dependencies changed, run `just flatpak-sources`.
4. Build and try the Flatpak locally with `just flatpak`.
5. Tag and push: `git tag v0.1.0 && git push --tags`, then create a GitHub release.

## Submitting

1. Fork `pop-os/cosmic-flatpak` and create `app/io.github.markmiddo.Murmur/`.
2. Copy in `flatpak/io.github.markmiddo.Murmur.json` and
   `flatpak/cargo-sources.json`.
3. In the copied manifest, replace the `dir` source with the release tag:
   ```json
   {
     "type": "git",
     "url": "https://github.com/markmiddo/murmur.git",
     "tag": "v0.1.0"
   }
   ```
4. Test with `just build io.github.markmiddo.Murmur` in that repo, then open a
   pull request.

## Notes for reviewers

- `--device=input` lets Murmur read the push-to-talk key globally. Wayland has
  no hold-to-talk global shortcut API yet, and the Global Shortcuts portal only
  reports activation, not release. Murmur only acts on the configured key.
- `--share=network` downloads the speech model (about 660 MB) on first run
  rather than bundling it in the Flatpak.
- `--filesystem=xdg-run/pipewire-0` is for microphone capture through `pw-record`.
- The speech engine runs as a separate process (`murmurd`) that the applet
  supervises, so a crash in inference never takes down the panel.

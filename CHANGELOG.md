# Changelog

All notable changes to Murmur are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org).

## [0.1.1] - 2026-09-26

### Changed

- The store listing now leads with a screenshot of the panel applet.
- Wording now follows the COSMIC™ trademark policy for third-party apps.

## [0.1.0] - 2026-09-26

First release.

### Added

- Push-to-talk dictation on Right Alt (configurable), typed into the focused
  window through the Wayland virtual keyboard, with a clipboard fallback.
- On-device speech recognition with NVIDIA Parakeet TDT 0.6B: v2 English and
  v3 multilingual, downloaded on first run.
- COSMIC panel applet with a live level meter, recent dictations (click to
  copy), a pause switch and "Dictate now".
- Settings window for the hotkey, model, sounds, timing and word replacements.
- Hot-plug keyboard handling, per-dictation microphone opening, a listener
  watchdog, and automatic engine restarts.

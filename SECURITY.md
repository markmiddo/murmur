# Security

## Reporting a vulnerability

Please report security problems privately through
[GitHub security advisories](https://github.com/markmiddo/murmur/security/advisories/new)
rather than a public issue. You'll get a reply within a few days.

## What Murmur can access

Murmur needs a few permissions that are worth understanding:

- **Keyboard input (`/dev/input`).** Murmur reads raw key events so it can see
  the push-to-talk key in any app. It only acts on the configured key, plus
  "another key was pressed" to cancel. It never records, stores or transmits
  keystrokes.
- **Microphone.** Audio is captured only while the push-to-talk key is held
  (or after you click *Dictate now*). It stays in memory and is discarded once
  transcribed.
- **Typing into windows.** Transcribed text is typed through the Wayland
  virtual keyboard protocol, into whichever window has focus.
- **Network.** Used only to download the speech model from Hugging Face on
  first run. Murmur has no telemetry.

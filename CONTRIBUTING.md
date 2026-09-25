# Contributing to Murmur

Thanks for helping out. Bug reports, ideas and pull requests are all welcome.

## Reporting bugs

Open an issue using the bug report template. The engine log is the most useful
thing you can include:

```sh
journalctl --user -u murmurd --since "10 min ago"
```

## Making changes

1. Fork the repo and create a branch from `main`.
2. Build and install your copy with `just install`. It replaces the running one.
3. Before opening a pull request, make sure these pass:
   ```sh
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
4. Use [Conventional Commits](https://www.conventionalcommits.org) for commit
   messages, e.g. `fix(daemon): release the hotkey when a keyboard disappears`.

## Where things live

- `daemon/`: `murmurd`. Hotkeys (`hotkey.rs`), audio (`audio.rs`), speech
  model (`engine.rs`), typing (`output.rs`), and the D-Bus service and state
  machine (`main.rs`).
- `applet/`: the COSMIC panel applet (`window.rs`), settings window
  (`settings.rs`) and D-Bus client (`dbus.rs`).
- `common/`: config, model catalogue and names shared by both.

## Principles

- **Never fail silently.** If something breaks, the panel icon should say so,
  and the engine should restart rather than sit there deaf.
- **Stay private.** Audio never leaves the machine.
- **Stay light.** One engine process, one model in memory, no background work
  while idle.

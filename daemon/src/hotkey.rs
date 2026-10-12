//! Push-to-talk key listener.
//!
//! Reads every keyboard under /dev/input directly (works on any Wayland
//! compositor). Each device gets its own reader thread; a supervisor rescans
//! every couple of seconds so keyboards that sleep, re-pair or get plugged in
//! later are picked up again. A device vanishing never stops the listener.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use evdev::{Device, EventSummary, KeyCode};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;

const RESCAN_EVERY: Duration = Duration::from_secs(2);

/// Milliseconds since process start at the supervisor's last loop. The
/// controller exits the process if this goes stale so systemd restarts us.
/// Monotonic on purpose: wall-clock time jumps across suspend and would
/// trigger a false alarm on resume.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn beat() {
    // +1 so a beat at t=0 is distinguishable from "never beat".
    HEARTBEAT.store(now_ms() + 1, Ordering::Relaxed);
}

/// Time since the supervisor last looped, or None before its first loop.
pub fn heartbeat_age() -> Option<Duration> {
    match HEARTBEAT.load(Ordering::Relaxed) {
        0 => None,
        b => Some(Duration::from_millis(now_ms().saturating_sub(b - 1))),
    }
}

/// Which push-to-talk key a press belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Types the text into the focused window.
    Dictation,
    /// Sends the text over D-Bus for a voice assistant instead.
    Assistant,
}

/// The pair of keys being watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    pub dictation: KeyCode,
    pub assistant: Option<KeyCode>,
}

impl Keys {
    /// Which slot a key code triggers, if any. The dictation key wins a tie.
    pub fn slot_of(&self, code: KeyCode) -> Option<Slot> {
        if code == self.dictation {
            Some(Slot::Dictation)
        } else if self.assistant == Some(code) {
            Some(Slot::Assistant)
        } else {
            None
        }
    }

    pub fn from_config(cfg: &murmur_common::Config) -> Self {
        Self {
            dictation: parse_key(&cfg.hotkey),
            assistant: parse_assistant_key(cfg),
        }
    }
}

/// The assistant key from config. Off when blank, when it is the same as the
/// dictation key, or when the name isn't a real key (warned about, not fatal).
pub fn parse_assistant_key(cfg: &murmur_common::Config) -> Option<KeyCode> {
    let name = cfg.assistant_key()?;
    let Ok(code) = name.parse::<KeyCode>() else {
        tracing::warn!("Unknown assistant_hotkey {name:?}, assistant key is off");
        return None;
    };
    if code == parse_key(&cfg.hotkey) {
        tracing::warn!("assistant_hotkey is the same key as hotkey, assistant key is off");
        return None;
    }
    Some(code)
}

pub fn parse_key(name: &str) -> KeyCode {
    name.parse().unwrap_or_else(|_| {
        tracing::warn!("Unknown hotkey {name:?}, using KEY_RIGHTALT");
        KeyCode::KEY_RIGHTALT
    })
}

#[derive(Default)]
struct Shared {
    /// Device paths with a live reader thread, and the key codes each supports.
    open: HashMap<PathBuf, HashSet<u16>>,
    /// Device paths on which each key is currently held down.
    held: HashMap<Slot, HashSet<PathBuf>>,
}

impl Shared {
    fn any_held(&self) -> bool {
        self.held.values().any(|h| !h.is_empty())
    }
}

pub struct Listener {
    keys: Arc<Mutex<Keys>>,
    shared: Arc<Mutex<Shared>>,
    events: UnboundedSender<Event>,
}

impl Listener {
    pub fn spawn(keys: Keys, events: UnboundedSender<Event>) -> Self {
        let keys = Arc::new(Mutex::new(keys));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (k, s, e) = (keys.clone(), shared.clone(), events.clone());
        std::thread::Builder::new()
            .name("hotkey-supervisor".into())
            .spawn(move || supervise(k, s, e))
            .expect("spawn hotkey supervisor");
        Self {
            keys,
            shared,
            events,
        }
    }

    pub fn set_keys(&self, keys: Keys) {
        *self.keys.lock().unwrap() = keys;
        // The old keys' releases will never match the new ones, so forget
        // anything held and release it now rather than jamming.
        let mut s = self.shared.lock().unwrap();
        for (slot, paths) in s.held.drain() {
            if !paths.is_empty() {
                let _ = self.events.send(Event::HotkeyUp(slot));
            }
        }
    }
}

fn supervise(keys: Arc<Mutex<Keys>>, shared: Arc<Mutex<Shared>>, events: UnboundedSender<Event>) {
    let mut last_warning: Option<String> = None;
    let mut last_assistant_missing = false;
    loop {
        beat();
        let watched = *keys.lock().unwrap();
        let hotkey = watched.dictation;
        let mut permission_denied = false;

        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_event = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"));
                if !is_event || shared.lock().unwrap().open.contains_key(&path) {
                    continue;
                }
                match Device::open(&path) {
                    Ok(dev) => {
                        let Some(caps) = keyboard_caps(&dev, watched) else {
                            continue;
                        };
                        tracing::info!(
                            "Listening on {} ({})",
                            dev.name().unwrap_or("keyboard"),
                            path.display()
                        );
                        shared.lock().unwrap().open.insert(path.clone(), caps);
                        let (k, s, e) = (keys.clone(), shared.clone(), events.clone());
                        std::thread::Builder::new()
                            .name(format!("hotkey-{}", path.display()))
                            .spawn(move || read_device(dev, path, k, s, e))
                            .expect("spawn hotkey reader");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                        permission_denied = true;
                    }
                    Err(_) => {}
                }
            }
        }

        let (any_open, has_hotkey) = {
            let s = shared.lock().unwrap();
            (
                !s.open.is_empty(),
                s.open.values().any(|caps| caps.contains(&hotkey.code())),
            )
        };
        let warning = if has_hotkey {
            None
        } else if permission_denied && !any_open {
            Some("Can't read keyboards. Add yourself to the input group: sudo usermod -aG input $USER, then log out and in.".to_string())
        } else if any_open {
            let label = murmur_common::hotkey_label(&format!("{hotkey:?}"));
            Some(format!(
                "No connected keyboard has a {label} key. Pick another in Settings."
            ))
        } else {
            Some("No keyboard found.".to_string())
        };
        if let Some(code) = watched.assistant {
            let missing = !shared
                .lock()
                .unwrap()
                .open
                .values()
                .any(|caps| caps.contains(&code.code()));
            if missing && !last_assistant_missing {
                tracing::warn!("No connected keyboard has the assistant key {code:?}");
            }
            last_assistant_missing = missing;
        }
        if warning != last_warning {
            match &warning {
                Some(msg) => {
                    tracing::warn!("{msg}");
                    let _ = events.send(Event::KeyboardsMissing(msg.clone()));
                }
                None => {
                    let _ = events.send(Event::KeyboardsOk);
                }
            }
            last_warning = warning;
        }

        std::thread::sleep(RESCAN_EVERY);
    }
}

/// Keys a device supports, if it is worth listening to: a typing keyboard
/// (for "another key pressed" cancelling) or anything with the hotkey, such
/// as a macropad or foot pedal.
fn keyboard_caps(dev: &Device, watched: Keys) -> Option<HashSet<u16>> {
    let keys = dev.supported_keys()?;
    let typing = keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z);
    (typing
        || keys.contains(watched.dictation)
        || watched.assistant.is_some_and(|k| keys.contains(k)))
    .then(|| keys.iter().map(|k| k.code()).collect())
}

fn read_device(
    mut dev: Device,
    path: PathBuf,
    keys: Arc<Mutex<Keys>>,
    shared: Arc<Mutex<Shared>>,
    events: UnboundedSender<Event>,
) {
    loop {
        let batch = match dev.fetch_events() {
            Ok(evs) => evs.collect::<Vec<_>>(),
            Err(err) => {
                tracing::info!("Keyboard {} went away: {err}", path.display());
                break;
            }
        };
        let watched = *keys.lock().unwrap();
        for ev in batch {
            let EventSummary::Key(_, code, value) = ev.destructure() else {
                continue;
            };
            let mut s = shared.lock().unwrap();
            if let Some(slot) = watched.slot_of(code) {
                match value {
                    1 => {
                        let held = s.held.entry(slot).or_default();
                        let first = held.is_empty();
                        held.insert(path.clone());
                        if first {
                            let _ = events.send(Event::HotkeyDown(slot));
                        }
                    }
                    0 => {
                        let held = s.held.entry(slot).or_default();
                        if held.remove(&path) && held.is_empty() {
                            let _ = events.send(Event::HotkeyUp(slot));
                        }
                    }
                    _ => {} // autorepeat
                }
            } else if value == 1 && code.code() < 0x100 && s.any_held() {
                // Another key pressed while held: the user wants Alt as a modifier.
                let _ = events.send(Event::OtherKey);
            }
        }
    }

    // Device gone. Release the hotkey if it was held here so we never get stuck recording.
    let mut s = shared.lock().unwrap();
    s.open.remove(&path);
    for slot in [Slot::Dictation, Slot::Assistant] {
        let held = s.held.entry(slot).or_default();
        if held.remove(&path) && held.is_empty() {
            let _ = events.send(Event::HotkeyUp(slot));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_common::Config;

    fn keys(assistant: Option<KeyCode>) -> Keys {
        Keys {
            dictation: KeyCode::KEY_RIGHTALT,
            assistant,
        }
    }

    #[test]
    fn slot_routing() {
        let k = keys(Some(KeyCode::KEY_RIGHTCTRL));
        assert_eq!(k.slot_of(KeyCode::KEY_RIGHTALT), Some(Slot::Dictation));
        assert_eq!(k.slot_of(KeyCode::KEY_RIGHTCTRL), Some(Slot::Assistant));
        assert_eq!(k.slot_of(KeyCode::KEY_A), None);
    }

    #[test]
    fn assistant_off_routes_nothing() {
        let k = keys(None);
        assert_eq!(k.slot_of(KeyCode::KEY_RIGHTCTRL), None);
    }

    fn cfg(hotkey: &str, assistant: &str) -> Config {
        Config {
            hotkey: hotkey.into(),
            assistant_hotkey: assistant.into(),
            ..Config::default()
        }
    }

    #[test]
    fn assistant_key_validation() {
        assert_eq!(parse_assistant_key(&cfg("KEY_RIGHTALT", "")), None);
        assert_eq!(
            parse_assistant_key(&cfg("KEY_RIGHTALT", "KEY_F13")),
            Some(KeyCode::KEY_F13)
        );
        assert_eq!(parse_assistant_key(&cfg("KEY_RIGHTALT", "KEY_NOPE")), None);
        assert_eq!(
            parse_assistant_key(&cfg("KEY_RIGHTALT", "KEY_RIGHTALT")),
            None
        );
        // An unknown dictation key falls back to Right Alt, so that clashes too.
        assert_eq!(parse_assistant_key(&cfg("KEY_BOGUS", "KEY_RIGHTALT")), None);
    }
}

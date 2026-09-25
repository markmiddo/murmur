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
    /// Device paths on which the hotkey is currently held down.
    held: HashSet<PathBuf>,
}

pub struct Listener {
    key: Arc<Mutex<KeyCode>>,
    shared: Arc<Mutex<Shared>>,
    events: UnboundedSender<Event>,
}

impl Listener {
    pub fn spawn(key: KeyCode, events: UnboundedSender<Event>) -> Self {
        let key = Arc::new(Mutex::new(key));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (k, s, e) = (key.clone(), shared.clone(), events.clone());
        std::thread::Builder::new()
            .name("hotkey-supervisor".into())
            .spawn(move || supervise(k, s, e))
            .expect("spawn hotkey supervisor");
        Self {
            key,
            shared,
            events,
        }
    }

    pub fn set_key(&self, key: KeyCode) {
        *self.key.lock().unwrap() = key;
        // The old key's release will never match the new key, so forget
        // anything held and release it now rather than jamming.
        let mut s = self.shared.lock().unwrap();
        if !s.held.is_empty() {
            s.held.clear();
            let _ = self.events.send(Event::HotkeyUp);
        }
    }
}

fn supervise(key: Arc<Mutex<KeyCode>>, shared: Arc<Mutex<Shared>>, events: UnboundedSender<Event>) {
    let mut last_warning: Option<String> = None;
    loop {
        beat();
        let hotkey = *key.lock().unwrap();
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
                        let Some(caps) = keyboard_caps(&dev, hotkey) else {
                            continue;
                        };
                        tracing::info!(
                            "Listening on {} ({})",
                            dev.name().unwrap_or("keyboard"),
                            path.display()
                        );
                        shared.lock().unwrap().open.insert(path.clone(), caps);
                        let (k, s, e) = (key.clone(), shared.clone(), events.clone());
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
fn keyboard_caps(dev: &Device, hotkey: KeyCode) -> Option<HashSet<u16>> {
    let keys = dev.supported_keys()?;
    let typing = keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z);
    (typing || keys.contains(hotkey)).then(|| keys.iter().map(|k| k.code()).collect())
}

fn read_device(
    mut dev: Device,
    path: PathBuf,
    key: Arc<Mutex<KeyCode>>,
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
        let hotkey = *key.lock().unwrap();
        for ev in batch {
            let EventSummary::Key(_, code, value) = ev.destructure() else {
                continue;
            };
            let mut s = shared.lock().unwrap();
            if code == hotkey {
                match value {
                    1 => {
                        let first = s.held.is_empty();
                        s.held.insert(path.clone());
                        if first {
                            let _ = events.send(Event::HotkeyDown);
                        }
                    }
                    0 if s.held.remove(&path) && s.held.is_empty() => {
                        let _ = events.send(Event::HotkeyUp);
                    }
                    _ => {} // autorepeat
                }
            } else if value == 1 && code.code() < 0x100 && !s.held.is_empty() {
                // Another key pressed while held: the user wants Alt as a modifier.
                let _ = events.send(Event::OtherKey);
            }
        }
    }

    // Device gone. Release the hotkey if it was held here so we never get stuck recording.
    let mut s = shared.lock().unwrap();
    s.open.remove(&path);
    if s.held.remove(&path) && s.held.is_empty() {
        let _ = events.send(Event::HotkeyUp);
    }
}

//! Push-to-talk key listener.
//!
//! Reads every keyboard under /dev/input directly (works on any Wayland
//! compositor). Each device gets its own reader thread; a supervisor rescans
//! every couple of seconds so keyboards that sleep, re-pair or get plugged in
//! later are picked up again. A device vanishing never stops the listener.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evdev::{Device, EventSummary, KeyCode};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;

const RESCAN_EVERY: Duration = Duration::from_secs(2);

/// Unix seconds of the supervisor's last loop. The controller exits the
/// process if this goes stale so systemd can restart us.
pub static HEARTBEAT: AtomicU64 = AtomicU64::new(0);

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn parse_key(name: &str) -> KeyCode {
    name.parse().unwrap_or_else(|_| {
        tracing::warn!("Unknown hotkey {name:?}, using KEY_RIGHTALT");
        KeyCode::KEY_RIGHTALT
    })
}

#[derive(Default)]
struct Shared {
    /// Device paths with a live reader thread.
    open: HashSet<PathBuf>,
    /// Device paths on which the hotkey is currently held down.
    held: HashSet<PathBuf>,
}

pub struct Listener {
    key: Arc<Mutex<KeyCode>>,
}

impl Listener {
    pub fn spawn(key: KeyCode, events: UnboundedSender<Event>) -> Self {
        let key = Arc::new(Mutex::new(key));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let k = key.clone();
        std::thread::Builder::new()
            .name("hotkey-supervisor".into())
            .spawn(move || supervise(k, shared, events))
            .expect("spawn hotkey supervisor");
        Self { key }
    }

    pub fn set_key(&self, key: KeyCode) {
        *self.key.lock().unwrap() = key;
    }
}

fn supervise(key: Arc<Mutex<KeyCode>>, shared: Arc<Mutex<Shared>>, events: UnboundedSender<Event>) {
    let mut warned_no_devices = false;
    loop {
        HEARTBEAT.store(now_secs(), Ordering::Relaxed);
        let hotkey = *key.lock().unwrap();
        let mut found_any = !shared.lock().unwrap().open.is_empty();
        let mut permission_denied = false;

        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_event = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"));
                if !is_event || shared.lock().unwrap().open.contains(&path) {
                    continue;
                }
                match Device::open(&path) {
                    Ok(dev) => {
                        if !is_keyboard(&dev, hotkey) {
                            continue;
                        }
                        tracing::info!(
                            "Listening on {} ({})",
                            dev.name().unwrap_or("keyboard"),
                            path.display()
                        );
                        found_any = true;
                        shared.lock().unwrap().open.insert(path.clone());
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

        if found_any {
            if warned_no_devices {
                let _ = events.send(Event::KeyboardsOk);
            }
            warned_no_devices = false;
        } else if !warned_no_devices {
            warned_no_devices = true;
            let msg = if permission_denied {
                "Can't read keyboards. Add yourself to the input group: sudo usermod -aG input $USER, then log out and in."
            } else {
                "No keyboard found."
            };
            tracing::warn!("{msg}");
            let _ = events.send(Event::KeyboardsMissing(msg.into()));
        }

        std::thread::sleep(RESCAN_EVERY);
    }
}

/// A real keyboard: has letter keys and the configured hotkey.
fn is_keyboard(dev: &Device, hotkey: KeyCode) -> bool {
    dev.supported_keys().is_some_and(|keys| {
        keys.contains(hotkey) && keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z)
    })
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

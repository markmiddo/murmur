//! murmurd: hold a key, speak, release, and the words are typed.

mod audio;
mod engine;
mod hotkey;
mod output;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use murmur_common::{Config, DBUS_NAME, DBUS_PATH, State, hotkey_label, model_by_id};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zbus::fdo::RequestNameFlags;
use zbus::fdo::RequestNameReply;
use zbus::object_server::{InterfaceRef, SignalEmitter};

use crate::audio::{Audio, Tone};
use crate::engine::Engine;
use crate::hotkey::{Keys, Slot};
use crate::output::{Delivery, Replacer};

const HISTORY_LEN: usize = 20;
const HEARTBEAT_STALE: Duration = Duration::from_secs(20);
/// Auto-stop a recording after this long (e.g. a forgotten "Dictate now").
const MAX_RECORDING: Duration = Duration::from_secs(300);

/// Everything that can happen, funnelled into one controller loop.
#[derive(Debug)]
pub enum Event {
    HotkeyDown(Slot),
    HotkeyUp(Slot),
    OtherKey,
    KeyboardsMissing(String),
    KeyboardsOk,
    Level(f32),
    // Model events carry the load generation so a slow, superseded load
    // can't mark a newer one ready or failed.
    Downloading(u64, f64),
    Loading(u64),
    ModelReady(u64),
    ModelFailed(u64, String),
    Transcribed {
        seq: u64,
        result: Result<String, String>,
    },
    FinishRecording(u64),
    MaxLength(u64),
    // From D-Bus
    Toggle,
    SetEnabled(bool),
    ReloadConfig,
    Retry,
    Restart,
}

/// Snapshot published over D-Bus.
#[derive(Clone, Default, PartialEq)]
struct Status {
    state: State,
    detail: String,
    level: f64,
    enabled: bool,
    model: String,
    hotkey: String,
    assistant_hotkey: String,
    history: VecDeque<(i64, String)>,
}

struct Service {
    status: Arc<Mutex<Status>>,
    events: UnboundedSender<Event>,
}

#[zbus::interface(name = "io.github.markmiddo.Murmur1")]
impl Service {
    #[zbus(property)]
    fn state(&self) -> String {
        self.status.lock().unwrap().state.as_str().into()
    }

    /// Human-readable status line, e.g. "Hold Right Alt to dictate".
    #[zbus(property)]
    fn detail(&self) -> String {
        self.status.lock().unwrap().detail.clone()
    }

    /// Microphone level 0..1 while recording.
    #[zbus(property)]
    fn level(&self) -> f64 {
        self.status.lock().unwrap().level
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.status.lock().unwrap().enabled
    }

    #[zbus(property)]
    fn model(&self) -> String {
        self.status.lock().unwrap().model.clone()
    }

    #[zbus(property)]
    fn hotkey(&self) -> String {
        self.status.lock().unwrap().hotkey.clone()
    }

    /// Label of the assistant key, e.g. "Right Ctrl". Empty when it is off.
    #[zbus(property)]
    fn assistant_hotkey(&self) -> String {
        self.status.lock().unwrap().assistant_hotkey.clone()
    }

    /// Recent transcripts as (unix time, text), newest first.
    fn history(&self) -> Vec<(i64, String)> {
        self.status
            .lock()
            .unwrap()
            .history
            .iter()
            .cloned()
            .collect()
    }

    fn toggle_recording(&self) {
        let _ = self.events.send(Event::Toggle);
    }

    fn set_enabled(&self, enabled: bool) {
        let _ = self.events.send(Event::SetEnabled(enabled));
    }

    fn reload_config(&self) {
        let _ = self.events.send(Event::ReloadConfig);
    }

    fn retry(&self) {
        let _ = self.events.send(Event::Retry);
    }

    /// Exit so the supervisor (systemd or the applet) starts a fresh engine.
    fn restart(&self) {
        let _ = self.events.send(Event::Restart);
    }

    #[zbus(signal)]
    async fn transcribed(emitter: &SignalEmitter<'_>, text: &str) -> zbus::Result<()>;

    /// The assistant key went down and recording started.
    #[zbus(signal)]
    async fn assistant_started(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Final text for an assistant turn. Empty means nothing was heard.
    #[zbus(signal)]
    async fn assistant_transcribed(emitter: &SignalEmitter<'_>, text: &str) -> zbus::Result<()>;

    /// The assistant turn ended without text (cancelled, too short or failed).
    #[zbus(signal)]
    async fn assistant_cancelled(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// What a pressed key does when the controller already has a recording.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    Start,
    /// The previous recording is only in its release tail: wrap it up, then start.
    FinishThenStart,
    /// Another recording is live (either key): the press is ignored.
    Ignore,
}

/// `active` is `Some(finishing)` when a recording exists. Only one recording
/// exists at a time, whichever key started it.
fn key_down_action(active: Option<bool>) -> KeyAction {
    match active {
        None => KeyAction::Start,
        Some(true) => KeyAction::FinishThenStart,
        Some(false) => KeyAction::Ignore,
    }
}

/// A release only ends the recording that the same key started.
fn key_up_releases(rec: Option<(Slot, bool)>, released: Slot) -> bool {
    rec.is_some_and(|(slot, from_hotkey)| from_hotkey && slot == released)
}

/// Another key cancels a live, still-held hotkey recording (either key).
fn other_key_cancels(cancel_on_other_key: bool, rec: Option<(bool, bool)>) -> bool {
    cancel_on_other_key && rec.is_some_and(|(from_hotkey, finishing)| from_hotkey && !finishing)
}

/// Where a finished transcript goes.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Typed into the focused window, kept in history, `Transcribed` signal.
    Type(String),
    /// Only the `AssistantTranscribed` signal (empty text is still sent).
    Assistant(String),
}

fn route_text(slot: Slot, text: String) -> Option<Route> {
    match slot {
        Slot::Dictation if text.is_empty() => None,
        Slot::Dictation => Some(Route::Type(text)),
        Slot::Assistant => Some(Route::Assistant(text)),
    }
}

/// Assistant signals queued by the synchronous recording code and emitted in
/// order once the event has been handled.
#[derive(Debug, PartialEq, Eq)]
enum Outgoing {
    Started,
    Transcribed(String),
    Cancelled,
}

struct Recording {
    started: Instant,
    released: Option<Instant>,
    seq: u64,
    slot: Slot,
    from_hotkey: bool,
    finishing: bool,
}

struct Controller {
    cfg: Config,
    replacer: Replacer,
    audio: Audio,
    engine: Engine,
    listener: hotkey::Listener,
    events: UnboundedSender<Event>,
    iface: InterfaceRef<Service>,
    status: Arc<Mutex<Status>>,
    published: Status,

    enabled: bool,
    model_ready: bool,
    model_gen: u64,
    model_state: (State, String),
    keyboard_error: Option<String>,
    recording: Option<Recording>,
    next_seq: u64,
    pending: usize,
    /// In-flight transcriptions: which key started them and when it was released.
    turns: HashMap<u64, (Slot, Option<Instant>)>,
    outbox: Vec<Outgoing>,
    level: f64,
    last_error: Option<(String, Instant)>,
}

impl Controller {
    fn derive_status(&self) -> Status {
        let key = hotkey_label(&self.cfg.hotkey);
        let (state, detail) = if let Some(err) = &self.keyboard_error {
            (State::Error, err.clone())
        } else if !self.model_ready {
            self.model_state.clone()
        } else if self.recording.is_some() {
            (State::Recording, "Listening…".into())
        } else if self.pending > 0 {
            (State::Transcribing, "Transcribing…".into())
        } else if !self.enabled {
            (State::Disabled, "Dictation is paused".into())
        } else if let Some((err, at)) = &self.last_error
            && at.elapsed() < Duration::from_secs(8)
        {
            (State::Idle, err.clone())
        } else {
            (State::Idle, format!("Hold {key} to dictate"))
        };
        Status {
            state,
            detail,
            level: if self.recording.is_some() {
                self.level
            } else {
                0.0
            },
            enabled: self.enabled,
            model: self.cfg.model.clone(),
            hotkey: key,
            assistant_hotkey: hotkey::parse_assistant_key(&self.cfg)
                .map(|_| hotkey_label(self.cfg.assistant_key().unwrap_or_default()))
                .unwrap_or_default(),
            history: self.status.lock().unwrap().history.clone(),
        }
    }

    async fn publish(&mut self) {
        let next = self.derive_status();
        if next == self.published {
            return;
        }
        *self.status.lock().unwrap() = next.clone();
        let old = std::mem::replace(&mut self.published, next);
        let iface = self.iface.get().await;
        let em = self.iface.signal_emitter();
        let new = &self.published;
        if old.state != new.state {
            let _ = iface.state_changed(em).await;
        }
        if old.detail != new.detail {
            let _ = iface.detail_changed(em).await;
        }
        if old.level != new.level {
            let _ = iface.level_changed(em).await;
        }
        if old.enabled != new.enabled {
            let _ = iface.enabled_changed(em).await;
        }
        if old.model != new.model {
            let _ = iface.model_changed(em).await;
        }
        if old.hotkey != new.hotkey {
            let _ = iface.hotkey_changed(em).await;
        }
        if old.assistant_hotkey != new.assistant_hotkey {
            let _ = iface.assistant_hotkey_changed(em).await;
        }
    }

    fn start_recording(&mut self, slot: Slot, from_hotkey: bool) {
        if !self.enabled {
            return;
        }
        // Pressed again during the release tail: wrap up the previous
        // dictation now so this press gets a recording of its own.
        match key_down_action(self.recording.as_ref().map(|r| r.finishing)) {
            KeyAction::Ignore => return,
            KeyAction::FinishThenStart => {
                if let Some(seq) = self.recording.as_ref().map(|r| r.seq) {
                    self.finish(seq);
                }
            }
            KeyAction::Start => {}
        }
        if !self.model_ready {
            self.fail("Speech model is still loading");
            return;
        }
        match self.audio.start() {
            Ok(()) => {
                if self.cfg.sounds {
                    self.audio.tone(Tone::Start);
                }
                self.next_seq += 1;
                self.level = 0.0;
                self.recording = Some(Recording {
                    started: Instant::now(),
                    released: None,
                    seq: self.next_seq,
                    slot,
                    from_hotkey,
                    finishing: false,
                });
                if slot == Slot::Assistant {
                    self.outbox.push(Outgoing::Started);
                }
                let (seq, tx) = (self.next_seq, self.events.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(MAX_RECORDING).await;
                    let _ = tx.send(Event::MaxLength(seq));
                });
            }
            Err(err) => self.fail(&format!("{err:#}")),
        }
    }

    /// Key released: keep listening a moment so the last word isn't clipped.
    fn release(&mut self) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        if rec.finishing {
            return;
        }
        rec.finishing = true;
        rec.released = Some(Instant::now());
        let seq = rec.seq;
        let tail = Duration::from_millis(self.cfg.release_tail_ms);
        let tx = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tail).await;
            let _ = tx.send(Event::FinishRecording(seq));
        });
    }

    fn finish(&mut self, seq: u64) {
        let Some(rec) = self.recording.take_if(|r| r.seq == seq) else {
            return;
        };
        let recorded = rec.started.elapsed();
        // How long the key was actually held, not counting the release tail.
        let held = rec.released.unwrap_or_else(Instant::now) - rec.started;
        let mut samples = self.audio.stop();
        if held < Duration::from_millis(self.cfg.min_hold_ms) {
            self.assistant_ended(rec.slot);
            return; // accidental tap
        }
        let got = Duration::from_secs_f32(samples.len() as f32 / audio::TARGET_RATE as f32);
        // The recorder should have delivered roughly the whole recording. If it
        // didn't, the microphone stalled: say so instead of transcribing scraps.
        if got < recorded.mul_f32(0.5) {
            tracing::warn!("Microphone delivered {got:?} of {recorded:?}");
            self.fail("Microphone didn't pick anything up. Check your input device.");
            self.assistant_ended(rec.slot);
            return;
        }
        // Blank the start chime so the model never "hears" it as a word.
        if self.cfg.sounds {
            let n = (audio::START_TONE.as_secs_f32() * 1.4 * audio::TARGET_RATE as f32) as usize;
            samples.iter_mut().take(n).for_each(|s| *s = 0.0);
        }
        if self.cfg.sounds {
            self.audio.tone(Tone::Stop);
        }
        self.pending += 1;
        self.turns.insert(rec.seq, (rec.slot, rec.released));
        self.engine.transcribe(rec.seq, samples);
    }

    /// An assistant turn ended with no text: tell the client so it can reset.
    fn assistant_ended(&mut self, slot: Slot) {
        if slot == Slot::Assistant {
            self.outbox.push(Outgoing::Cancelled);
        }
    }

    fn cancel(&mut self) {
        if let Some(rec) = self.recording.take() {
            self.assistant_ended(rec.slot);
            let _ = self.audio.stop();
            if self.cfg.sounds {
                self.audio.tone(Tone::Cancel);
            }
        }
    }

    fn fail(&mut self, msg: &str) {
        tracing::warn!("{msg}");
        if self.cfg.sounds {
            self.audio.tone(Tone::Error);
        }
        self.last_error = Some((msg.to_string(), Instant::now()));
        let tx = self.events.clone();
        // Re-publish once the error message expires.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(9)).await;
            let _ = tx.send(Event::Level(0.0));
        });
    }

    /// Emit any queued assistant signals, oldest first.
    async fn flush(&mut self) {
        for out in std::mem::take(&mut self.outbox) {
            let em = self.iface.signal_emitter();
            let _ = match out {
                Outgoing::Started => Service::assistant_started(em).await,
                Outgoing::Transcribed(text) => Service::assistant_transcribed(em, &text).await,
                Outgoing::Cancelled => Service::assistant_cancelled(em).await,
            };
        }
    }

    /// Filler removal and word replacements, shared by both keys.
    fn clean(&self, text: &str) -> String {
        let text = if self.cfg.remove_fillers {
            output::strip_fillers(text)
        } else {
            text.to_string()
        };
        self.replacer.apply(&text)
    }

    async fn deliver(&mut self, slot: Slot, released: Option<Instant>, text: String) {
        let text = self.clean(&text);
        let mut text = match route_text(slot, text) {
            None => return,
            Some(Route::Assistant(text)) => {
                self.outbox.push(Outgoing::Transcribed(text));
                self.flush().await;
                if let Some(at) = released {
                    tracing::info!(
                        "Assistant turn: {} ms from release to text",
                        at.elapsed().as_millis()
                    );
                }
                return;
            }
            Some(Route::Type(text)) => text,
        };
        {
            let mut s = self.status.lock().unwrap();
            s.history
                .push_front((chrono::Local::now().timestamp(), text.clone()));
            s.history.truncate(HISTORY_LEN);
        }
        let _ = Service::transcribed(self.iface.signal_emitter(), &text).await;
        if self.cfg.trailing_space {
            text.push(' ');
        }
        let result = tokio::task::spawn_blocking(move || output::type_text(&text)).await;
        match result {
            Ok(Ok(Delivery::Typed)) => {}
            Ok(Ok(Delivery::Clipboard)) => {
                output::notify(
                    self.iface.signal_emitter().connection(),
                    "Copied to clipboard",
                    "Murmur couldn't type into this window. Paste with Ctrl+V.",
                )
                .await;
            }
            Ok(Err(err)) => self.fail(&format!("Couldn't type text: {err:#}")),
            Err(err) => self.fail(&format!("Typing task failed: {err}")),
        }
    }

    fn apply_config(&mut self, cfg: Config) {
        let model_changed = cfg.model != self.cfg.model;
        if cfg.hotkey != self.cfg.hotkey || cfg.assistant_hotkey != self.cfg.assistant_hotkey {
            self.listener.set_keys(Keys::from_config(&cfg));
        }
        self.replacer = Replacer::new(&cfg.replacements);
        self.cfg = cfg;
        if model_changed {
            self.load_model();
        }
    }

    fn load_model(&mut self) {
        self.model_ready = false;
        self.model_gen += 1;
        self.model_state = (State::Loading, "Loading speech model…".into());
        self.engine
            .load(model_by_id(&self.cfg.model), self.model_gen);
    }

    async fn handle(&mut self, ev: Event) {
        match ev {
            Event::HotkeyDown(slot) => self.start_recording(slot, true),
            Event::HotkeyUp(slot) => {
                let rec = self.recording.as_ref().map(|r| (r.slot, r.from_hotkey));
                if key_up_releases(rec, slot) {
                    self.release();
                }
            }
            Event::OtherKey => {
                let rec = self
                    .recording
                    .as_ref()
                    .map(|r| (r.from_hotkey, r.finishing));
                if other_key_cancels(self.cfg.cancel_on_other_key, rec) {
                    self.cancel();
                }
            }
            Event::KeyboardsMissing(msg) => self.keyboard_error = Some(msg),
            Event::KeyboardsOk => self.keyboard_error = None,
            Event::Level(l) => self.level = (l as f64 * 100.0).round() / 100.0,
            Event::Downloading(g, _)
            | Event::Loading(g)
            | Event::ModelReady(g)
            | Event::ModelFailed(g, _)
                if g != self.model_gen =>
            {
                tracing::debug!("Ignoring event from superseded model load {g}");
            }
            Event::Downloading(_, p) => {
                self.model_state = (
                    State::Downloading,
                    format!("Downloading speech model… {:.0}%", p * 100.0),
                );
            }
            Event::Loading(_) => {
                self.model_state = (State::Loading, "Loading speech model…".into());
            }
            Event::ModelReady(_) => self.model_ready = true,
            Event::ModelFailed(_, err) => {
                self.model_ready = false;
                self.model_state = (State::Error, format!("Speech model failed: {err}"));
            }
            Event::Transcribed { seq, result } => {
                self.pending = self.pending.saturating_sub(1);
                let (slot, released) = self.turns.remove(&seq).unwrap_or((Slot::Dictation, None));
                match result {
                    Ok(text) => self.deliver(slot, released, text).await,
                    Err(err) => {
                        self.fail(&format!("Transcription failed: {err}"));
                        self.assistant_ended(slot);
                    }
                }
            }
            Event::FinishRecording(seq) => self.finish(seq),
            Event::MaxLength(seq) => {
                if self
                    .recording
                    .as_ref()
                    .is_some_and(|r| r.seq == seq && !r.finishing)
                {
                    tracing::warn!("Recording hit the {MAX_RECORDING:?} limit");
                    self.release();
                }
            }
            Event::Toggle => {
                if self.recording.is_some() {
                    self.release();
                } else {
                    self.start_recording(Slot::Dictation, false);
                }
            }
            Event::SetEnabled(on) => {
                self.enabled = on;
                if !on {
                    self.cancel();
                }
            }
            Event::ReloadConfig => self.apply_config(Config::load()),
            Event::Retry => {
                if !self.model_ready {
                    self.load_model();
                }
            }
            Event::Restart => {
                tracing::info!("Restart requested");
                self.cancel();
                // Let the D-Bus reply go out first; non-zero so systemd restarts us.
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    std::process::exit(75);
                });
            }
        }
        self.flush().await;
        self.publish().await;
    }
}

async fn run() -> Result<()> {
    let cfg = Config::load();
    if !murmur_common::config_path().exists() {
        let _ = cfg.save();
    }

    let conn = zbus::connection::Builder::session()?
        .build()
        .await
        .context("Could not connect to the session bus")?;

    let (tx, mut rx): (UnboundedSender<Event>, UnboundedReceiver<Event>) = unbounded_channel();
    let status = Arc::new(Mutex::new(Status::default()));
    conn.object_server()
        .at(
            DBUS_PATH,
            Service {
                status: status.clone(),
                events: tx.clone(),
            },
        )
        .await?;
    match conn
        .request_name_with_flags(DBUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            tracing::info!("Murmur is already running");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    }
    let iface = conn
        .object_server()
        .interface::<_, Service>(DBUS_PATH)
        .await?;

    let audio = Audio::new(tx.clone());
    let engine = Engine::spawn(tx.clone());
    let listener = hotkey::Listener::spawn(Keys::from_config(&cfg), tx.clone());

    let mut ctl = Controller {
        replacer: Replacer::new(&cfg.replacements),
        cfg,
        audio,
        engine,
        listener,
        events: tx.clone(),
        iface,
        status,
        published: Status::default(),
        enabled: true,
        model_ready: false,
        model_gen: 0,
        model_state: (State::Starting, "Starting…".into()),
        keyboard_error: None,
        recording: None,
        next_seq: 0,
        pending: 0,
        turns: HashMap::new(),
        outbox: Vec::new(),
        level: 0.0,
        last_error: None,
    };
    ctl.load_model();
    ctl.publish().await;
    tracing::info!(
        "Murmur ready. Hold {} to dictate.",
        hotkey_label(&ctl.cfg.hotkey)
    );
    if let Some(code) = hotkey::parse_assistant_key(&ctl.cfg) {
        tracing::info!(
            "Assistant key: hold {} to send speech to the assistant instead of typing.",
            hotkey_label(&format!("{code:?}"))
        );
    }

    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            Some(ev) = rx.recv() => ctl.handle(ev).await,
            _ = watchdog.tick() => {
                if hotkey::heartbeat_age().is_some_and(|age| age > HEARTBEAT_STALE) {
                    anyhow::bail!("Keyboard listener stopped responding");
                }
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ort=warn".into()),
        )
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    // Any panic in any thread means we're in an unknown state: exit and let
    // the supervisor (systemd or the applet) start a fresh process.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::exit(70);
    }));

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--transcribe") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: murmurd --transcribe <file.wav>");
            std::process::exit(2);
        };
        match engine::transcribe_file(std::path::Path::new(path)) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(err) = rt.block_on(run()) {
        tracing::error!("{err:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_down_is_ignored_while_either_key_is_held() {
        assert_eq!(key_down_action(None), KeyAction::Start);
        assert_eq!(key_down_action(Some(false)), KeyAction::Ignore);
        assert_eq!(key_down_action(Some(true)), KeyAction::FinishThenStart);
    }

    #[test]
    fn key_up_only_releases_its_own_recording() {
        let live = Some((Slot::Assistant, true));
        assert!(key_up_releases(live, Slot::Assistant));
        assert!(!key_up_releases(live, Slot::Dictation));
        assert!(!key_up_releases(
            Some((Slot::Dictation, true)),
            Slot::Assistant
        ));
        // A "Dictate now" recording (not from a key) ignores key releases.
        assert!(!key_up_releases(
            Some((Slot::Dictation, false)),
            Slot::Dictation
        ));
        assert!(!key_up_releases(None, Slot::Dictation));
    }

    #[test]
    fn other_key_cancel_rules() {
        assert!(other_key_cancels(true, Some((true, false))));
        assert!(!other_key_cancels(false, Some((true, false))));
        assert!(!other_key_cancels(true, Some((true, true)))); // already released
        assert!(!other_key_cancels(true, Some((false, false)))); // manual
        assert!(!other_key_cancels(true, None));
    }

    #[test]
    fn signal_choice_by_key() {
        assert_eq!(
            route_text(Slot::Dictation, "hello".into()),
            Some(Route::Type("hello".into()))
        );
        assert_eq!(route_text(Slot::Dictation, String::new()), None);
        assert_eq!(
            route_text(Slot::Assistant, "hello".into()),
            Some(Route::Assistant("hello".into()))
        );
        // Empty assistant text still goes out so the client can reset.
        assert_eq!(
            route_text(Slot::Assistant, String::new()),
            Some(Route::Assistant(String::new()))
        );
    }
}

//! murmurd: hold a key, speak, release, and the words are typed.

mod audio;
mod engine;
mod hotkey;
mod output;

use std::collections::VecDeque;
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
use crate::output::{Delivery, Replacer};

const HISTORY_LEN: usize = 20;
const HEARTBEAT_STALE: Duration = Duration::from_secs(20);
/// Auto-stop a recording after this long (e.g. a forgotten "Dictate now").
const MAX_RECORDING: Duration = Duration::from_secs(300);

/// Everything that can happen, funnelled into one controller loop.
#[derive(Debug)]
pub enum Event {
    HotkeyDown,
    HotkeyUp,
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
}

struct Recording {
    started: Instant,
    released: Option<Instant>,
    seq: u64,
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
    }

    fn start_recording(&mut self, from_hotkey: bool) {
        if !self.enabled {
            return;
        }
        // Pressed again during the release tail: wrap up the previous
        // dictation now so this press gets a recording of its own.
        if let Some(seq) = self
            .recording
            .as_ref()
            .filter(|r| r.finishing)
            .map(|r| r.seq)
        {
            self.finish(seq);
        }
        if self.recording.is_some() {
            return;
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
                    from_hotkey,
                    finishing: false,
                });
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
            return; // accidental tap
        }
        let got = Duration::from_secs_f32(samples.len() as f32 / audio::TARGET_RATE as f32);
        // The recorder should have delivered roughly the whole recording. If it
        // didn't, the microphone stalled: say so instead of transcribing scraps.
        if got < recorded.mul_f32(0.5) {
            tracing::warn!("Microphone delivered {got:?} of {recorded:?}");
            self.fail("Microphone didn't pick anything up. Check your input device.");
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
        self.engine.transcribe(rec.seq, samples);
    }

    fn cancel(&mut self) {
        if self.recording.take().is_some() {
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

    async fn deliver(&mut self, text: String) {
        let text = if self.cfg.remove_fillers {
            output::strip_fillers(&text)
        } else {
            text
        };
        let mut text = self.replacer.apply(&text);
        if text.is_empty() {
            return;
        }
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
        if cfg.hotkey != self.cfg.hotkey {
            self.listener.set_key(hotkey::parse_key(&cfg.hotkey));
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
            Event::HotkeyDown => self.start_recording(true),
            Event::HotkeyUp => {
                if self.recording.as_ref().is_some_and(|r| r.from_hotkey) {
                    self.release();
                }
            }
            Event::OtherKey => {
                if self.cfg.cancel_on_other_key
                    && self
                        .recording
                        .as_ref()
                        .is_some_and(|r| r.from_hotkey && !r.finishing)
                {
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
            Event::Transcribed { seq: _, result } => {
                self.pending = self.pending.saturating_sub(1);
                match result {
                    Ok(text) => self.deliver(text).await,
                    Err(err) => self.fail(&format!("Transcription failed: {err}")),
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
                    self.start_recording(false);
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
    let listener = hotkey::Listener::spawn(hotkey::parse_key(&cfg.hotkey), tx.clone());

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
        level: 0.0,
        last_error: None,
    };
    ctl.load_model();
    ctl.publish().await;
    tracing::info!(
        "Murmur ready. Hold {} to dictate.",
        hotkey_label(&ctl.cfg.hotkey)
    );

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

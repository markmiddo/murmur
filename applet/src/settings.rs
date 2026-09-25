//! Murmur Settings: a regular window for everything in config.toml.

use cosmic::ApplicationExt;
use cosmic::app::{Core, Task};
use cosmic::iced::widget::{column, row};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget::{self, button, container, dropdown, icon, settings, text, text_input};
use cosmic::{Element, theme};
use murmur_common::{Config, MODELS, State};

use crate::dbus::{self, MurmurProxy, Update};

const APP_ICON: &[u8] =
    include_bytes!("../../res/icons/hicolor/scalable/apps/io.github.markmiddo.Murmur.svg");

/// (evdev name, label) for keys that make sense as push-to-talk.
const HOTKEYS: &[(&str, &str)] = &[
    ("KEY_RIGHTALT", "Right Alt"),
    ("KEY_RIGHTCTRL", "Right Ctrl"),
    ("KEY_RIGHTSHIFT", "Right Shift"),
    ("KEY_RIGHTMETA", "Right Super"),
    ("KEY_COMPOSE", "Menu"),
    ("KEY_CAPSLOCK", "Caps Lock"),
    ("KEY_SCROLLLOCK", "Scroll Lock"),
    ("KEY_PAUSE", "Pause"),
    ("KEY_INSERT", "Insert"),
    ("KEY_F13", "F13"),
    ("KEY_F14", "F14"),
    ("KEY_F15", "F15"),
    ("KEY_F16", "F16"),
    ("KEY_F17", "F17"),
    ("KEY_F18", "F18"),
    ("KEY_F19", "F19"),
    ("KEY_F20", "F20"),
];
const TAIL_MS: &[u64] = &[0, 100, 200, 350, 500, 800];
const MIN_HOLD_MS: &[u64] = &[100, 250, 400, 600];

pub struct SettingsApp {
    core: Core,
    cfg: Config,
    replacements: Vec<(String, String)>,
    proxy: Option<MurmurProxy<'static>>,
    state: State,
    detail: String,
    connected: bool,

    hotkey_labels: Vec<String>,
    model_labels: Vec<String>,
    tail_labels: Vec<String>,
    hold_labels: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Engine(Update),
    Hotkey(usize),
    Model(usize),
    Sounds(bool),
    TrailingSpace(bool),
    CancelOnOther(bool),
    RemoveFillers(bool),
    Tail(usize),
    MinHold(usize),
    ReplFrom(usize, String),
    ReplTo(usize, String),
    ReplRemove(usize),
    ReplAdd,
    RestartEngine,
    OpenConfigFile,
    Noop,
}

impl SettingsApp {
    fn save(&mut self) -> Task<Message> {
        self.cfg.replacements = self
            .replacements
            .iter()
            .filter(|(from, _)| !from.trim().is_empty())
            .map(|(from, to)| (from.trim().to_string(), to.clone()))
            .collect();
        if let Err(err) = self.cfg.save() {
            tracing::warn!("could not save config: {err}");
        }
        match self.proxy.clone() {
            Some(p) => cosmic::task::future(async move {
                let _ = p.reload_config().await;
                Message::Noop
            }),
            None => Task::none(),
        }
    }

    fn hotkey_index(&self) -> Option<usize> {
        HOTKEYS.iter().position(|(k, _)| *k == self.cfg.hotkey)
    }

    fn engine_status(&self) -> Element<'_, Message> {
        let (label, color_is_ok) = if !self.connected {
            ("Engine not running".to_string(), false)
        } else {
            (self.detail.clone(), self.state != State::Error)
        };
        let dot = container(widget::Space::new().width(8).height(8)).class(
            theme::Container::custom(move |t: &cosmic::Theme| {
                let c = t.cosmic();
                container::Style {
                    background: Some(cosmic::iced::Background::Color(
                        if color_is_ok {
                            c.success_color()
                        } else {
                            c.warning_color()
                        }
                        .into(),
                    )),
                    border: cosmic::iced::Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }),
        );
        row![dot, text::caption(label)]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
    }
}

impl cosmic::Application for SettingsApp {
    type Executor = cosmic::executor::multi::Executor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "io.github.markmiddo.Murmur.Settings";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, _flags: ()) -> (Self, Task<Message>) {
        core.window.show_context = false;
        let cfg = Config::load();
        let mut replacements: Vec<(String, String)> = cfg
            .replacements
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        if replacements.is_empty() {
            replacements.push((String::new(), String::new()));
        }
        let mut app = Self {
            core,
            cfg,
            replacements,
            proxy: None,
            state: State::Starting,
            detail: String::new(),
            connected: false,
            hotkey_labels: HOTKEYS.iter().map(|(_, l)| l.to_string()).collect(),
            model_labels: MODELS.iter().map(|m| m.name.to_string()).collect(),
            tail_labels: TAIL_MS
                .iter()
                .map(|ms| {
                    if *ms == 0 {
                        "Off".into()
                    } else {
                        format!("{ms} ms")
                    }
                })
                .collect(),
            hold_labels: MIN_HOLD_MS.iter().map(|ms| format!("{ms} ms")).collect(),
        };
        let title = match app.core.main_window_id() {
            Some(id) => app.set_window_title("Murmur Settings".into(), id),
            None => Task::none(),
        };
        (app, title)
    }

    fn subscription(&self) -> Subscription<Message> {
        dbus::subscription().map(Message::Engine)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Engine(u) => match u {
                Update::Connected(p) => {
                    self.proxy = Some(p);
                    self.connected = true;
                }
                Update::Disconnected => {
                    self.proxy = None;
                    self.connected = false;
                }
                Update::State(s) => self.state = State::parse(&s),
                Update::Detail(d) => self.detail = d,
                _ => {}
            },
            Message::Hotkey(i) => {
                self.cfg.hotkey = HOTKEYS[i].0.to_string();
                return self.save();
            }
            Message::Model(i) => {
                self.cfg.model = MODELS[i].id.to_string();
                return self.save();
            }
            Message::Sounds(v) => {
                self.cfg.sounds = v;
                return self.save();
            }
            Message::TrailingSpace(v) => {
                self.cfg.trailing_space = v;
                return self.save();
            }
            Message::CancelOnOther(v) => {
                self.cfg.cancel_on_other_key = v;
                return self.save();
            }
            Message::RemoveFillers(v) => {
                self.cfg.remove_fillers = v;
                return self.save();
            }
            Message::Tail(i) => {
                self.cfg.release_tail_ms = TAIL_MS[i];
                return self.save();
            }
            Message::MinHold(i) => {
                self.cfg.min_hold_ms = MIN_HOLD_MS[i];
                return self.save();
            }
            Message::ReplFrom(i, s) => {
                if let Some(r) = self.replacements.get_mut(i) {
                    r.0 = s;
                }
                return self.save();
            }
            Message::ReplTo(i, s) => {
                if let Some(r) = self.replacements.get_mut(i) {
                    r.1 = s;
                }
                return self.save();
            }
            Message::ReplRemove(i) => {
                if i < self.replacements.len() {
                    self.replacements.remove(i);
                }
                return self.save();
            }
            Message::ReplAdd => {
                self.replacements.push((String::new(), String::new()));
            }
            Message::RestartEngine => {
                if let Some(p) = self.proxy.clone() {
                    return cosmic::task::future(async move {
                        let _ = p.restart().await;
                        Message::Noop
                    });
                }
            }
            Message::OpenConfigFile => {
                let mut cmd = std::process::Command::new("xdg-open");
                cmd.arg(murmur_common::config_path());
                tokio::spawn(cosmic::process::spawn(cmd));
            }
            Message::Noop => {}
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = theme::active().cosmic().spacing;
        let model_idx = MODELS.iter().position(|m| m.id == self.cfg.model);
        let model_desc = MODELS
            .iter()
            .find(|m| m.id == self.cfg.model)
            .map(|m| {
                if m.is_installed() {
                    m.description.to_string()
                } else {
                    format!(
                        "{} Downloads {} MB on first use.",
                        m.description,
                        m.total_bytes() / 1_000_000
                    )
                }
            })
            .unwrap_or_default();

        let header = row![
            icon(icon::from_svg_bytes(APP_ICON)).size(56),
            column![text::title3("Murmur"), self.engine_status()].spacing(4),
        ]
        .spacing(spacing.space_s)
        .align_y(Alignment::Center);

        let dictation = settings::section()
            .title("Dictation")
            .add(
                settings::item::builder("Push-to-talk key")
                    .description("Hold it, speak, then let go.")
                    .control(dropdown(
                        &self.hotkey_labels,
                        self.hotkey_index(),
                        Message::Hotkey,
                    )),
            )
            .add(
                settings::item::builder("Speech model")
                    .description(model_desc)
                    .control(dropdown(&self.model_labels, model_idx, Message::Model)),
            )
            .add(
                settings::item::builder("Sounds")
                    .description("Soft tones when recording starts and stops.")
                    .toggler(self.cfg.sounds, Message::Sounds),
            )
            .add(
                settings::item::builder("Add a space after each dictation")
                    .description("So back-to-back dictations join into one sentence.")
                    .toggler(self.cfg.trailing_space, Message::TrailingSpace),
            )
            .add(
                settings::item::builder("Remove filler sounds")
                    .description("Drops \"um\", \"uh\" and \"mm-hmm\" from what gets typed.")
                    .toggler(self.cfg.remove_fillers, Message::RemoveFillers),
            )
            .add(
                settings::item::builder("Cancel when another key is pressed")
                    .description("Keeps the key usable as a normal modifier, like AltGr.")
                    .toggler(self.cfg.cancel_on_other_key, Message::CancelOnOther),
            );

        let timing = settings::section()
            .title("Timing")
            .add(
                settings::item::builder("Keep listening after release")
                    .description("Catches the end of your last word.")
                    .control(dropdown(
                        &self.tail_labels,
                        TAIL_MS.iter().position(|v| *v == self.cfg.release_tail_ms),
                        Message::Tail,
                    )),
            )
            .add(
                settings::item::builder("Ignore taps shorter than")
                    .description("Stops accidental key presses from recording.")
                    .control(dropdown(
                        &self.hold_labels,
                        MIN_HOLD_MS.iter().position(|v| *v == self.cfg.min_hold_ms),
                        Message::MinHold,
                    )),
            );

        // Word replacements editor.
        let mut repl = column![
            row![
                text::caption_heading("When Murmur hears").width(Length::FillPortion(1)),
                text::caption_heading("It types").width(Length::FillPortion(1)),
                widget::Space::new().width(36),
            ]
            .spacing(spacing.space_xs)
        ]
        .spacing(spacing.space_xxs);
        for (i, (from, to)) in self.replacements.iter().enumerate() {
            repl = repl.push(
                row![
                    text_input("e.g. java script", from.as_str())
                        .on_input(move |s| Message::ReplFrom(i, s))
                        .width(Length::FillPortion(1)),
                    text_input("e.g. JavaScript", to.as_str())
                        .on_input(move |s| Message::ReplTo(i, s))
                        .width(Length::FillPortion(1)),
                    button::icon(icon::from_name("edit-delete-symbolic"))
                        .tooltip("Remove")
                        .on_press(Message::ReplRemove(i)),
                ]
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center),
            );
        }
        repl = repl.push(
            container(
                button::standard("Add replacement")
                    .leading_icon(icon::from_name("list-add-symbolic"))
                    .on_press(Message::ReplAdd),
            )
            .padding([spacing.space_xxs, 0]),
        );
        let replacements = settings::section()
            .title("Word replacements")
            .add(
                container(
                    column![
                        text::caption(
                            "Fix names and jargon the model mishears. Matching ignores capitals and only replaces whole words."
                        ),
                        repl
                    ]
                    .spacing(spacing.space_s),
                )
                .padding([spacing.space_xs, spacing.space_s]),
            );

        let engine = settings::section()
            .title("Engine")
            .add(
                settings::item::builder("Restart engine")
                    .description("Reloads the speech model and reconnects keyboards.")
                    .control(button::standard("Restart").on_press(Message::RestartEngine)),
            )
            .add(
                settings::item::builder("Advanced")
                    .description("Every option lives in a plain text file.")
                    .control(button::text("Open config file").on_press(Message::OpenConfigFile)),
            );

        let content = column![header, dictation, timing, replacements, engine]
            .spacing(spacing.space_l)
            .padding([spacing.space_l, spacing.space_l])
            .max_width(760);

        widget::scrollable(container(content).center_x(Length::Fill)).into()
    }
}

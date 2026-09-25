//! The panel button and its popup.

use std::time::{Duration, Instant};

use cosmic::app::{Core, Task};
use cosmic::applet::{menu_button, padded_control};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::widget::{column, row};
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Background, Border, Color, Length, Subscription};
use cosmic::widget::{self, container, divider, icon, text, toggler};
use cosmic::{Element, surface, theme};
use murmur_common::{APP_ID, Config, State};

use crate::dbus::{self, MurmurProxy, Update};

const MIC: &[u8] = include_bytes!("../icons/mic-symbolic.svg");
const MIC_OFF: &[u8] = include_bytes!("../icons/mic-off-symbolic.svg");
const MIC_ERROR: &[u8] = include_bytes!("../icons/mic-error-symbolic.svg");
const APP_ICON: &[u8] =
    include_bytes!("../../res/icons/hicolor/scalable/apps/io.github.markmiddo.Murmur.svg");

/// Relaunch the engine if it has been missing this long.
const ENGINE_GRACE: Duration = Duration::from_secs(4);
const HISTORY_SHOWN: usize = 5;

pub struct Window {
    core: Core,
    popup: Option<Id>,
    proxy: Option<MurmurProxy<'static>>,
    connected: bool,
    disconnected_since: Option<Instant>,
    last_spawn: Option<Instant>,

    state: State,
    detail: String,
    level: f32,
    shown_level: f32,
    enabled: bool,
    model: String,
    hotkey: String,
    history: Vec<(i64, String)>,
    sounds: bool,
    copied: Option<usize>,

    started: Instant,
    phase: f32,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            core: Core::default(),
            popup: None,
            proxy: None,
            connected: false,
            disconnected_since: Some(Instant::now()),
            last_spawn: None,
            state: State::Starting,
            detail: "Starting…".into(),
            level: 0.0,
            shown_level: 0.0,
            enabled: true,
            model: String::new(),
            hotkey: "Right Alt".into(),
            history: Vec::new(),
            sounds: Config::load().sounds,
            copied: None,
            started: Instant::now(),
            phase: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    Surface(surface::Action<Message>),
    Engine(Update),
    Tick,
    SetEnabled(bool),
    SetSounds(bool),
    DictateNow,
    StartDictation,
    Copy(usize),
    OpenSettings,
    Retry,
    Noop,
}

impl Window {
    fn animating(&self) -> bool {
        self.connected && matches!(self.state, State::Recording | State::Transcribing)
    }

    fn call<F, Fut>(&self, f: F) -> Task<Message>
    where
        F: FnOnce(MurmurProxy<'static>) -> Fut,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send + 'static,
    {
        match self.proxy.clone() {
            Some(p) => {
                let fut = f(p);
                cosmic::task::future(async move {
                    if let Err(err) = fut.await {
                        tracing::warn!("engine call failed: {err}");
                    }
                    Message::Noop
                })
            }
            None => Task::none(),
        }
    }

    fn edit_config(&self, edit: impl FnOnce(&mut Config)) -> Task<Message> {
        if let Err(err) = Config::update(edit) {
            tracing::warn!("could not save config: {err}");
            return Task::none();
        }
        self.call(|p| async move { p.reload_config().await })
    }

    /// Start murmurd if nothing owns the bus name. The engine refuses to run
    /// twice, so a spare launch is harmless.
    fn supervise_engine(&mut self) {
        let Some(since) = self.disconnected_since else {
            return;
        };
        if since.elapsed() < ENGINE_GRACE
            || self
                .last_spawn
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.last_spawn = Some(Instant::now());
        let exe = crate::sibling_exe("murmurd");
        tracing::info!("Starting engine: {}", exe.display());
        let mut cmd = std::process::Command::new(exe);
        cmd.stdin(std::process::Stdio::null());
        tokio::spawn(cosmic::process::spawn(cmd));
    }

    fn status_color(&self, theme: &cosmic::Theme) -> Color {
        let c = theme.cosmic();
        let srgba = match self.state {
            State::Recording => c.destructive_color(),
            State::Error => c.warning_color(),
            State::Idle if self.connected => c.success_color(),
            _ => c.accent_color(),
        };
        srgba.into()
    }

    /// Heights (0..1) for `n` level bars at the current animation phase.
    fn bar_heights(&self, n: usize) -> Vec<f32> {
        let t = self.phase;
        let mid = (n as f32 - 1.0) / 2.0;
        (0..n)
            .map(|i| {
                let x = i as f32;
                let shape = 1.0 - ((x - mid).abs() / (mid + 1.0)).powf(1.6) * 0.7;
                match self.state {
                    State::Recording => {
                        let wobble = 0.75 + 0.25 * (t * 9.0 + x * 1.7).sin();
                        (0.18 + 0.82 * self.shown_level * shape * wobble).clamp(0.12, 1.0)
                    }
                    State::Transcribing => {
                        0.22 + 0.5 * (0.5 + 0.5 * (t * 6.0 - x * 0.8).sin()) * shape
                    }
                    _ => 0.12,
                }
            })
            .collect()
    }

    fn bars<'a>(
        &self,
        n: usize,
        width: f32,
        gap: f32,
        height: f32,
        dim: bool,
    ) -> Element<'a, Message> {
        let color_state = self.state;
        let connected = self.connected;
        let mut r = cosmic::iced::widget::Row::new()
            .spacing(gap)
            .align_y(Alignment::Center);
        for h in self.bar_heights(n) {
            let bar = container(
                widget::Space::new()
                    .width(width)
                    .height((height * h).max(width)),
            )
            .class(theme::Container::custom(move |theme: &cosmic::Theme| {
                let c = theme.cosmic();
                let mut color: Color = match color_state {
                    State::Recording => c.destructive_color(),
                    _ if !connected => c.palette.neutral_6,
                    _ => c.accent_color(),
                }
                .into();
                if dim {
                    color.a = 0.35;
                }
                container::Style {
                    background: Some(Background::Color(color)),
                    border: Border {
                        radius: (width / 2.0).into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }));
            r = r.push(bar);
        }
        container(r).height(height).center_y(height).into()
    }

    fn panel_icon(&self) -> &'static [u8] {
        if !self.connected || self.state == State::Error {
            MIC_ERROR
        } else if self.state == State::Disabled {
            MIC_OFF
        } else {
            MIC
        }
    }

    fn popup_view(&self) -> Element<'_, Message> {
        let spacing = theme::active().cosmic().spacing;

        // Header: app icon, name + status, enable switch.
        let status_dot =
            container(widget::Space::new().width(8).height(8)).class(theme::Container::custom({
                let color = |t: &cosmic::Theme| self.status_color(t);
                let c = color(&theme::active());
                move |_t: &cosmic::Theme| container::Style {
                    background: Some(Background::Color(c)),
                    border: Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }));
        let header = row![
            icon(icon::from_svg_bytes(APP_ICON)).size(40),
            column![
                text::title4("Murmur"),
                row![status_dot, text::caption(self.detail.clone())]
                    .spacing(6)
                    .align_y(Alignment::Center),
            ]
            .spacing(2)
            .width(Length::Fill),
            toggler(self.enabled).on_toggle_maybe(self.connected.then_some(Message::SetEnabled)),
        ]
        .spacing(spacing.space_s)
        .align_y(Alignment::Center);

        // Big live meter.
        let active = self.animating();
        let meter = container(self.bars(21, 5.0, 4.0, 44.0, !active))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .padding([spacing.space_xs, 0]);

        let mut content = column![
            padded_control(header),
            padded_control(meter),
            padded_control(divider::horizontal::default())
                .padding([spacing.space_xxs, spacing.space_s]),
            padded_control(
                row![
                    text::caption_heading("Recent").width(Length::Fill),
                    text::caption(if self.history.is_empty() {
                        ""
                    } else {
                        "Click to copy"
                    }),
                ]
                .align_y(Alignment::Center)
            ),
        ]
        .padding([8, 0]);

        if self.history.is_empty() {
            content = content.push(padded_control(text::caption(format!(
                "Hold {}, speak, and let go. Your words land wherever you're typing.",
                self.hotkey
            ))));
        } else {
            let now = chrono::Local::now().timestamp();
            for (i, (ts, line)) in self.history.iter().take(HISTORY_SHOWN).enumerate() {
                let copied = self.copied == Some(i);
                let right = if copied {
                    "Copied".to_string()
                } else {
                    ago(now - ts)
                };
                content = content.push(
                    menu_button(
                        row![
                            text::body(ellipsize(line, 40)).width(Length::Fill),
                            text::caption(right),
                            icon::from_name(if copied {
                                "object-select-symbolic"
                            } else {
                                "edit-copy-symbolic"
                            })
                            .size(16)
                            .icon(),
                        ]
                        .spacing(spacing.space_xs)
                        .align_y(Alignment::Center),
                    )
                    .on_press(Message::Copy(i)),
                );
            }
        }

        content = content
            .push(
                padded_control(divider::horizontal::default())
                    .padding([spacing.space_xxs, spacing.space_s]),
            )
            .push(padded_control(
                toggler(self.sounds)
                    .on_toggle(Message::SetSounds)
                    .label("Sounds".to_string())
                    .text_size(14)
                    .width(Length::Fill),
            ))
            .push(
                padded_control(divider::horizontal::default())
                    .padding([spacing.space_xxs, spacing.space_s]),
            );

        if !self.connected {
            content =
                content.push(menu_button(text::body("Start engine")).on_press(Message::Retry));
        } else if self.state == State::Error {
            content = content.push(menu_button(text::body("Try again")).on_press(Message::Retry));
        } else {
            content = content.push(
                menu_button(
                    row![
                        text::body("Dictate now").width(Length::Fill),
                        text::caption(format!("or hold {}", self.hotkey)),
                    ]
                    .align_y(Alignment::Center),
                )
                .on_press_maybe((self.state == State::Idle).then_some(Message::DictateNow)),
            );
        }
        content =
            content.push(menu_button(text::body("Settings…")).on_press(Message::OpenSettings));

        self.core.applet.popup_container(content).into()
    }
}

impl cosmic::Application for Window {
    type Executor = cosmic::executor::multi::Executor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (
            Self {
                core,
                ..Default::default()
            },
            Task::none(),
        )
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![dbus::subscription().map(Message::Engine)];
        if self.animating() {
            subs.push(cosmic::iced::time::every(Duration::from_millis(33)).map(|_| Message::Tick));
        } else if !self.connected {
            subs.push(cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TogglePopup => {
                // While dictating, a click on the panel stops it.
                if self.state == State::Recording {
                    return self.call(|p| async move { p.toggle_recording().await });
                }
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p);
                }
                self.copied = None;
                self.sounds = Config::load().sounds;
                return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_| Default::default(),
                    |app: &mut Self| {
                        let id = Id::unique();
                        app.popup = Some(id);
                        app.core.applet.get_popup_settings(
                            app.core.main_window_id().unwrap(),
                            id,
                            Some((380, 1)),
                            None,
                            None,
                        )
                    },
                    None,
                ));
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::Surface(a) => return cosmic::task::message(cosmic::Action::Surface(a)),
            Message::Engine(update) => match update {
                Update::Connected(p) => {
                    self.proxy = Some(p);
                    self.connected = true;
                    self.disconnected_since = None;
                }
                Update::Disconnected => {
                    if self.connected || self.disconnected_since.is_none() {
                        self.disconnected_since = Some(Instant::now());
                    }
                    self.connected = false;
                    self.proxy = None;
                    self.state = State::Error;
                    self.detail = "Engine not running".into();
                    self.supervise_engine();
                }
                Update::State(s) => {
                    self.state = State::parse(&s);
                    if self.state != State::Recording {
                        self.level = 0.0;
                    }
                }
                Update::Detail(d) => self.detail = d,
                Update::Level(l) => self.level = l as f32,
                Update::Enabled(e) => self.enabled = e,
                Update::Model(m) => self.model = m,
                Update::Hotkey(h) => self.hotkey = h,
                Update::History(h) => {
                    self.history = h;
                    self.copied = None;
                }
            },
            Message::Tick => {
                self.phase = self.started.elapsed().as_secs_f32();
                // Ease the displayed level so bars glide instead of jumping.
                let k = if self.level > self.shown_level {
                    0.55
                } else {
                    0.18
                };
                self.shown_level += (self.level - self.shown_level) * k;
                if !self.connected {
                    self.supervise_engine();
                }
            }
            Message::SetEnabled(on) => {
                self.enabled = on;
                return self.call(move |p| async move { p.set_enabled(on).await });
            }
            Message::SetSounds(on) => {
                self.sounds = on;
                return self.edit_config(|c| c.sounds = on);
            }
            Message::DictateNow => {
                // Close the popup first so the previously focused window gets the text.
                let close = self
                    .popup
                    .take()
                    .map(destroy_popup)
                    .unwrap_or_else(Task::none);
                let start = cosmic::task::future(async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Message::StartDictation
                });
                return close.chain(start);
            }
            Message::StartDictation => {
                return self.call(|p| async move { p.toggle_recording().await });
            }
            Message::Copy(i) => {
                if let Some((_, line)) = self.history.get(i) {
                    let line = line.clone();
                    self.copied = Some(i);
                    std::thread::spawn(move || {
                        use std::io::Write;
                        if let Ok(mut child) = std::process::Command::new("wl-copy")
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                        {
                            if let Some(mut stdin) = child.stdin.take() {
                                let _ = stdin.write_all(line.as_bytes());
                            }
                            let _ = child.wait();
                        }
                    });
                }
            }
            Message::OpenSettings => {
                let cmd = std::process::Command::new(crate::sibling_exe("murmur-settings"));
                tokio::spawn(cosmic::process::spawn(cmd));
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p);
                }
            }
            Message::Retry => {
                if self.connected {
                    return self.call(|p| async move { p.retry().await });
                }
                self.last_spawn = None;
                self.disconnected_since = Some(Instant::now() - ENGINE_GRACE);
                self.supervise_engine();
            }
            Message::Noop => {}
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let button = if self.animating() {
            let (_, h) = self.core.applet.suggested_size(true);
            let h = h as f32;
            let bars = self.bars(5, (h / 7.0).max(2.0), (h / 10.0).max(1.5), h, false);
            self.core.applet.button_from_element(bars, true)
        } else {
            self.core
                .applet
                .icon_button_from_handle(icon::from_svg_bytes(self.panel_icon()).symbolic(true))
        };
        let tooltip = format!("Murmur — {}", self.detail);
        self.core
            .applet
            .applet_tooltip::<Message>(
                button.on_press_down(Message::TogglePopup),
                tooltip,
                self.popup.is_some(),
                Message::Surface,
                None,
            )
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        self.popup_view()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

fn ago(secs: i64) -> String {
    match secs {
        s if s < 45 => "now".into(),
        s if s < 3600 => format!("{}m", (s + 30) / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

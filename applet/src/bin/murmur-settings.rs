fn main() -> cosmic::iced::Result {
    murmur_applet::logging::init("settings");
    let settings = cosmic::app::Settings::default()
        .size(cosmic::iced::Size::new(720.0, 820.0))
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(480.0)
                .min_height(420.0),
        );
    cosmic::app::run::<murmur_applet::settings::SettingsApp>(settings, ())
}

fn main() -> cosmic::iced::Result {
    murmur_applet::logging::init("applet");
    cosmic::applet::run::<murmur_applet::window::Window>(())
}

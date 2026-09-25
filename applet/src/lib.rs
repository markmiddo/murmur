pub mod dbus;
pub mod logging;
pub mod settings;
pub mod window;

/// Path to a sibling binary installed next to the current one, falling back to $PATH.
pub fn sibling_exe(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| name.into())
}

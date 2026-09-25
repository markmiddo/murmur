/// The panel discards applet stderr, so log to ~/.cache/murmur/<name>.log too.
pub fn init(name: &str) {
    let log_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("murmur");
    let _ = std::fs::create_dir_all(&log_dir);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn,murmur_applet=info".into());
    match std::fs::File::create(log_dir.join(format!("{name}.log"))) {
        Ok(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

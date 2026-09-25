//! Live connection to the murmurd engine over the session bus.

use std::time::Duration;

use cosmic::iced::{Subscription, stream};
use futures::{SinkExt, StreamExt, channel::mpsc::Sender, stream::BoxStream};
use murmur_common::{DBUS_NAME, DBUS_PATH};

#[zbus::proxy(
    interface = "io.github.markmiddo.Murmur1",
    default_service = "io.github.markmiddo.Murmur",
    default_path = "/io/github/markmiddo/Murmur"
)]
pub trait Murmur {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn detail(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn level(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn model(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn hotkey(&self) -> zbus::Result<String>;

    fn history(&self) -> zbus::Result<Vec<(i64, String)>>;
    fn toggle_recording(&self) -> zbus::Result<()>;
    fn set_enabled(&self, enabled: bool) -> zbus::Result<()>;
    fn reload_config(&self) -> zbus::Result<()>;
    fn retry(&self) -> zbus::Result<()>;
    fn restart(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn transcribed(&self, text: String) -> zbus::Result<()>;
}

#[derive(Debug, Clone)]
pub enum Update {
    Connected(MurmurProxy<'static>),
    Disconnected,
    State(String),
    Detail(String),
    Level(f64),
    Enabled(bool),
    Model(String),
    Hotkey(String),
    History(Vec<(i64, String)>),
}

pub fn subscription() -> Subscription<Update> {
    Subscription::run_with("murmur-dbus", |_| {
        stream::channel(64, |mut out: Sender<Update>| async move {
            loop {
                if let Err(err) = watch(&mut out).await {
                    tracing::warn!("engine connection: {err}");
                }
                let _ = out.send(Update::Disconnected).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    })
}

/// Follow the engine until it goes away.
async fn watch(out: &mut Sender<Update>) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;

    // Wait for the engine to appear.
    let name = zbus::names::BusName::try_from(DBUS_NAME)?;
    while !dbus.name_has_owner(name.clone()).await? {
        let _ = out.send(Update::Disconnected).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
    }

    let proxy = MurmurProxy::builder(&conn).path(DBUS_PATH)?.build().await?;
    tracing::info!("connected to engine");
    let _ = out.send(Update::Connected(proxy.clone())).await;
    out.send(Update::State(proxy.state().await?)).await.ok();
    out.send(Update::Detail(proxy.detail().await?)).await.ok();
    out.send(Update::Enabled(proxy.enabled().await?)).await.ok();
    out.send(Update::Model(proxy.model().await?)).await.ok();
    out.send(Update::Hotkey(proxy.hotkey().await?)).await.ok();
    out.send(Update::History(proxy.history().await?)).await.ok();

    let p = proxy.clone();
    let streams: Vec<BoxStream<'static, Update>> = vec![
        proxy
            .receive_state_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::State) })
            .boxed(),
        proxy
            .receive_detail_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::Detail) })
            .boxed(),
        proxy
            .receive_level_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::Level) })
            .boxed(),
        proxy
            .receive_enabled_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::Enabled) })
            .boxed(),
        proxy
            .receive_model_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::Model) })
            .boxed(),
        proxy
            .receive_hotkey_changed()
            .await
            .filter_map(|c| async move { c.get().await.ok().map(Update::Hotkey) })
            .boxed(),
        proxy
            .receive_transcribed()
            .await?
            .filter_map(move |_| {
                let p = p.clone();
                async move { p.history().await.ok().map(Update::History) }
            })
            .boxed(),
    ];
    let mut updates = futures::stream::select_all(streams);

    loop {
        tokio::select! {
            Some(update) = updates.next() => {
                if out.send(update).await.is_err() {
                    return Ok(());
                }
            }
            Some(change) = owner_changes.next() => {
                if let Ok(args) = change.args()
                    && args.name() == &name
                    && args.new_owner().is_none()
                {
                    return Ok(());
                }
            }
            else => return Ok(()),
        }
    }
}

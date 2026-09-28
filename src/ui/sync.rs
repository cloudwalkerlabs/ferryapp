//! The one subscription that watches the in-process core: a snapshot, then
//! the core's events, and a fresh snapshot whenever the receiver lags.

use std::hash::{Hash, Hasher};

use iced::{
    Subscription,
    futures::{SinkExt, Stream},
};
use tokio::sync::broadcast::error::RecvError;

use crate::{
    core::{Core, CoreEvent},
    ui::store::Snapshot,
};

/// What the core reports to the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The whole state, sent first and again after a lag.
    Snapshot(Box<Snapshot>),
    /// One change since the last snapshot.
    Event(Box<CoreEvent>),
}

/// Watch `core`. Subscribing before taking the snapshot means no change is
/// missed; events already reflected in the snapshot are replayed after it.
pub fn watch(core: &Core) -> Subscription<Update> {
    Subscription::run_with(Watched(core.clone()), stream)
}

/// The core, as the key of the subscription that watches it.
struct Watched(Core);

impl Hash for Watched {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.local_device_id().hash(state);
    }
}

fn stream(watched: &Watched) -> impl Stream<Item = Update> + use<> {
    let core = watched.0.clone();
    iced::stream::channel(64, async move |mut output| {
        loop {
            let mut events = core.subscribe();
            let snapshot = Update::Snapshot(Box::new(Snapshot::take(&core)));
            if output.send(snapshot).await.is_err() {
                return;
            }
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if output.send(Update::Event(Box::new(event))).await.is_err() {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => return,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use iced::futures::{StreamExt, executor::block_on};

    use super::*;
    use crate::core::{
        EventData, SettingsPatch,
        testing::{handle, make_identity},
    };

    async fn rename(core: &Core, name: &str) {
        core.update_settings(SettingsPatch {
            device_name: Some(Some(name.into())),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sends_a_snapshot_then_events_and_a_fresh_snapshot_after_a_lag() {
        // The test core's event bus holds one event.
        let (core, _commands) = handle().await;
        let mut updates = Box::pin(stream(&Watched(core.clone())));

        let Some(Update::Snapshot(first)) = block_on(updates.next()) else {
            panic!("expected a snapshot first");
        };
        assert_eq!(first.devices, Ok(vec![]));

        rename(&core, "Renamed").await;
        let Some(Update::Event(event)) = block_on(updates.next()) else {
            panic!("expected an event");
        };
        assert!(matches!(
            &event.event,
            EventData::SettingsChanged(settings) if settings.device_name == "Renamed"
        ));

        // Two changes overflow the bus: the stream starts over.
        core.discover_device(&make_identity(&"a".repeat(32), vec![]), true, 1)
            .unwrap();
        rename(&core, "Renamed again").await;
        let Some(Update::Snapshot(fresh)) = block_on(updates.next()) else {
            panic!("expected a fresh snapshot");
        };
        assert_eq!(fresh.devices.unwrap().len(), 1);
        assert_eq!(fresh.settings.unwrap().device_name, "Renamed again");
    }
}

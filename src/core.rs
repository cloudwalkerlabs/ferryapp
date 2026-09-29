//! The core of the daemon: devices, connections, pairing, transfers,
//! settings and events, and the plugin API that features are built on.
//! It knows no feature by name; the composition root ([`crate::daemon`])
//! decides which plugins run.
//!
//! [`Core`] is the handle to all of it. Its methods are split by what they
//! act on: `devices` (the registry and what clients see of a device),
//! `connections` (live control channels, packet dispatch, and the LAN
//! transport's command channel), `pairing` (the pairing state machine and
//! trust), `transfers` and `settings`. Every part of the state that
//! must change together (devices, connections, pairings) sits behind one
//! lock; transfers, settings and each plugin's state have their own.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;

use crate::{config::LocalIdentity, store::Store};

mod connections;
mod devices;
mod error;
mod events;
mod pairing;
mod payload;
mod plugin;
mod settings;
#[cfg(test)]
pub(crate) mod testing;
mod transfers;

use connections::Connection;
use devices::paired_devices;
use pairing::PairingRuntime;
pub(crate) use settings::{Settings, StoredSettings};

pub use connections::LanCommand;
pub use devices::{DeviceReachability, DeviceRegistry, DeviceRegistryError, DeviceSnapshot};
pub use error::{CoreError, OperationErrorCode};
pub use events::{CoreEvent, EventBus, EventBusError, EventData};
pub use pairing::{
    Pairing, PairingDirection, PairingSnapshot, PairingStatus, PairingTransitionError,
};
pub use payload::{AcceptedPayload, DialedPayload, PayloadListener, PayloadPeer, SshAuthError};
pub use plugin::{
    Capabilities, Plugin, PluginContext, PluginEvent, PluginEventKind, PluginRegistry,
};
pub use settings::{
    APPEARANCE, Appearance, CLOSE_TO_TRAY, DEVICE_NAME, DOWNLOAD_DIR, LANGUAGE, SettingsDefaults,
    SettingsPatch, SettingsSnapshot,
};
pub use transfers::{
    DEFAULT_MAX_TRANSFER_BYTES, FileNameError, PROGRESS_EVENT_INTERVAL, Transfer, TransferConfig,
    TransferDirection, TransferHandle, TransferProgressError, TransferSnapshot, TransferStatus,
    TransferTransitionError, Transfers, forward_reader, sanitize_file_name, upload_channel,
};

/// The core: devices, connections, pairing, transfers, settings and events,
/// and the plugins built on them. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    started_at: Instant,
    local_device_id: Arc<str>,
    /// Locked before, never while holding, `state`.
    settings: Arc<Mutex<Settings>>,
    settings_updates: Arc<tokio::sync::Mutex<()>>,
    mutations: tokio_util::task::TaskTracker,
    device_operations: Arc<Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
    /// The device name in effect, watched by the LAN transport so a rename
    /// reaches peers.
    local_device_name: Arc<watch::Sender<String>>,
    protocol_version: u8,
    local_public_key_der: Arc<Vec<u8>>,
    store: Store,
    identity: Arc<LocalIdentity>,
    state: Arc<RwLock<CoreState>>,
    commands: mpsc::Sender<LanCommand>,
    events: EventBus,
    transfers: Transfers,
    plugins: Arc<PluginRegistry>,
}

/// What must change together, behind [`Core`]'s one lock. It is never held
/// while calling into a plugin or publishing an event.
struct CoreState {
    devices: DeviceRegistry,
    connections: HashMap<String, Connection>,
    pairings: BTreeMap<Uuid, PairingRuntime>,
    pairing_by_device: HashMap<String, Uuid>,
}

impl Core {
    /// A core running `plugins`: in the daemon, `plugins::builtin()`, chosen
    /// by [`crate::daemon`].
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        local_device: LocalDeviceSnapshot,
        protocol_version: u8,
        local_public_key_der: Vec<u8>,
        store: Store,
        plugins: Vec<Arc<dyn Plugin>>,
        command_capacity: usize,
        event_capacity: usize,
        identity: Arc<LocalIdentity>,
        transfer_config: TransferConfig,
    ) -> Result<(Self, mpsc::Receiver<LanCommand>), CoreError> {
        if command_capacity == 0 {
            return Err(CoreError::InvalidCommandCapacity);
        }
        let (commands, receiver) = mpsc::channel(command_capacity);
        let events = EventBus::new(event_capacity)?;
        let devices = paired_devices(&store).await;
        let plugins = PluginRegistry::new(plugins);
        let settings = Settings::new(SettingsDefaults {
            device_name: local_device.device_name.clone(),
            download_dir: transfer_config.download_dir.clone(),
        });
        let settings = Arc::new(Mutex::new(settings));
        let transfers = Transfers::new(transfer_config, events.clone(), settings.clone());
        Ok((
            Self {
                started_at: Instant::now(),
                local_device_id: local_device.device_id.into(),
                settings,
                settings_updates: Arc::new(tokio::sync::Mutex::new(())),
                mutations: tokio_util::task::TaskTracker::new(),
                device_operations: Arc::new(Mutex::new(HashMap::new())),
                local_device_name: Arc::new(watch::Sender::new(local_device.device_name)),
                protocol_version,
                local_public_key_der: Arc::new(local_public_key_der),
                store,
                identity,
                state: Arc::new(RwLock::new(CoreState {
                    devices,
                    connections: HashMap::new(),
                    pairings: BTreeMap::new(),
                    pairing_by_device: HashMap::new(),
                })),
                commands,
                events,
                transfers,
                plugins: Arc::new(plugins),
            },
            receiver,
        ))
    }

    fn device_operation(&self, device_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut operations = self
            .device_operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operations.retain(|_, operation| operation.strong_count() > 0);
        if let Some(operation) = operations.get(device_id).and_then(std::sync::Weak::upgrade) {
            return operation;
        }
        let operation = Arc::new(tokio::sync::Mutex::new(()));
        operations.insert(device_id.to_owned(), Arc::downgrade(&operation));
        operation
    }

    /// The core as plugins see it.
    pub fn plugin_context(&self) -> PluginContext {
        PluginContext::new(self.clone())
    }

    /// The packet types this core's plugins receive and send, for the LAN
    /// transport to advertise.
    pub fn capabilities(&self) -> Capabilities {
        self.plugins.capabilities()
    }

    pub fn event_bus(&self) -> &EventBus {
        &self.events
    }

    /// The daemon's data: typed configs and paired devices.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Events from now on, for `/events` and other clients.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    /// Every plugin's HTTP routes, merged, for the API server.
    pub(crate) fn plugin_routes(&self) -> axum::Router {
        self.plugins.routes(&self.plugin_context())
    }

    /// Every plugin's streaming (upload) routes, merged, for the API server.
    pub(crate) fn plugin_streaming_routes(&self) -> axum::Router {
        self.plugins.streaming_routes(&self.plugin_context())
    }

    /// Every file transfer, whichever feature started it.
    pub fn transfers(&self) -> &Transfers {
        &self.transfers
    }

    /// Request cancellation of an active transfer, whichever feature
    /// started it. Returns the transfer's current snapshot; its task tears
    /// down its socket and partial file and marks it `cancelled` once it
    /// notices.
    pub fn cancel_transfer(&self, transfer_id: Uuid) -> Result<TransferSnapshot, CoreError> {
        self.transfers.cancel(transfer_id)
    }

    /// Cancel every in-progress transfer task and wait up to `deadline` for
    /// each to observe cancellation and finish tearing down its own socket
    /// and temporary file, aborting any that do not in time. Called during
    /// daemon shutdown so no transfer task or socket outlives the process.
    pub async fn shutdown_transfers(&self, deadline: Duration) {
        self.transfers.shutdown(deadline).await;
    }

    pub fn local_device_id(&self) -> &str {
        &self.local_device_id
    }

    /// The name this device currently advertises to peers.
    pub fn local_device_name(&self) -> String {
        self.local_device_name.borrow().clone()
    }

    /// Watch the advertised device name, which changes when the user
    /// renames this device.
    pub fn watch_local_device_name(&self) -> watch::Receiver<String> {
        self.local_device_name.subscribe()
    }

    /// Replace the settings this handle was created with, e.g. with ones
    /// backed by the settings file, and apply their runtime effects.
    pub(crate) fn install_settings(&self, settings: Settings) {
        let Ok(mut current) = self.settings.lock() else {
            return;
        };
        *current = settings;
        self.apply_settings(&current.snapshot());
    }

    pub fn settings(&self) -> Result<SettingsSnapshot, CoreError> {
        Ok(self
            .settings
            .lock()
            .map_err(|_| CoreError::StateUnavailable)?
            .snapshot())
    }

    /// Validate, persist, and apply a settings change, then publish
    /// `settings.changed` if anything changed. A new device name is
    /// re-announced to the network by the LAN transport.
    pub async fn update_settings(
        &self,
        patch: SettingsPatch,
    ) -> Result<SettingsSnapshot, CoreError> {
        let core = self.clone();
        self.mutations
            .spawn(async move { core.update_settings_inner(patch).await })
            .await
            .map_err(|_| CoreError::StateUnavailable)?
    }

    async fn update_settings_inner(
        &self,
        patch: SettingsPatch,
    ) -> Result<SettingsSnapshot, CoreError> {
        let _update = self.settings_updates.lock().await;
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| CoreError::StateUnavailable)?
            .clone();
        let before = settings.snapshot();
        let after = settings.update(patch).await?;
        *self
            .settings
            .lock()
            .map_err(|_| CoreError::StateUnavailable)? = settings;
        if after != before {
            self.apply_settings(&after);
            self.events
                .publish(EventData::SettingsChanged(after.clone()))?;
        }
        Ok(after)
    }

    /// Push the settings that live outside [`Settings`] to where they take
    /// effect. The download directory is read when each transfer starts.
    fn apply_settings(&self, settings: &SettingsSnapshot) {
        self.local_device_name.send_if_modified(|name| {
            let changed = *name != settings.device_name;
            if changed {
                name.clone_from(&settings.device_name);
            }
            changed
        });
    }

    /// Start what plugins run of their own. Called once by the code that
    /// starts the daemon, inside the async runtime.
    pub async fn start_plugins(&self) {
        self.plugins.started(&self.plugin_context()).await;
    }

    /// Stop every plugin's own work and close what plugins hold open.
    /// Called during daemon shutdown, after [`Self::shutdown_transfers`].
    pub async fn shutdown_plugins(&self) {
        self.mutations.close();
        self.mutations.wait().await;
        self.plugins.shutdown().await;
    }

    pub fn status(&self) -> StatusSnapshot {
        StatusSnapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            local_device: LocalDeviceSnapshot {
                device_id: self.local_device_id.to_string(),
                device_name: self.local_device_name(),
            },
            protocol_version: self.protocol_version,
        }
    }

    fn read_state(&self) -> Result<std::sync::RwLockReadGuard<'_, CoreState>, CoreError> {
        self.state.read().map_err(|_| CoreError::StateUnavailable)
    }
}

/// Summary of the daemon's local KDE Connect identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDeviceSnapshot {
    pub device_id: String,
    pub device_name: String,
}

/// The daemon's status, for `GET /status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    pub version: String,
    pub uptime_seconds: u64,
    pub local_device: LocalDeviceSnapshot,
    pub protocol_version: u8,
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::core::testing::handle;

    #[tokio::test]
    async fn status_and_empty_snapshots_are_readable() {
        let (handle, _commands) = handle().await;
        assert_eq!(handle.status().protocol_version, 8);
        assert_eq!(handle.devices().unwrap(), Vec::new());
        assert_eq!(handle.pairings().unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn cancelled_settings_call_still_updates_memory_and_persistence() {
        let (core, _commands) = handle().await;
        let settings = Settings::new(SettingsDefaults {
            device_name: "Ferry".into(),
            download_dir: std::env::temp_dir(),
        })
        .with_store(core.store.clone())
        .await;
        core.install_settings(settings);
        let gate = core.settings_updates.lock().await;
        let caller = tokio::spawn({
            let core = core.clone();
            async move {
                core.update_settings(SettingsPatch {
                    device_name: Some(Some("Renamed".into())),
                    ..Default::default()
                })
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while core.mutations.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        drop(gate);
        tokio::time::timeout(std::time::Duration::from_secs(1), core.shutdown_plugins())
            .await
            .unwrap();
        assert_eq!(core.settings().unwrap().device_name, "Renamed");
        assert_eq!(core.local_device_name(), "Renamed");
        assert_eq!(
            core.store.get(&DEVICE_NAME).await.unwrap().as_deref(),
            Some("Renamed")
        );
    }

    #[test]
    fn status_snapshots_use_stable_camel_case_json_names() {
        let status = StatusSnapshot {
            version: "0.1.0".into(),
            uptime_seconds: 7,
            local_device: LocalDeviceSnapshot {
                device_id: "local".into(),
                device_name: "Desk".into(),
            },
            protocol_version: 8,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({
                "version": "0.1.0",
                "uptimeSeconds": 7,
                "localDevice": {"deviceId": "local", "deviceName": "Desk"},
                "protocolVersion": 8
            })
        );
    }
}

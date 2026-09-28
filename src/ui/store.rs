//! What the core owns, cached for the pages: devices, pairings, transfers
//! and settings. Filled from a [`Snapshot`], patched by events and by the
//! snapshots mutations return, and discarded on exit.
//!
//! Events queued before a snapshot are replayed after it, so each resource
//! is patched only in ways a stale snapshot can't undo: a finished pairing
//! stays finished, and a transfer keeps its newest state.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::{
    core::{
        Core, DeviceReachability, DeviceSnapshot, EventData, PairingDirection, PairingSnapshot,
        SettingsSnapshot, TransferSnapshot,
    },
    ui::error::describe_error,
};

/// A resource as the store holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Load<T> {
    /// No snapshot yet.
    #[default]
    Loading,
    /// The first snapshot failed: why, in words for the user.
    Failed(String),
    Loaded(T),
}

impl<T> Load<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }

    pub fn into_loaded(self) -> Option<T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }

    /// The value made from the loaded one, or the same state.
    pub fn map<'a, U>(&'a self, f: impl FnOnce(&'a T) -> U) -> Load<U> {
        match self {
            Self::Loading => Load::Loading,
            Self::Failed(error) => Load::Failed(error.clone()),
            Self::Loaded(value) => Load::Loaded(f(value)),
        }
    }

    /// Take `result`, except that a failure keeps a value already loaded.
    fn refresh<U>(&mut self, result: Result<U, String>, into: impl FnOnce(U) -> T) {
        match result {
            Ok(value) => *self = Self::Loaded(into(value)),
            Err(error) if !matches!(self, Self::Loaded(_)) => *self = Self::Failed(error),
            Err(error) => tracing::warn!(%error, "keeping the state the UI has"),
        }
    }
}

/// Everything the store holds, read from the core at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub devices: Result<Vec<DeviceSnapshot>, String>,
    pub pairings: Result<Vec<PairingSnapshot>, String>,
    pub transfers: Vec<TransferSnapshot>,
    pub settings: Result<SettingsSnapshot, String>,
}

impl Snapshot {
    pub fn take(core: &Core) -> Self {
        Self {
            devices: core.devices().map_err(|error| describe_error(&error)),
            pairings: core.pairings().map_err(|error| describe_error(&error)),
            transfers: core.transfers().list(),
            settings: core.settings().map_err(|error| describe_error(&error)),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Store {
    devices: Load<BTreeMap<String, DeviceSnapshot>>,
    pairings: Load<BTreeMap<Uuid, PairingSnapshot>>,
    transfers: Load<BTreeMap<Uuid, TransferSnapshot>>,
    settings: Load<SettingsSnapshot>,
}

impl Store {
    /// Replace everything with `snapshot`. A resource that failed to read
    /// keeps what the store had.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.devices.refresh(snapshot.devices, |devices| {
            devices
                .into_iter()
                .map(|device| (device.device_id.clone(), device))
                .collect()
        });
        self.pairings.refresh(snapshot.pairings, |pairings| {
            pairings
                .into_iter()
                .map(|pairing| (pairing.id, pairing))
                .collect()
        });
        self.transfers.refresh(Ok(snapshot.transfers), |transfers| {
            transfers
                .into_iter()
                .map(|transfer| (transfer.id, transfer))
                .collect()
        });
        self.settings
            .refresh(snapshot.settings, |settings| settings);
    }

    /// Patch the store with a core event. Resources not loaded yet ignore
    /// it: their snapshot will include it.
    pub fn apply_event(&mut self, event: &EventData) {
        match event {
            EventData::DeviceDiscovered(device)
            | EventData::DeviceConnected(device)
            | EventData::DeviceUpdated(device)
            | EventData::DeviceDisconnected(device) => {
                if let Load::Loaded(devices) = &mut self.devices {
                    devices.insert(device.device_id.clone(), device.clone());
                }
            }
            EventData::DeviceForgotten(device) => self.remove_device(&device.device_id),
            EventData::PairingRequested(pairing) | EventData::PairingUpdated(pairing) => {
                self.apply_pairing(pairing.clone());
            }
            EventData::TransferStarted(transfer)
            | EventData::TransferProgress(transfer)
            | EventData::TransferCompleted(transfer)
            | EventData::TransferFailed(transfer) => {
                self.apply_transfer(transfer.clone());
            }
            EventData::SettingsChanged(settings) => self.apply_settings(settings.clone()),
            EventData::Plugin(_) => {}
        }
    }

    /// Forget a device now, without waiting for `device.forgotten`.
    pub fn remove_device(&mut self, device_id: &str) {
        if let Load::Loaded(devices) = &mut self.devices {
            devices.remove(device_id);
        }
    }

    /// Store `pairing`, unless the one held has already ended and this one
    /// hasn't: a start's answer can arrive after the event that ended it.
    /// Returns the pairing as the store now has it.
    pub fn apply_pairing(&mut self, pairing: PairingSnapshot) -> PairingSnapshot {
        let Load::Loaded(pairings) = &mut self.pairings else {
            return pairing;
        };
        if let Some(held) = pairings.get(&pairing.id)
            && held.status.is_terminal()
            && !pairing.status.is_terminal()
        {
            return held.clone();
        }
        pairings.insert(pairing.id, pairing.clone());
        pairing
    }

    /// Store `transfer`, unless the one held is newer: it has ended and
    /// this one hasn't, or it changed later. A send's answer can arrive
    /// after the events that superseded it. Returns the transfer as the
    /// store now has it.
    pub fn apply_transfer(&mut self, transfer: TransferSnapshot) -> TransferSnapshot {
        let Load::Loaded(transfers) = &mut self.transfers else {
            return transfer;
        };
        if let Some(held) = transfers.get(&transfer.id)
            && ((held.status.is_terminal() && !transfer.status.is_terminal())
                || held.updated_at > transfer.updated_at)
        {
            return held.clone();
        }
        transfers.insert(transfer.id, transfer.clone());
        transfer
    }

    pub fn apply_settings(&mut self, settings: SettingsSnapshot) {
        if let Load::Loaded(held) = &mut self.settings {
            *held = settings;
        }
    }

    pub fn device(&self, device_id: &str) -> Option<&DeviceSnapshot> {
        self.devices.loaded()?.get(device_id)
    }

    /// Paired devices, for the home page, by name.
    pub fn paired_devices(&self) -> Load<Vec<&DeviceSnapshot>> {
        self.devices_where(|device| device.paired)
    }

    /// Devices that could be paired, for Add device, by name: unpaired
    /// ones that haven't gone away.
    pub fn pairing_candidates(&self) -> Load<Vec<&DeviceSnapshot>> {
        self.devices_where(|device| {
            !device.paired && device.reachability != DeviceReachability::Unavailable
        })
    }

    fn devices_where(&self, keep: impl Fn(&DeviceSnapshot) -> bool) -> Load<Vec<&DeviceSnapshot>> {
        self.devices.map(|devices| {
            let mut devices: Vec<_> = devices.values().filter(|device| keep(device)).collect();
            // Sorting is stable, and the map is by id, so equal names keep
            // one order.
            devices.sort_by_cached_key(|device| device.device_name.to_lowercase());
            devices
        })
    }

    pub fn pairing(&self, pairing_id: Uuid) -> Option<&PairingSnapshot> {
        self.pairings.loaded()?.get(&pairing_id)
    }

    /// Incoming requests waiting for the user to accept or reject them,
    /// oldest first.
    pub fn pending_incoming_pairings(&self) -> Vec<&PairingSnapshot> {
        let Some(pairings) = self.pairings.loaded() else {
            return Vec::new();
        };
        let mut pending: Vec<_> = pairings
            .values()
            .filter(|pairing| {
                pairing.direction == PairingDirection::Incoming && !pairing.status.is_terminal()
            })
            .collect();
        pending.sort_by_key(|pairing| pairing.created_at);
        pending
    }

    pub fn transfer(&self, transfer_id: Uuid) -> Option<&TransferSnapshot> {
        self.transfers.loaded()?.get(&transfer_id)
    }

    /// Transfers newest first, all of them or only those with one device.
    pub fn transfers(&self, device_id: Option<&str>) -> Load<Vec<&TransferSnapshot>> {
        self.transfers.map(|transfers| {
            let mut transfers: Vec<_> = transfers
                .values()
                .filter(|transfer| device_id.is_none_or(|id| transfer.device_id == id))
                .collect();
            transfers.sort_by_key(|transfer| std::cmp::Reverse(transfer.created_at));
            transfers
        })
    }

    pub fn settings(&self) -> &Load<SettingsSnapshot> {
        &self.settings
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        core::{
            CoreEvent, PairingStatus, SettingsPatch, TransferDirection, TransferStatus,
            testing::{handle, handle_with_event_capacity},
        },
        protocol::{DeviceType, IdentityBody},
        ui::testing,
    };

    fn device(
        id: char,
        name: &str,
        paired: bool,
        reachability: DeviceReachability,
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            device_id: id.to_string().repeat(32),
            paired,
            reachability,
            ..testing::device(name)
        }
    }

    fn pairing(id: u128, status: PairingStatus, created_at: u64) -> PairingSnapshot {
        PairingSnapshot {
            id: Uuid::from_u128(id),
            device_id: "a".repeat(32),
            device_name: "Phone".into(),
            direction: PairingDirection::Incoming,
            status,
            verification_code: Some("1234".into()),
            created_at,
            expires_at: created_at + 30,
            error_code: None,
        }
    }

    fn transfer(id: u128, device: char, created_at: u64) -> TransferSnapshot {
        TransferSnapshot {
            id: Uuid::from_u128(id),
            device_id: device.to_string().repeat(32),
            device_name: "Phone".into(),
            direction: TransferDirection::Incoming,
            status: TransferStatus::Transferring,
            file_name: "photo.jpg".into(),
            total_bytes: 100,
            transferred_bytes: 0,
            created_at,
            updated_at: created_at,
            error_code: None,
            saved_path: None,
        }
    }

    fn settings(name: &str) -> SettingsSnapshot {
        SettingsSnapshot {
            device_name: name.into(),
            download_dir: PathBuf::from("/tmp/downloads"),
            close_to_tray: true,
            language: None,
            appearance: None,
            plugins: BTreeMap::new(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            devices: Ok(vec![]),
            pairings: Ok(vec![]),
            transfers: vec![],
            settings: Ok(settings("Desktop")),
        }
    }

    fn loaded(snapshot: Snapshot) -> Store {
        let mut store = Store::default();
        store.apply_snapshot(snapshot);
        store
    }

    fn names(devices: Load<Vec<&DeviceSnapshot>>) -> Vec<&str> {
        devices
            .loaded()
            .unwrap()
            .iter()
            .map(|device| device.device_name.as_str())
            .collect()
    }

    fn pending(store: &Store) -> Vec<u128> {
        store
            .pending_incoming_pairings()
            .iter()
            .map(|pairing| pairing.id.as_u128())
            .collect()
    }

    fn transfer_ids(store: &Store, device: Option<&str>) -> Vec<u128> {
        store
            .transfers(device)
            .loaded()
            .unwrap()
            .iter()
            .map(|transfer| transfer.id.as_u128())
            .collect()
    }

    fn devices_store() -> Store {
        loaded(Snapshot {
            devices: Ok(vec![
                device('b', "Tablet", true, DeviceReachability::Connected),
                device('a', "phone", false, DeviceReachability::Discovered),
                device('c', "Gone", false, DeviceReachability::Unavailable),
                device('e', "desktop", true, DeviceReachability::Unavailable),
            ]),
            ..snapshot()
        })
    }

    #[test]
    fn nothing_is_loaded_before_the_first_snapshot() {
        let mut store = Store::default();
        store.apply_event(&EventData::DeviceConnected(device(
            'a',
            "Phone",
            true,
            DeviceReachability::Connected,
        )));
        assert_eq!(store.paired_devices(), Load::Loading);
        assert_eq!(store.transfers(None), Load::Loading);
        assert_eq!(store.settings(), &Load::Loading);
        assert!(store.pending_incoming_pairings().is_empty());
    }

    #[test]
    fn splits_paired_devices_from_candidates_sorted_by_name() {
        let store = devices_store();
        assert_eq!(names(store.paired_devices()), ["desktop", "Tablet"]);
        assert_eq!(names(store.pairing_candidates()), ["phone"]);
    }

    #[test]
    fn applies_device_events_and_removes_forgotten_devices() {
        let mut store = devices_store();
        let tablet = device('b', "Tablet", true, DeviceReachability::Unavailable);
        store.apply_event(&EventData::DeviceDisconnected(tablet.clone()));
        store.apply_event(&EventData::DeviceConnected(device(
            'd',
            "Laptop",
            true,
            DeviceReachability::Connected,
        )));
        assert_eq!(
            store.device(&tablet.device_id).unwrap().reachability,
            DeviceReachability::Unavailable
        );
        assert_eq!(
            names(store.paired_devices()),
            ["desktop", "Laptop", "Tablet"]
        );

        store.apply_event(&EventData::DeviceForgotten(tablet.clone()));
        assert!(store.device(&tablet.device_id).is_none());
    }

    #[test]
    fn forgetting_removes_the_device_without_waiting_for_the_event() {
        let mut store = devices_store();
        store.remove_device(&"b".repeat(32));
        assert_eq!(names(store.paired_devices()), ["desktop"]);
    }

    #[test]
    fn a_failed_snapshot_keeps_what_was_loaded() {
        let mut store = Store::default();
        store.apply_snapshot(Snapshot {
            devices: Err("Ferry is not responding.".into()),
            ..snapshot()
        });
        assert_eq!(
            store.paired_devices(),
            Load::Failed("Ferry is not responding.".into())
        );

        let mut store = devices_store();
        store.apply_snapshot(Snapshot {
            devices: Err("Ferry is not responding.".into()),
            ..snapshot()
        });
        assert_eq!(names(store.paired_devices()), ["desktop", "Tablet"]);
    }

    #[test]
    fn recovers_pairing_requests_made_before_the_ui_started() {
        let mut outgoing = pairing(2, PairingStatus::Requested, 0);
        outgoing.direction = PairingDirection::Outgoing;
        let store = loaded(Snapshot {
            pairings: Ok(vec![
                pairing(1, PairingStatus::Accepted, 0),
                outgoing,
                pairing(3, PairingStatus::AwaitingConfirmation, 0),
            ]),
            ..snapshot()
        });
        assert_eq!(pending(&store), [3]);
    }

    #[test]
    fn tracks_pairing_requests_and_their_resolution_through_events() {
        let mut store = loaded(snapshot());
        store.apply_event(&EventData::PairingRequested(pairing(
            2,
            PairingStatus::AwaitingConfirmation,
            5,
        )));
        store.apply_event(&EventData::PairingRequested(pairing(
            1,
            PairingStatus::AwaitingConfirmation,
            1,
        )));
        assert_eq!(pending(&store), [1, 2]);

        store.apply_event(&EventData::PairingUpdated(pairing(
            1,
            PairingStatus::Expired,
            1,
        )));
        assert_eq!(pending(&store), [2]);
    }

    #[test]
    fn accepting_applies_the_returned_pairing_at_once() {
        let mut store = loaded(Snapshot {
            pairings: Ok(vec![pairing(1, PairingStatus::AwaitingConfirmation, 0)]),
            ..snapshot()
        });
        store.apply_pairing(pairing(1, PairingStatus::Accepted, 0));
        assert!(pending(&store).is_empty());
        assert_eq!(
            store.pairing(Uuid::from_u128(1)).unwrap().status,
            PairingStatus::Accepted
        );
    }

    #[test]
    fn a_late_start_answer_does_not_undo_the_outcome() {
        let mut store = loaded(snapshot());
        let mut accepted = pairing(1, PairingStatus::Accepted, 0);
        accepted.direction = PairingDirection::Outgoing;
        store.apply_event(&EventData::PairingUpdated(accepted.clone()));

        let mut requested = pairing(1, PairingStatus::Requested, 0);
        requested.direction = PairingDirection::Outgoing;
        assert_eq!(store.apply_pairing(requested), accepted);
        assert_eq!(store.pairing(Uuid::from_u128(1)), Some(&accepted));
    }

    #[test]
    fn lists_transfers_newest_first_optionally_for_one_device() {
        let store = loaded(Snapshot {
            transfers: vec![
                transfer(1, 'a', 0),
                transfer(2, 'b', 1),
                transfer(3, 'a', 2),
            ],
            ..snapshot()
        });
        assert_eq!(transfer_ids(&store, None), [3, 2, 1]);
        assert_eq!(transfer_ids(&store, Some(&"a".repeat(32))), [3, 1]);
    }

    #[test]
    fn applies_transfer_progress_and_outcome() {
        let mut store = loaded(Snapshot {
            transfers: vec![transfer(1, 'a', 0)],
            ..snapshot()
        });
        let mut progress = transfer(1, 'a', 0);
        progress.transferred_bytes = 40;
        progress.updated_at = 1;
        store.apply_event(&EventData::TransferProgress(progress));
        let id = Uuid::from_u128(1);
        assert_eq!(store.transfer(id).unwrap().transferred_bytes, 40);

        let mut failed = transfer(1, 'a', 0);
        failed.status = TransferStatus::Failed;
        failed.updated_at = 2;
        store.apply_event(&EventData::TransferFailed(failed.clone()));
        assert_eq!(store.transfer(id), Some(&failed));
    }

    #[test]
    fn a_send_answer_never_overwrites_a_newer_event() {
        let mut store = loaded(snapshot());
        let mut completed = transfer(1, 'a', 0);
        completed.status = TransferStatus::Completed;
        completed.transferred_bytes = 100;
        completed.updated_at = 5;
        store.apply_event(&EventData::TransferCompleted(completed.clone()));

        // The answer describes the transfer mid-flight.
        let mut answer = transfer(1, 'a', 0);
        answer.updated_at = 4;
        assert_eq!(store.apply_transfer(answer), completed);
        // Even at the same time, an ended transfer stays ended.
        let mut answer = transfer(1, 'a', 0);
        answer.updated_at = 5;
        assert_eq!(store.apply_transfer(answer), completed);
        assert_eq!(store.transfer(Uuid::from_u128(1)), Some(&completed));
    }

    #[test]
    fn follows_settings_changed() {
        let mut store = loaded(snapshot());
        store.apply_event(&EventData::SettingsChanged(settings("Renamed")));
        assert_eq!(store.settings().loaded().unwrap().device_name, "Renamed");
    }

    fn identity(id: &str, name: &str) -> IdentityBody {
        IdentityBody {
            device_id: id.into(),
            device_name: name.into(),
            device_type: DeviceType::Phone,
            incoming_capabilities: vec![],
            outgoing_capabilities: vec![],
            protocol_version: 8,
            extra: Default::default(),
        }
    }

    /// The order the sync subscription works in: subscribe, take the
    /// snapshot, then replay every event, including those the snapshot
    /// already has.
    #[test]
    fn events_around_a_snapshot_survive_it() {
        let (core, _commands) = handle_with_event_capacity(8);
        core.discover_device(&identity(&"a".repeat(32), "phone"), true, 1)
            .unwrap();
        let mut events = core.subscribe();
        let mut store = Store::default();

        // Changes while the snapshot is taken: one it includes, and one it
        // misses.
        core.discover_device(&identity(&"d".repeat(32), "Laptop"), true, 2)
            .unwrap();
        let snapshot = Snapshot::take(&core);
        core.forget_device(&"a".repeat(32)).unwrap();
        store.apply_snapshot(snapshot);
        assert_eq!(names(store.paired_devices()), ["Laptop", "phone"]);

        while let Ok(CoreEvent { event, .. }) = events.try_recv() {
            store.apply_event(&event);
        }
        assert_eq!(names(store.paired_devices()), ["Laptop"]);
    }

    #[test]
    fn a_settings_change_applies_the_answer_and_its_event() {
        let (core, _commands) = handle();
        let mut events = core.subscribe();
        let mut store = loaded(Snapshot::take(&core));
        let close_to_tray = store.settings().loaded().unwrap().close_to_tray;

        let answer = core
            .update_settings(SettingsPatch {
                device_name: Some(Some("Renamed".into())),
                ..SettingsPatch::default()
            })
            .unwrap();
        store.apply_settings(answer);
        let held = store.settings().loaded().unwrap().clone();
        assert_eq!(held.device_name, "Renamed");
        // Only the name changed.
        assert_eq!(held.close_to_tray, close_to_tray);

        let event = events.try_recv().unwrap().event;
        store.apply_event(&event);
        assert_eq!(store.settings().loaded(), Some(&held));
    }
}

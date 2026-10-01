//! The core's methods: status, devices, pairings, transfers, settings and
//! events. Plugins add theirs through [`crate::core::Plugin::methods`].

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use super::{Method, Methods, RpcError, StreamMethod};
use crate::core::{
    Core, CoreError, CoreEvent, DeviceSnapshot, Host, PairingSnapshot, SettingsPatch,
    SettingsSnapshot, StatusSnapshot, TransferSnapshot,
};

define_methods! {
    /// The daemon's version, uptime and local device.
    "status" => Status {} -> StatusSnapshot;
    /// Every known device.
    "devices.list" => ListDevices {} -> Vec<DeviceSnapshot>;
    /// One device; `device_not_found` if it isn't known.
    "devices.get" => GetDevice { device_id: String } -> DeviceSnapshot;
    /// Announce this device so peers answer promptly: broadcast, or to
    /// `address` only, for networks where broadcast doesn't reach the peer.
    "devices.scan" => Scan { address: Option<Host> } -> ();
    /// Wait for a device to answer at an IPv4 address or hostname,
    /// announcing to it until one does, and answer with it, ready to pair.
    /// A device that pairs afterwards keeps the address.
    /// `address_unreachable` if nothing answered.
    "devices.connect" => Connect { address: Host } -> DeviceSnapshot;
    /// Replace the addresses a paired device is reached at when it isn't
    /// found by broadcast. Answers with what is saved.
    "devices.setAddresses" => SetAddresses { device_id: String, addresses: Vec<Host> } -> Vec<Host>;
    /// Unpair and forget a device.
    "devices.forget" => ForgetDevice { device_id: String } -> ();
    /// Every pairing this daemon process knows about, including finished
    /// ones, so a client can find incoming requests it didn't see arrive.
    "pairings.list" => ListPairings {} -> Vec<PairingSnapshot>;
    "pairings.get" => GetPairing { pairing_id: Uuid } -> PairingSnapshot;
    /// Ask a device to pair.
    "pairings.start" => StartPairing { device_id: String } -> PairingSnapshot;
    /// Accept a device's request to pair.
    "pairings.accept" => AcceptPairing { pairing_id: Uuid } -> PairingSnapshot;
    /// Reject a request, or cancel one this device made.
    "pairings.cancel" => CancelPairing { pairing_id: Uuid } -> PairingSnapshot;
    "transfers.list" => ListTransfers {} -> Vec<TransferSnapshot>;
    "transfers.get" => GetTransfer { transfer_id: Uuid } -> TransferSnapshot;
    "transfers.cancel" => CancelTransfer { transfer_id: Uuid } -> TransferSnapshot;
    "settings.get" => GetSettings {} -> SettingsSnapshot;
    /// Every core event from now on: first `null`, once subscribed, then
    /// each event. Never answers, except with `events_lagged` when the
    /// client fell behind and missed some; it then takes fresh snapshots
    /// and subscribes again.
    "events.subscribe" => Subscribe {} -> ();
}

impl StreamMethod for Subscribe {
    type Item = Option<CoreEvent>;
}

/// Change the settings present; `null` resets one to its default. The
/// change is saved before it takes effect, and `settings.changed` is
/// published if anything changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpdateSettings(pub SettingsPatch);

impl Method for UpdateSettings {
    const NAME: &'static str = "settings.update";
    type Output = SettingsSnapshot;
}

/// Every method a daemon running `core` answers: the core's and its
/// plugins'.
pub fn all(core: &Core) -> Methods {
    let mut methods = Methods::new();
    add_core(core, &mut methods);
    core.plugin_methods(&mut methods);
    methods
}

fn add_core(core: &Core, methods: &mut Methods) {
    let core = || core.clone();
    methods.add(core(), |core, Status {}| async move {
        Ok::<_, CoreError>(core.status())
    });
    methods.add(core(), |core, ListDevices {}| async move { core.devices() });
    methods.add(core(), |core, GetDevice { device_id }| async move {
        core.device(&device_id).ok_or(CoreError::UnknownDevice)
    });
    methods.add(core(), |core, Scan { address }| async move {
        match address {
            Some(address) => core.announce_to(&address).await,
            None => core.announce(),
        }
    });
    methods.add(core(), |core, Connect { address }| async move {
        core.connect_address(address).await
    });
    methods.add(
        core(),
        |core,
         SetAddresses {
             device_id,
             addresses,
         }| async move { core.set_device_addresses(&device_id, addresses).await },
    );
    methods.add(core(), |core, ForgetDevice { device_id }| async move {
        core.forget_device(&device_id).await
    });
    methods.add(
        core(),
        |core, ListPairings {}| async move { core.pairings() },
    );
    methods.add(core(), |core, GetPairing { pairing_id }| async move {
        core.pairing(pairing_id).ok_or(CoreError::UnknownPairing)
    });
    methods.add(core(), |core, StartPairing { device_id }| async move {
        core.start_outgoing_pairing(&device_id).await
    });
    methods.add(core(), |core, AcceptPairing { pairing_id }| async move {
        core.accept_pairing(pairing_id).await
    });
    methods.add(core(), |core, CancelPairing { pairing_id }| async move {
        core.cancel_pairing(pairing_id).await
    });
    methods.add(core(), |core, ListTransfers {}| async move {
        Ok::<_, CoreError>(core.transfers().list())
    });
    methods.add(core(), |core, GetTransfer { transfer_id }| async move {
        core.transfers()
            .get(transfer_id)
            .ok_or(CoreError::UnknownTransfer)
    });
    methods.add(core(), |core, CancelTransfer { transfer_id }| async move {
        core.cancel_transfer(transfer_id)
    });
    methods.add(
        core(),
        |core, GetSettings {}| async move { core.settings() },
    );
    methods.add(core(), |core, UpdateSettings(patch)| async move {
        core.update_settings(patch).await
    });
    methods.add_stream(core(), |core, Subscribe {}, items| async move {
        let mut events = core.subscribe();
        if !items.send(&None).await {
            return Ok(());
        }
        loop {
            match events.recv().await {
                Ok(event) => {
                    if !items.send(&Some(event)).await {
                        return Ok(());
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    return Err(RpcError::failed(
                        "events_lagged",
                        "the client fell behind and missed events",
                    ));
                }
                Err(RecvError::Closed) => return Ok(()),
            }
        }
    });
}

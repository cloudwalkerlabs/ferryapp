//! Addresses a paired device can be reached at when broadcast discovery
//! doesn't find it (a tailnet or VPN, another subnet). They are kept per
//! device ([`ADDRESSES`]); the LAN transport announces to those of
//! devices that aren't connected on its interval
//! ([`Core::fallback_addresses`]).
//!
//! Adding a device by address is [`Core::connect_address`]: it announces
//! to the address until a device connects from it, or gives up. The
//! address is saved only once that device is paired
//! ([`Core::commit_pending_address`]), so an address that never led to a
//! device is never stored.

use std::{net::Ipv4Addr, time::Duration};

use tokio::time::{Instant, sleep_until};

use super::{Core, CoreError, DeviceReachability, DeviceSnapshot, LanCommand};
use crate::store::{ConfigKey, PerDevice};

/// The addresses to try for a paired device, in the order they were added.
pub const ADDRESSES: ConfigKey<Vec<Ipv4Addr>, PerDevice> = ConfigKey::new("core.addresses");

/// How many addresses one device keeps.
pub const MAX_ADDRESSES: usize = 8;

/// How long [`Core::connect_address`] waits for a device to answer. Shorter
/// than the API's request deadline, so a request gets its answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often [`Core::connect_address`] looks for the connection, and how
/// many looks pass between announcements.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const POLLS_PER_ANNOUNCEMENT: u32 = 8;

/// `address` if it can be announced to: a unicast address, not `0.0.0.0`,
/// broadcast or multicast, so the identity can't be sprayed at a group.
pub(super) fn unicast(address: Ipv4Addr) -> Result<Ipv4Addr, CoreError> {
    if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
        Err(CoreError::InvalidDiscoveryAddress)
    } else {
        Ok(address)
    }
}

impl Core {
    /// The addresses saved for `device_id`.
    pub(super) fn saved_addresses(&self, device_id: &str) -> Vec<Ipv4Addr> {
        self.store
            .cached(&ADDRESSES.of(device_id))
            .inspect_err(|error| tracing::warn!(%error, "ignoring unreadable device addresses"))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Replace the addresses saved for a paired device, dropping repeats,
    /// and tell clients. Returns what is saved.
    pub async fn set_device_addresses(
        &self,
        device_id: &str,
        addresses: Vec<Ipv4Addr>,
    ) -> Result<Vec<Ipv4Addr>, CoreError> {
        let mut unique = Vec::with_capacity(addresses.len());
        for address in addresses {
            let address = unicast(address)?;
            if !unique.contains(&address) {
                unique.push(address);
            }
        }
        if unique.len() > MAX_ADDRESSES {
            return Err(CoreError::TooManyAddresses);
        }
        let operation = self.device_operation(device_id);
        let _operation = operation.lock().await;
        let device = self.device(device_id).ok_or(CoreError::UnknownDevice)?;
        if !device.paired {
            return Err(CoreError::NotPaired);
        }
        self.store_addresses(device_id, &unique).await?;
        Ok(unique)
    }

    /// Write `addresses` (removing the entry when empty) and publish the
    /// device. The caller holds the device's operation lock.
    async fn store_addresses(
        &self,
        device_id: &str,
        addresses: &[Ipv4Addr],
    ) -> Result<(), CoreError> {
        if addresses == self.saved_addresses(device_id) {
            return Ok(());
        }
        if addresses.is_empty() {
            self.store
                .remove(&ADDRESSES.of(device_id))
                .await
                .map(|_| ())
        } else {
            self.store
                .set(&ADDRESSES.of(device_id), &addresses.to_vec())
                .await
        }
        .map_err(CoreError::Store)?;
        self.publish_device_update(device_id);
        Ok(())
    }

    /// Every saved address of a paired device that isn't connected: where
    /// the LAN transport announces on its interval, so a device that
    /// broadcast can't reach still finds this one.
    pub fn fallback_addresses(&self) -> Vec<Ipv4Addr> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut addresses = Vec::new();
        for device in state.devices.snapshot() {
            if device.paired && device.reachability != DeviceReachability::Connected {
                for address in self.saved_addresses(&device.device_id) {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
            }
        }
        addresses
    }

    /// Wait for a device to connect from `address`, announcing to it until
    /// one does, and return it. Dropping the future cancels the attempt.
    ///
    /// A paired device keeps the address at once. An unpaired one keeps it
    /// once it is paired, as long as it stays connected until then. Nothing
    /// is saved when this fails with [`CoreError::AddressUnreachable`].
    pub async fn connect_address(&self, address: Ipv4Addr) -> Result<DeviceSnapshot, CoreError> {
        let address = unicast(address)?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut polls = 0_u32;
        let device_id = loop {
            if let Some(device_id) = self.device_connected_from(address) {
                break device_id;
            }
            if Instant::now() >= deadline {
                return Err(CoreError::AddressUnreachable);
            }
            if polls.is_multiple_of(POLLS_PER_ANNOUNCEMENT) {
                match self.send_lan_command(LanCommand::AnnounceTo { address }) {
                    // A full queue is already busy announcing.
                    Ok(()) | Err(CoreError::CommandQueueFull) => {}
                    Err(error) => return Err(error),
                }
            }
            polls += 1;
            sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
        };

        let operation = self.device_operation(&device_id);
        let _operation = operation.lock().await;
        let device = self.device(&device_id).ok_or(CoreError::UnknownDevice)?;
        if device.paired {
            let mut addresses = self.saved_addresses(&device_id);
            if !addresses.contains(&address) && addresses.len() < MAX_ADDRESSES {
                addresses.push(address);
                self.store_addresses(&device_id, &addresses).await?;
            }
        } else {
            self.state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?
                .pending_addresses
                .insert(device_id.clone(), address);
        }
        Ok(self.device(&device_id).unwrap_or(device))
    }

    /// The device with a live connection from `address`.
    fn device_connected_from(&self, address: Ipv4Addr) -> Option<String> {
        let state = self.state.read().ok()?;
        state
            .connections
            .iter()
            .find(|(device_id, connection)| {
                connection
                    .peer_addr
                    .is_some_and(|peer| peer.ip() == address)
                    && state.devices.get(device_id).is_some()
            })
            .map(|(device_id, _)| device_id.clone())
    }

    /// Save the address a device was connected from, if
    /// [`Self::connect_address`] left one for it. Called when the device
    /// becomes paired, with its operation lock held.
    pub(super) async fn commit_pending_address(&self, device_id: &str) {
        let pending = self
            .state
            .write()
            .ok()
            .and_then(|mut state| state.pending_addresses.remove(device_id));
        let Some(address) = pending else {
            return;
        };
        let mut addresses = self.saved_addresses(device_id);
        if !addresses.contains(&address) && addresses.len() < MAX_ADDRESSES {
            addresses.push(address);
            if let Err(error) = self.store_addresses(device_id, &addresses).await {
                tracing::warn!(%error, "could not save the device's address");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        core::testing::{handle, handle_with_trust},
        store::testing::trusted_device,
    };

    const PEER: &str = "740bd4b9b4184ee497d6caf1da8151be";

    async fn paired_core() -> (Core, mpsc::Receiver<LanCommand>) {
        handle_with_trust(vec![trusted_device(PEER)]).await
    }

    async fn connected_from(core: &Core, device_id: &str, address: Ipv4Addr) {
        let (packets, _) = mpsc::channel(4);
        core.register_connection(device_id, vec![1], 8, packets, CancellationToken::new(), 1)
            .await
            .unwrap();
        core.set_connection_peer_addr(device_id, SocketAddr::new(address.into(), 40000));
    }

    #[test]
    fn only_unicast_addresses_are_kept() {
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 251),
        ] {
            assert!(matches!(
                unicast(address),
                Err(CoreError::InvalidDiscoveryAddress)
            ));
        }
        assert!(unicast(Ipv4Addr::new(100, 64, 0, 7)).is_ok());
    }

    #[tokio::test]
    async fn addresses_belong_to_paired_devices_and_are_deduplicated() {
        let (core, _commands) = paired_core().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        let b = Ipv4Addr::new(192, 168, 1, 20);
        let saved = core
            .set_device_addresses(PEER, vec![a, b, a])
            .await
            .unwrap();
        assert_eq!(saved, [a, b]);
        assert_eq!(core.device(PEER).unwrap().addresses, [a, b]);

        assert!(matches!(
            core.set_device_addresses(PEER, vec![Ipv4Addr::BROADCAST])
                .await,
            Err(CoreError::InvalidDiscoveryAddress)
        ));
        assert!(matches!(
            core.set_device_addresses(PEER, (1..=9).map(|n| Ipv4Addr::new(10, 0, 0, n)).collect())
                .await,
            Err(CoreError::TooManyAddresses)
        ));
        assert!(matches!(
            core.set_device_addresses("nobody", vec![a]).await,
            Err(CoreError::UnknownDevice)
        ));

        core.set_device_addresses(PEER, Vec::new()).await.unwrap();
        assert!(core.device(PEER).unwrap().addresses.is_empty());
    }

    #[tokio::test]
    async fn fallback_addresses_skip_connected_devices() {
        let (core, _commands) = paired_core().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        core.set_device_addresses(PEER, vec![a]).await.unwrap();
        assert_eq!(core.fallback_addresses(), [a]);
        connected_from(&core, PEER, a).await;
        assert!(core.fallback_addresses().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn connecting_to_an_address_nobody_answers_saves_nothing() {
        let (core, mut commands) = handle().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        let error = core.connect_address(a).await.unwrap_err();
        assert!(matches!(error, CoreError::AddressUnreachable));
        assert_eq!(
            commands.try_recv().unwrap(),
            LanCommand::AnnounceTo { address: a }
        );
        assert!(core.state.read().unwrap().pending_addresses.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_paired_device_that_answers_keeps_the_address() {
        let (core, _commands) = paired_core().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        let attempt = tokio::spawn({
            let core = core.clone();
            async move { core.connect_address(a).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        connected_from(&core, PEER, a).await;
        let device = attempt.await.unwrap().unwrap();
        assert_eq!(device.device_id, PEER);
        assert_eq!(core.device(PEER).unwrap().addresses, [a]);
    }
}

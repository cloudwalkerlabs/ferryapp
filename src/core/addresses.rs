//! Addresses a paired device can be reached at when broadcast discovery
//! doesn't find it (a tailnet or VPN, another subnet). An address is a
//! [`Host`]: an IPv4 address or a hostname (a tailnet's MagicDNS name, an
//! internal domain) that the system resolver turns into IPv4 addresses each
//! time it is used, so a device whose IP changes is still found. They are
//! kept per device ([`ADDRESSES`]); the LAN transport announces to those of
//! devices that aren't connected on its interval
//! ([`Core::fallback_addresses`]).
//!
//! Adding a device by address is [`Core::connect_address`]: it announces
//! to the address until a device connects from it, or gives up. The
//! address is saved only once that device is paired
//! ([`Core::commit_pending_address`]), so an address that never led to a
//! device is never stored.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
    str::FromStr,
    time::Duration,
};

use futures_util::future::join_all;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::{
    net::lookup_host,
    time::{Instant, sleep_until, timeout},
};

use super::{Core, CoreError, DeviceReachability, DeviceSnapshot, LanCommand};
use crate::store::{ConfigKey, PerDevice};

/// The addresses to try for a paired device, in the order they were added.
pub const ADDRESSES: ConfigKey<Vec<Host>, PerDevice> = ConfigKey::new("core.addresses");

/// How many addresses one device keeps.
pub const MAX_ADDRESSES: usize = 8;

/// How long [`Core::connect_address`] waits for a device to answer. Shorter
/// than the API's request deadline, so a request gets its answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one name may take to resolve before it counts as not found, so
/// a dead resolver can't stall the announce interval.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

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

/// Where a device can be reached: a unicast IPv4 address or a hostname.
/// Written as text (`100.64.0.7`, `phone.tailnet.ts.net`), lowercased, and
/// checked when made, so a `Host` is always something [`Host::resolve`] can
/// ask the resolver about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Host(String);

impl Host {
    /// The longest name DNS allows, and the longest label in it.
    const MAX_LENGTH: usize = 253;
    const MAX_LABEL: usize = 63;

    /// The IPv4 addresses this host stands for now. An address is itself;
    /// a name goes to the system resolver (the OS's `getaddrinfo`, so
    /// `/etc/hosts`, mDNS, a tailnet's MagicDNS and the like all apply), and
    /// only its unicast IPv4 answers are kept, as the LAN transport speaks
    /// IPv4. Empty when the name isn't found or the resolver is too slow.
    pub async fn resolve(&self) -> Vec<Ipv4Addr> {
        if let Ok(address) = self.0.parse::<Ipv4Addr>() {
            return vec![address];
        }
        let found = match timeout(RESOLVE_TIMEOUT, lookup_host((self.0.as_str(), 0))).await {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::debug!(host = %self.0, %error, "could not resolve the address");
                return Vec::new();
            }
            Err(_) => {
                tracing::debug!(host = %self.0, "resolving the address timed out");
                return Vec::new();
            }
        };
        let mut addresses = Vec::new();
        for found in found {
            if let IpAddr::V4(address) = found.ip()
                && unicast(address).is_ok()
                && !addresses.contains(&address)
            {
                addresses.push(address);
            }
        }
        addresses
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_hostname(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= Self::MAX_LENGTH
            && name.split('.').all(|label| {
                (1..=Self::MAX_LABEL).contains(&label.len())
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
            // `1.2.3` and `999.1.1.1` are malformed addresses, not names:
            // a name's last label is never all digits.
            && name
                .rsplit('.')
                .next()
                .is_some_and(|label| !label.bytes().all(|byte| byte.is_ascii_digit()))
    }
}

impl From<Ipv4Addr> for Host {
    fn from(address: Ipv4Addr) -> Self {
        Self(address.to_string())
    }
}

impl FromStr for Host {
    type Err = CoreError;

    fn from_str(text: &str) -> Result<Self, CoreError> {
        let text = text.trim();
        if let Ok(address) = text.parse::<Ipv4Addr>() {
            return unicast(address).map(Self::from);
        }
        // An absolute name's trailing dot is the same name.
        let name = text.strip_suffix('.').unwrap_or(text).to_ascii_lowercase();
        if Self::is_hostname(&name) {
            Ok(Self(name))
        } else {
            Err(CoreError::InvalidDiscoveryAddress)
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for Host {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Host {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Core {
    /// The addresses saved for `device_id`.
    pub(super) fn saved_addresses(&self, device_id: &str) -> Vec<Host> {
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
        addresses: Vec<Host>,
    ) -> Result<Vec<Host>, CoreError> {
        let mut unique = Vec::with_capacity(addresses.len());
        for address in addresses {
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
    async fn store_addresses(&self, device_id: &str, addresses: &[Host]) -> Result<(), CoreError> {
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

    /// Every saved address of a paired device that isn't connected,
    /// resolved: where the LAN transport announces on its interval, so a
    /// device that broadcast can't reach still finds this one. Names are
    /// looked up now, all at once, so a moved device is followed and a slow
    /// name delays the rest by at most the resolve timeout.
    pub async fn fallback_addresses(&self) -> Vec<Ipv4Addr> {
        let mut hosts = Vec::new();
        if let Ok(state) = self.state.read() {
            for device in state.devices.snapshot() {
                if device.paired && device.reachability != DeviceReachability::Connected {
                    for host in self.saved_addresses(&device.device_id) {
                        if !hosts.contains(&host) {
                            hosts.push(host);
                        }
                    }
                }
            }
        }
        let mut addresses = Vec::new();
        for address in join_all(hosts.iter().map(Host::resolve))
            .await
            .into_iter()
            .flatten()
        {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        addresses
    }

    /// Wait for a device to connect from `address` (a name is resolved
    /// first, and any of its addresses counts), announcing to it until one
    /// does, and return it. Dropping the future cancels the attempt. Fails
    /// with [`CoreError::AddressUnresolvable`] if a name isn't found.
    ///
    /// A paired device keeps the address at once. An unpaired one keeps it
    /// once it is paired, as long as it stays connected until then. Nothing
    /// is saved when this fails with [`CoreError::AddressUnreachable`].
    pub async fn connect_address(&self, address: Host) -> Result<DeviceSnapshot, CoreError> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut resolved = address.resolve().await;
        if resolved.is_empty() {
            return Err(CoreError::AddressUnresolvable);
        }
        let mut polls = 0_u32;
        let device_id = loop {
            if let Some(device_id) = self.device_connected_from(&resolved) {
                break device_id;
            }
            if Instant::now() >= deadline {
                return Err(CoreError::AddressUnreachable);
            }
            if polls.is_multiple_of(POLLS_PER_ANNOUNCEMENT) {
                // A name may have moved since the last look.
                if polls > 0 {
                    let now = address.resolve().await;
                    if !now.is_empty() {
                        resolved = now;
                    }
                }
                for &target in &resolved {
                    match self.send_lan_command(LanCommand::AnnounceTo { address: target }) {
                        // A full queue is already busy announcing.
                        Ok(()) | Err(CoreError::CommandQueueFull) => {}
                        Err(error) => return Err(error),
                    }
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

    /// The device with a live connection from one of `addresses`.
    fn device_connected_from(&self, addresses: &[Ipv4Addr]) -> Option<String> {
        let state = self.state.read().ok()?;
        state
            .connections
            .iter()
            .find(|(device_id, connection)| {
                connection.peer_addr.is_some_and(
                    |peer| matches!(peer.ip(), IpAddr::V4(ip) if addresses.contains(&ip)),
                ) && state.devices.get(device_id).is_some()
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

    fn host(text: &str) -> Host {
        text.parse().unwrap()
    }

    #[test]
    fn only_unicast_addresses_and_valid_names_are_kept() {
        for text in [
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.251",
            "1.2.3",
            "999.1.1.1",
            "::1",
            "",
            "a b",
            "-x.example",
            "x..example",
            "under score!.example",
            "http://phone",
        ] {
            assert!(
                matches!(
                    text.parse::<Host>(),
                    Err(CoreError::InvalidDiscoveryAddress)
                ),
                "{text}"
            );
        }
        assert!(
            format!("{}.example", "a".repeat(64))
                .parse::<Host>()
                .is_err()
        );
        assert_eq!(host(" 100.64.0.7 ").as_str(), "100.64.0.7");
        // Names are case-insensitive, and an absolute name is the same name.
        assert_eq!(
            host("Phone.Tailnet.TS.net.").as_str(),
            "phone.tailnet.ts.net"
        );
        assert_eq!(host("nas").as_str(), "nas");
    }

    #[test]
    fn a_host_is_saved_as_plain_text() {
        let hosts = vec![host("100.64.0.7"), host("phone.example.net")];
        let json = serde_json::to_string(&hosts).unwrap();
        assert_eq!(json, r#"["100.64.0.7","phone.example.net"]"#);
        assert_eq!(serde_json::from_str::<Vec<Host>>(&json).unwrap(), hosts);
        assert!(serde_json::from_str::<Host>(r#""0.0.0.0""#).is_err());
    }

    #[tokio::test]
    async fn a_host_resolves_through_the_system() {
        let a = Ipv4Addr::new(100, 64, 0, 7);
        assert_eq!(Host::from(a).resolve().await, [a]);
        assert_eq!(host("localhost").resolve().await, [Ipv4Addr::LOCALHOST]);
        // Reserved by RFC 2606: never resolves.
        assert!(host("nothing.invalid").resolve().await.is_empty());
    }

    #[tokio::test]
    async fn addresses_belong_to_paired_devices_and_are_deduplicated() {
        let (core, _commands) = paired_core().await;
        let a = host("100.64.0.7");
        let b = host("phone.example.net");
        let saved = core
            .set_device_addresses(PEER, vec![a.clone(), b.clone(), a.clone()])
            .await
            .unwrap();
        assert_eq!(saved, [a.clone(), b.clone()]);
        assert_eq!(core.device(PEER).unwrap().addresses, [a.clone(), b]);

        assert!(matches!(
            core.set_device_addresses(
                PEER,
                (1..=9).map(|n| Ipv4Addr::new(10, 0, 0, n).into()).collect()
            )
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
    async fn fallback_addresses_resolve_names_and_skip_connected_devices() {
        let (core, _commands) = paired_core().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        core.set_device_addresses(
            PEER,
            vec![a.into(), host("localhost"), host("nothing.invalid")],
        )
        .await
        .unwrap();
        assert_eq!(core.fallback_addresses().await, [a, Ipv4Addr::LOCALHOST]);
        connected_from(&core, PEER, a).await;
        assert!(core.fallback_addresses().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn connecting_to_an_address_nobody_answers_saves_nothing() {
        let (core, mut commands) = handle().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        let error = core.connect_address(a.into()).await.unwrap_err();
        assert!(matches!(error, CoreError::AddressUnreachable));
        assert_eq!(
            commands.try_recv().unwrap(),
            LanCommand::AnnounceTo { address: a }
        );
        assert!(core.state.read().unwrap().pending_addresses.is_empty());
    }

    #[tokio::test]
    async fn connecting_to_a_name_that_does_not_resolve_fails_at_once() {
        let (core, mut commands) = handle().await;
        let error = core
            .connect_address(host("nothing.invalid"))
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::AddressUnresolvable));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_paired_device_that_answers_at_a_name_keeps_the_name() {
        let (core, mut commands) = paired_core().await;
        let attempt = tokio::spawn({
            let core = core.clone();
            async move { core.connect_address(host("localhost")).await }
        });
        assert_eq!(
            commands.recv().await,
            Some(LanCommand::AnnounceTo {
                address: Ipv4Addr::LOCALHOST
            })
        );
        connected_from(&core, PEER, Ipv4Addr::LOCALHOST).await;
        let device = attempt.await.unwrap().unwrap();
        assert_eq!(device.device_id, PEER);
        assert_eq!(core.device(PEER).unwrap().addresses, [host("localhost")]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_paired_device_that_answers_keeps_the_address() {
        let (core, _commands) = paired_core().await;
        let a = Ipv4Addr::new(100, 64, 0, 7);
        let attempt = tokio::spawn({
            let core = core.clone();
            async move { core.connect_address(a.into()).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        connected_from(&core, PEER, a).await;
        let device = attempt.await.unwrap().unwrap();
        assert_eq!(device.device_id, PEER);
        assert_eq!(core.device(PEER).unwrap().addresses, [Host::from(a)]);
    }
}

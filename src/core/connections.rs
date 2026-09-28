//! Live control channels to peers: registering and dropping them, routing
//! the packets that arrive on them, sending packets to peers that accept
//! them, and the channel through which the core asks the LAN transport to
//! announce this device.

use std::net::{Ipv4Addr, SocketAddr};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    Core, CoreError, DeviceSnapshot, EventData, OperationErrorCode, PayloadPeer,
    pairing::fail_active_pairing, unix_seconds,
};
use crate::protocol::{Packet, PairingBody};

/// What the core asks of the LAN transport, over the channel
/// [`super::Core::new`] returns for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LanCommand {
    /// Broadcast this device's identity now.
    AnnounceDiscovery,
    /// Send this device's identity to one address.
    AnnounceTo { address: Ipv4Addr },
}

/// A live, TLS-authenticated control-channel connection to a peer, as
/// registered by the transport layer once the double identity exchange and
/// TLS handshake succeed.
#[derive(Clone)]
pub(super) struct Connection {
    pub(super) packets: mpsc::Sender<Packet>,
    pub(super) certificate_der: Vec<u8>,
    pub(super) protocol_version: u8,
    pub(super) cancellation: CancellationToken,
    /// The peer's IP address, used to dial payload and file-server ports it
    /// advertises on the same host. `None` only if a connection was
    /// registered without going through real LAN transport (e.g. some unit
    /// tests), in which case incoming transfers cannot be established.
    pub(super) peer_addr: Option<SocketAddr>,
}

impl Core {
    /// Register a live, TLS-authenticated control channel for `device_id`.
    /// Called by the transport layer only after the pre-TLS identity, the
    /// TLS handshake (with real signature verification and, for trusted
    /// devices, certificate pinning), and the inner post-TLS identity all
    /// agree on the peer's device ID and protocol version.
    pub async fn register_connection(
        &self,
        device_id: &str,
        certificate_der: Vec<u8>,
        protocol_version: u8,
        packets: mpsc::Sender<Packet>,
        cancellation: CancellationToken,
        observed_at: u64,
    ) -> Result<DeviceSnapshot, CoreError> {
        let operation = self.device_operation(device_id);
        let _operation = operation.lock().await;

        {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            state.connections.insert(
                device_id.to_owned(),
                Connection {
                    packets,
                    certificate_der,
                    protocol_version,
                    cancellation,
                    peer_addr: None,
                },
            );
        }
        let snapshot = self.mark_device_connected(device_id, observed_at)?;
        self.refresh_trusted_identity(&snapshot).await;
        self.plugins
            .connected(&self.plugin_context(), &snapshot)
            .await;
        Ok(snapshot)
    }

    /// Record the peer's IP address for a registered connection, so an
    /// incoming transfer can later dial the auxiliary payload port the peer
    /// advertises on the same host. Called by the transport layer right
    /// after [`Self::register_connection`]; a no-op if the connection has
    /// since been replaced or removed.
    pub fn set_connection_peer_addr(&self, device_id: &str, addr: SocketAddr) {
        if let Ok(mut state) = self.state.write()
            && let Some(connection) = state.connections.get_mut(device_id)
        {
            connection.peer_addr = Some(addr);
        }
    }

    /// Remove a control channel, fail any pairing session in progress on it,
    /// cancel any transfer in progress with it, and mark the device
    /// unreachable.
    pub async fn unregister_connection(&self, device_id: &str) {
        let operation = self.device_operation(device_id);
        let _operation = operation.lock().await;

        let (had_connection, failed_pairing) = {
            let Ok(mut state) = self.state.write() else {
                return;
            };
            let had_connection = state.connections.remove(device_id).is_some();
            let failed_pairing =
                fail_active_pairing(&mut state, device_id, OperationErrorCode::ConnectionFailed);
            (had_connection, failed_pairing)
        };
        self.transfers.cancel_device(device_id);
        if had_connection {
            self.plugins
                .disconnected(&self.plugin_context(), device_id)
                .await;
            let _ = self.mark_device_disconnected(device_id);
        }
        if let Some(snapshot) = failed_pairing {
            let _ = self.events.publish(EventData::PairingUpdated(snapshot));
        }
    }

    /// Dispatch a packet received on a registered connection.
    ///
    /// `kdeconnect.pair` packets are always handled, independent of pairing
    /// state, since pairing itself establishes trust. Every other packet
    /// type is routed to the plugin that handles it, only if the sending
    /// device is currently paired; unpaired connections cannot trigger any
    /// other behavior. A type no plugin handles is dropped.
    pub async fn handle_peer_packet(&self, device_id: &str, packet: Packet) {
        let operation = self.device_operation(device_id);
        let _operation = operation.lock_owned().await;

        tracing::debug!(device_id, packet_type = %packet.packet_type, "packet received");
        if packet.packet_type == "kdeconnect.pair" {
            let Ok(body) = packet.body_as::<PairingBody>() else {
                tracing::debug!(device_id, "dropping malformed pair packet");
                return;
            };
            // Trust commits and their in-memory effects must finish together,
            // even when the transport cancels the packet dispatcher.
            let core = self.clone();
            let device_id = device_id.to_owned();
            let _ = self
                .mutations
                .spawn(async move {
                    let _operation = _operation;
                    core.handle_pair_body(&device_id, body, unix_seconds())
                        .await;
                })
                .await;
            return;
        }

        let Some(device) = self
            .state
            .read()
            .ok()
            .and_then(|state| state.devices.get(device_id))
            .filter(|device| device.paired)
        else {
            return;
        };

        if let Some(plugin) = self.plugins.for_packet(&packet.packet_type) {
            plugin
                .handle_packet(&self.plugin_context(), &device, &packet)
                .await;
        }
    }

    /// Announce this device on the network now, rather than at the next
    /// interval, so peers answer promptly.
    pub fn announce(&self) -> Result<(), CoreError> {
        self.send_lan_command(LanCommand::AnnounceDiscovery)
    }

    /// Announce this device to one address, for networks where broadcast
    /// discovery doesn't reach the peer. A peer that hears it dials back
    /// over TCP, as it would after a broadcast. Only unicast addresses are
    /// accepted, so this can't be used to spray the identity at a
    /// broadcast or multicast group.
    pub fn announce_to(&self, address: Ipv4Addr) -> Result<(), CoreError> {
        if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
            return Err(CoreError::InvalidDiscoveryAddress);
        }
        self.send_lan_command(LanCommand::AnnounceTo { address })
    }

    fn send_lan_command(&self, command: LanCommand) -> Result<(), CoreError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => CoreError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => CoreError::CommandQueueClosed,
            })
    }

    /// Queue `packet` to a paired, connected device. Refused, with a typed
    /// error, unless the device has advertised the packet's type in its
    /// `incomingCapabilities`.
    pub(super) fn send_to_capable(&self, device_id: &str, packet: Packet) -> Result<(), CoreError> {
        let connection = self.capable_connection(device_id, &packet.packet_type)?;
        connection
            .packets
            .try_send(packet)
            .map_err(|_| CoreError::DeviceNotConnected)
    }

    /// Queue `packet` to every paired, connected device that has advertised
    /// its type, except `except`.
    pub(super) fn broadcast_to_capable(&self, packet: &Packet, except: Option<&str>) {
        let Ok(state) = self.state.read() else {
            return;
        };
        for (device_id, connection) in &state.connections {
            if Some(device_id.as_str()) == except {
                continue;
            }
            let accepts = state.devices.get(device_id).is_some_and(|device| {
                device.paired && device.incoming_capabilities.contains(&packet.packet_type)
            });
            if accepts {
                let _ = connection.packets.try_send(packet.clone());
            }
        }
    }

    /// Whether [`Self::send_to_capable`] would take a packet of
    /// `packet_type` for the device now.
    pub(super) fn check_capable(
        &self,
        device_id: &str,
        packet_type: &str,
    ) -> Result<(), CoreError> {
        self.capable_connection(device_id, packet_type).map(|_| ())
    }

    /// The live connection to `device_id`, provided the device is paired
    /// and has advertised `capability` in its `incomingCapabilities`.
    fn capable_connection(
        &self,
        device_id: &str,
        capability: &str,
    ) -> Result<Connection, CoreError> {
        let state = self.read_state()?;
        let device = state
            .devices
            .get(device_id)
            .ok_or(CoreError::UnknownDevice)?;
        if !device.paired {
            return Err(CoreError::NotPaired);
        }
        if !device
            .incoming_capabilities
            .iter()
            .any(|advertised| advertised == capability)
        {
            return Err(CoreError::UnsupportedByPeer);
        }
        state
            .connections
            .get(device_id)
            .cloned()
            .ok_or(CoreError::DeviceNotConnected)
    }

    /// Payload-connection access to a paired, connected device.
    pub(super) fn payload_peer(&self, device_id: &str) -> Result<PayloadPeer, CoreError> {
        let state = self.read_state()?;
        let device = state
            .devices
            .get(device_id)
            .ok_or(CoreError::UnknownDevice)?;
        if !device.paired {
            return Err(CoreError::NotPaired);
        }
        let connection = state
            .connections
            .get(device_id)
            .ok_or(CoreError::DeviceNotConnected)?;
        let config = self.transfers.config();
        Ok(PayloadPeer::new(
            device_id.to_owned(),
            connection.certificate_der.clone(),
            connection.peer_addr.map(|addr| addr.ip()),
            self.identity.clone(),
            config.payload_bind_ip,
            config.payload_connect_timeout,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::handle;

    #[tokio::test]
    async fn commands_are_bounded_and_observable() {
        let (handle, mut commands) = handle().await;
        handle.announce().unwrap();
        assert!(matches!(
            handle.announce(),
            Err(CoreError::CommandQueueFull)
        ));
        assert_eq!(commands.recv().await, Some(LanCommand::AnnounceDiscovery));
    }

    #[tokio::test]
    async fn announcing_to_an_address_accepts_only_unicast() {
        let (handle, mut commands) = handle().await;
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 251),
        ] {
            assert!(matches!(
                handle.announce_to(address),
                Err(CoreError::InvalidDiscoveryAddress)
            ));
        }
        let address = Ipv4Addr::new(192, 168, 1, 20);
        handle.announce_to(address).unwrap();
        assert_eq!(
            commands.recv().await,
            Some(LanCommand::AnnounceTo { address })
        );
    }
}

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    ops::RangeInclusive,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    config::LocalIdentity,
    core::{Core, LanCommand},
    protocol::{DeviceType, IdentityBody, Packet, PacketCodec},
    store::Store,
    transport::tls::{self, PeerPin, TlsMaterial},
};

pub const DISCOVERY_PORT: u16 = 1716;
/// Where loopback-only discovery ([`LanConfig::loopback`]) listens and
/// announces: the broadcast address of `lo`, so every instance on the host
/// hears every other.
pub const LOOPBACK_BROADCAST: Ipv4Addr = Ipv4Addr::new(127, 255, 255, 255);
const PROTOCOL_VERSION: u8 = 8;
pub const TCP_PORT_RANGE: RangeInclusive<u16> = 1716..=1764;
pub const MAX_DISCOVERY_DATAGRAM: usize = 8 * 1024;
const MAX_IDENTITY_LINE: usize = 8 * 1024;
const MAX_PACKET_LINE: usize = 64 * 1024;
const MAX_PENDING_CONNECTIONS: usize = 32;
const PACKET_QUEUE_CAPACITY: usize = 32;
/// How long a cancelled connection may spend writing out packets that were
/// queued before it was cancelled.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct LanConfig {
    discovery_bind: SocketAddr,
    announcement_targets: Vec<SocketAddr>,
    /// Port a peer added by address is announced to.
    peer_discovery_port: u16,
    tcp_bind_ip: Ipv4Addr,
    tcp_ports: RangeInclusive<u16>,
    /// Keep discovery on loopback: see [`LanConfig::loopback`].
    loopback_only: bool,
    announce_interval: Duration,
    connect_timeout: Duration,
    identity_timeout: Duration,
    shutdown_timeout: Duration,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self {
            discovery_bind: SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                DISCOVERY_PORT,
            )),
            announcement_targets: vec![SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::BROADCAST,
                DISCOVERY_PORT,
            ))],
            peer_discovery_port: DISCOVERY_PORT,
            tcp_bind_ip: Ipv4Addr::UNSPECIFIED,
            tcp_ports: TCP_PORT_RANGE,
            loopback_only: false,
            announce_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            identity_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

impl LanConfig {
    /// Discovery and control connections on loopback only, for running
    /// several instances against each other on one machine. Nothing binds
    /// an address other interfaces receive on, so devices on the network
    /// can neither discover this one nor connect to it, and it announces
    /// only to [`LOOPBACK_BROADCAST`]. A peer added by address is announced
    /// to only if the address is a loopback one, and then by that broadcast.
    ///
    /// Discovery binds the broadcast address itself rather than 127.0.0.1:
    /// a socket bound to 127.0.0.1 doesn't receive datagrams sent to
    /// 127.255.255.255, and the wildcard address would receive them from
    /// every interface.
    pub fn loopback(discovery_port: u16) -> Self {
        let broadcast = SocketAddr::V4(SocketAddrV4::new(LOOPBACK_BROADCAST, discovery_port));
        Self {
            loopback_only: true,
            ..Self::default()
        }
        .with_discovery_bind(broadcast)
        .with_announcement_targets(vec![broadcast])
        .with_peer_discovery_port(discovery_port)
        .with_tcp_bind(Ipv4Addr::LOCALHOST, TCP_PORT_RANGE)
    }

    pub fn with_discovery_bind(mut self, address: SocketAddr) -> Self {
        self.discovery_bind = address;
        self
    }

    pub fn with_announcement_targets(mut self, targets: Vec<SocketAddr>) -> Self {
        self.announcement_targets = targets;
        self
    }

    /// Announce devices added by address on this port instead of 1716.
    /// Tests use it to reach a peer bound to a private port.
    pub fn with_peer_discovery_port(mut self, port: u16) -> Self {
        self.peer_discovery_port = port;
        self
    }

    pub fn with_tcp_bind(mut self, ip: Ipv4Addr, ports: RangeInclusive<u16>) -> Self {
        self.tcp_bind_ip = ip;
        self.tcp_ports = ports;
        self
    }

    pub fn with_announce_interval(mut self, value: Duration) -> Self {
        self.announce_interval = value;
        self
    }

    pub fn with_timeouts(
        mut self,
        connect: Duration,
        identity: Duration,
        shutdown: Duration,
    ) -> Self {
        self.connect_timeout = connect;
        self.identity_timeout = identity;
        self.shutdown_timeout = shutdown;
        self
    }
}

#[derive(Clone, Debug)]
pub struct LocalDeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub device_type: DeviceType,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

pub struct LanService {
    discovery_addr: SocketAddr,
    tcp_addr: SocketAddr,
    cancellation: CancellationToken,
    shutdown_timeout: Duration,
    task: Option<JoinHandle<()>>,
}

impl LanService {
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        config: LanConfig,
        local: LocalDeviceInfo,
        core: Core,
        commands: mpsc::Receiver<LanCommand>,
        identity: Arc<LocalIdentity>,
        store: Store,
        cancellation: CancellationToken,
    ) -> Result<Self, LanError> {
        validate_local(&local)?;
        let udp = match bind_udp(config.discovery_bind) {
            // A system whose loopback interface has no broadcast address
            // can't bind it; stay on loopback, where only announcements to
            // 127.0.0.1 itself arrive.
            Err(LanError::Socket(error))
                if config.loopback_only && error.kind() == std::io::ErrorKind::AddrNotAvailable =>
            {
                debug!(%error, "loopback broadcast unavailable; binding 127.0.0.1");
                let port = config.discovery_bind.port();
                bind_udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)))?
            }
            bound => bound?,
        };
        let udp = Arc::new(udp);
        let discovery_addr = udp.local_addr().map_err(LanError::Socket)?;
        let tcp = bind_tcp(config.tcp_bind_ip, config.tcp_ports.clone()).await?;
        let tcp_addr = tcp.local_addr().map_err(LanError::Socket)?;
        let announcement = Arc::new(encode_identity(&local, tcp_addr.port())?);
        let registry = Arc::new(ConnectionRegistry::default());
        let task_cancellation = cancellation.clone();
        let shutdown_timeout = config.shutdown_timeout;
        let task = tokio::spawn(run(
            config,
            local,
            core,
            commands,
            identity,
            store,
            udp,
            tcp,
            announcement,
            registry,
            task_cancellation,
        ));

        Ok(Self {
            discovery_addr,
            tcp_addr,
            cancellation,
            shutdown_timeout,
            task: Some(task),
        })
    }

    pub fn discovery_addr(&self) -> SocketAddr {
        self.discovery_addr
    }

    pub fn tcp_addr(&self) -> SocketAddr {
        self.tcp_addr
    }

    pub async fn shutdown(mut self) -> Result<(), LanError> {
        self.cancellation.cancel();
        let mut task = self.task.take().expect("LAN task is present");
        match timeout(self.shutdown_timeout, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(LanError::Task(error)),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(LanError::ShutdownDeadline)
            }
        }
    }
}

impl Drop for LanService {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    config: LanConfig,
    mut local: LocalDeviceInfo,
    core: Core,
    mut commands: mpsc::Receiver<LanCommand>,
    identity: Arc<LocalIdentity>,
    store: Store,
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
    mut announcement: Arc<Vec<u8>>,
    registry: Arc<ConnectionRegistry>,
    cancellation: CancellationToken,
) {
    let mut device_name = core.watch_local_device_name();
    let mut device_name_open = true;
    let mut connections = JoinSet::new();
    let connection_limit = Arc::new(Semaphore::new(MAX_PENDING_CONNECTIONS));
    let mut announcements = interval(config.announce_interval);
    announcements.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut commands_open = true;
    let mut datagram = [0_u8; MAX_DISCOVERY_DATAGRAM + 1];

    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = announcements.tick() => {
                announce(&udp, &config.announcement_targets, &announcement).await;
                // Paired devices that aren't connected and were given an
                // address: broadcast doesn't reach them (a tailnet, another
                // subnet), so ask them directly.
                for address in core.fallback_addresses() {
                    announce_to(&udp, &config, &announcement, address).await;
                }
            }
            command = commands.recv(), if commands_open => match command {
                Some(LanCommand::AnnounceDiscovery) => {
                    announce(&udp, &config.announcement_targets, &announcement).await;
                }
                Some(LanCommand::AnnounceTo { address }) => {
                    announce_to(&udp, &config, &announcement, address).await;
                }
                None => commands_open = false,
            },
            // A rename applies to connections made from now on; announcing
            // it right away makes peers (KDE Connect and Ferry alike)
            // pick up the new name and reconnect.
            changed = device_name.changed(), if device_name_open => match changed {
                Ok(()) => {
                    local.device_name = device_name.borrow_and_update().clone();
                    let port = tcp.local_addr().map(|address| address.port());
                    match port.map_err(LanError::Socket).and_then(|port| encode_identity(&local, port)) {
                        Ok(bytes) => {
                            announcement = Arc::new(bytes);
                            announce(&udp, &config.announcement_targets, &announcement).await;
                        }
                        Err(error) => debug!(%error, "could not re-encode identity after rename"),
                    }
                }
                Err(_) => device_name_open = false,
            },
            received = udp.recv_from(&mut datagram) => match received {
                Ok((length, source)) => {
                    if length <= MAX_DISCOVERY_DATAGRAM
                        && let Some(identity_body) = decode_identity(&datagram[..length])
                        && identity_body.device_id != local.device_id
                    {
                        let paired = is_trusted(&store, &identity_body.device_id).await;
                        let _ = core.discover_device(&identity_body, paired, unix_millis());
                        if let Some(port) = tcp_port(&identity_body)
                            && let Ok(permit) = connection_limit.clone().try_acquire_owned()
                            && let Some(reservation) = registry.reserve_outgoing(&local.device_id, &identity_body.device_id)
                        {
                            let address = SocketAddr::new(source.ip(), port);
                            spawn_outgoing(
                                &mut connections, address, identity_body.clone(), reservation,
                                permit, local.clone(), core.clone(), identity.clone(), store.clone(),
                                announcement.clone(), registry.clone(), cancellation.clone(),
                                config.connect_timeout, config.identity_timeout,
                            );
                        }
                    }
                }
                Err(error) => {
                    debug!(%error, "LAN discovery receive failed");
                    sleep(Duration::from_millis(100)).await;
                }
            },
            accepted = tcp.accept() => match accepted {
                Ok((stream, _)) => {
                    if let Ok(permit) = connection_limit.clone().try_acquire_owned() {
                        spawn_incoming(
                            &mut connections, stream, permit, local.clone(), core.clone(),
                            identity.clone(), store.clone(), announcement.clone(), registry.clone(),
                            cancellation.clone(), config.identity_timeout,
                        );
                    }
                }
                Err(error) => {
                    debug!(%error, "LAN TCP accept failed");
                    sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    registry.cancel_all();
    connections.shutdown().await;
}

/// Send the identity to one address, on the peer's discovery port. In
/// loopback-only mode only a loopback address is announced to, and it goes
/// to the loopback broadcast address, which is where such peers listen.
async fn announce_to(udp: &UdpSocket, config: &LanConfig, announcement: &[u8], address: Ipv4Addr) {
    let target = if !config.loopback_only {
        address
    } else if address.is_loopback() {
        LOOPBACK_BROADCAST
    } else {
        debug!(%address, "not announcing off loopback in loopback-only mode");
        return;
    };
    let target = SocketAddr::V4(SocketAddrV4::new(target, config.peer_discovery_port));
    announce(udp, &[target], announcement).await;
}

async fn is_trusted(store: &Store, device_id: &str) -> bool {
    matches!(store.device(device_id).await, Ok(Some(_)))
}

async fn announce(socket: &UdpSocket, targets: &[SocketAddr], announcement: &[u8]) {
    for target in targets {
        if let Err(error) = socket.send_to(announcement, target).await {
            debug!(%target, %error, "LAN identity announcement failed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_outgoing(
    connections: &mut JoinSet<()>,
    address: SocketAddr,
    discovered: IdentityBody,
    reservation: Reservation,
    permit: OwnedSemaphorePermit,
    local: LocalDeviceInfo,
    core: Core,
    identity: Arc<LocalIdentity>,
    store: Store,
    announcement: Arc<Vec<u8>>,
    registry: Arc<ConnectionRegistry>,
    shutdown: CancellationToken,
    connect_deadline: Duration,
    identity_deadline: Duration,
) {
    connections.spawn(async move {
        let _permit = permit;
        let expected_device_id = discovered.device_id.clone();
        debug!(local_id = %local.device_id, %address, %expected_device_id, "dialing outgoing connection");
        let result = timeout(connect_deadline, TcpStream::connect(address)).await;
        if let Ok(Ok(stream)) = result {
            handle_connection(
                stream,
                Some(discovered),
                reservation,
                local,
                core.clone(),
                identity,
                store,
                announcement,
                registry.clone(),
                shutdown,
                identity_deadline,
            ).await;
        } else if registry.release(&expected_device_id, reservation.id) {
            let _ = core.mark_device_disconnected(&expected_device_id);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_incoming(
    connections: &mut JoinSet<()>,
    stream: TcpStream,
    permit: OwnedSemaphorePermit,
    local: LocalDeviceInfo,
    core: Core,
    identity: Arc<LocalIdentity>,
    store: Store,
    announcement: Arc<Vec<u8>>,
    registry: Arc<ConnectionRegistry>,
    shutdown: CancellationToken,
    identity_deadline: Duration,
) {
    connections.spawn(async move {
        let _permit = permit;
        handle_connection(
            stream,
            None,
            // A placeholder reservation; handle_connection re-reserves once
            // the peer's pre-TLS device ID is known for incoming links.
            Reservation {
                id: 0,
                cancellation: CancellationToken::new(),
            },
            local,
            core,
            identity,
            store,
            announcement,
            registry,
            shutdown,
            identity_deadline,
        )
        .await;
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// This side received the peer's UDP announcement and dialed its TCP
    /// port. It sends its identity in plaintext and acts as the TLS server.
    Dialer,
    /// This side accepted the TCP connection. It only reads the dialer's
    /// plaintext identity and acts as the TLS client.
    Acceptor,
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    mut stream: TcpStream,
    discovered: Option<IdentityBody>,
    reservation: Reservation,
    local: LocalDeviceInfo,
    core: Core,
    identity: Arc<LocalIdentity>,
    store: Store,
    announcement: Arc<Vec<u8>>,
    registry: Arc<ConnectionRegistry>,
    shutdown: CancellationToken,
    identity_deadline: Duration,
) {
    let role = if discovered.is_some() {
        Role::Dialer
    } else {
        Role::Acceptor
    };
    let peer_key = discovered
        .as_ref()
        .map(|identity| identity.device_id.clone())
        .unwrap_or_default();
    let peer_addr = stream.peer_addr().ok();
    // The plaintext identity exchange is one-way, as in KDE Connect: the
    // dialer already knows the acceptor's identity from its UDP announcement,
    // so only the dialer sends one, addressed to the acceptor.
    let pre_tls_identity = match discovered {
        Some(discovered) => {
            let sent = match encode_dial_identity(&local, &discovered) {
                Ok(bytes) => send_identity(&mut stream, &bytes, identity_deadline).await,
                Err(error) => Err(error),
            };
            sent.map(|()| discovered)
        }
        None => receive_dial_identity(&mut stream, &local.device_id, identity_deadline).await,
    };
    let pre_tls_identity = match pre_tls_identity {
        Ok(identity) if identity.device_id != local.device_id => identity,
        result => {
            if let Err(error) = result {
                debug!(local_id = %local.device_id, ?role, %error, "plaintext identity exchange failed");
            }
            if !peer_key.is_empty() {
                registry.release(&peer_key, reservation.id);
            }
            return;
        }
    };

    // Incoming connections only learn the peer's device ID from the
    // plaintext identity exchange, so the connection slot is reserved here
    // instead of before dialing.
    let reservation = if role == Role::Acceptor {
        match registry.reserve_incoming(&local.device_id, &pre_tls_identity.device_id) {
            Some(reservation) => reservation,
            None => return,
        }
    } else {
        reservation
    };
    let device_id = pre_tls_identity.device_id.clone();

    let trusted = match store.device(&device_id).await {
        Ok(trusted) => trusted,
        Err(error) => {
            debug!(%device_id, %error, "could not load peer trust");
            registry.release(&device_id, reservation.id);
            return;
        }
    };
    let pin = match &trusted {
        Some(trusted_device) => PeerPin::Pinned(trusted_device.certificate_der.clone()),
        None => PeerPin::Unpinned,
    };
    let material = TlsMaterial::new(identity.certificate_der(), identity.private_key_der());

    let result = match role {
        Role::Acceptor => {
            establish_client_session(
                stream,
                &material,
                &device_id,
                pin,
                &announcement,
                identity_deadline,
            )
            .await
        }
        Role::Dialer => {
            establish_server_session(
                stream,
                &material,
                &device_id,
                pin,
                &announcement,
                identity_deadline,
            )
            .await
        }
    };
    let (mut reader, mut writer, peer_certificate_der, inner_identity) = match result {
        Ok(value) => value,
        Err(error) => {
            debug!(local_id = %local.device_id, %device_id, %error, "TLS session establishment failed");
            registry.release(&device_id, reservation.id);
            return;
        }
    };

    // The device ID and protocol version must be identical before and
    // after the TLS upgrade, or a person-in-the-middle could swap identity
    // mid-handshake or force a protocol downgrade.
    if inner_identity.device_id != pre_tls_identity.device_id
        || inner_identity.protocol_version != pre_tls_identity.protocol_version
    {
        registry.release(&device_id, reservation.id);
        return;
    }
    if let Some(trusted_device) = &trusted
        && inner_identity.protocol_version < trusted_device.last_trusted_protocol_version
    {
        registry.release(&device_id, reservation.id);
        return;
    }

    if !registry.establish(&device_id, reservation.id) {
        registry.release(&device_id, reservation.id);
        return;
    }

    // The peer may not have been independently discovered through a UDP
    // announcement yet (for example, an incoming connection can race ahead
    // of the discovery broadcast in the same process), so ensure the device
    // registry has an entry before registering the connection against it.
    let paired = trusted.is_some();
    if core
        .discover_device(&pre_tls_identity, paired, unix_millis())
        .is_err()
    {
        registry.release(&device_id, reservation.id);
        return;
    }

    let (packet_tx, mut packet_rx) = mpsc::channel::<Packet>(PACKET_QUEUE_CAPACITY);
    let _keep_alive = packet_tx.clone();
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<Packet>(PACKET_QUEUE_CAPACITY);
    // Reading, writing and ordered callback dispatch have independent
    // futures. A full dispatch queue backpressures only the reader.
    {
        let read_packets = async move {
            let mut codec = PacketCodec::new(MAX_PACKET_LINE);
            let mut buffer = [0_u8; 4096];
            loop {
                let length = match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(length) => length,
                };
                let packets = match codec.decode(&buffer[..length]) {
                    Ok(packets) => packets,
                    Err(_) => break,
                };
                for packet in packets {
                    if incoming_tx.send(packet).await.is_err() {
                        return;
                    }
                }
            }
            // EOF can follow a final unpair packet in the same read. Close
            // the queue so dispatch can drain it; bound the grace period
            // for a callback that never finishes. Shutdown still preempts it.
            drop(incoming_tx);
            tokio::time::sleep(CLOSE_FLUSH_TIMEOUT).await;
        };
        let write_packets = async {
            let codec = PacketCodec::new(MAX_PACKET_LINE);
            while let Some(packet) = packet_rx.recv().await {
                let bytes = match codec.encode(&packet) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        debug!(%error, "failed to encode outgoing packet");
                        continue;
                    }
                };
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        };
        let dispatch = async {
            if let Err(error) = core
                .register_connection(
                    &device_id,
                    peer_certificate_der,
                    inner_identity.protocol_version,
                    packet_tx,
                    reservation.cancellation.clone(),
                    unix_millis(),
                )
                .await
            {
                debug!(%device_id, %error, "register_connection failed");
                return;
            }
            if let Some(peer_addr) = peer_addr {
                core.set_connection_peer_addr(&device_id, peer_addr);
            }
            while let Some(packet) = incoming_rx.recv().await {
                core.handle_peer_packet(&device_id, packet).await;
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => {},
            _ = reservation.cancellation.cancelled() => {},
            _ = read_packets => {},
            _ = write_packets => {},
            _ = dispatch => {},
        }
        // Dropping dispatch cancels the current callback before cleanup.
    }
    if reservation.cancellation.is_cancelled() && !shutdown.is_cancelled() {
        let mut codec = PacketCodec::new(MAX_PACKET_LINE);
        let _ = timeout(
            CLOSE_FLUSH_TIMEOUT,
            flush_queued(&mut packet_rx, &mut codec, &mut writer),
        )
        .await;
    }
    core.unregister_current_connection(&device_id, &reservation.cancellation)
        .await;
    registry.release(&device_id, reservation.id);
}

/// Write every packet already waiting in `packet_rx`, then flush.
async fn flush_queued(
    packet_rx: &mut mpsc::Receiver<Packet>,
    codec: &mut PacketCodec,
    writer: &mut BoxedWriter,
) -> std::io::Result<()> {
    while let Ok(packet) = packet_rx.try_recv() {
        if let Ok(bytes) = codec.encode(&packet) {
            writer.write_all(&bytes).await?;
        }
    }
    writer.flush().await
}

type BoxedReader = Box<dyn AsyncRead + Send + Unpin>;
type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;

async fn establish_client_session(
    stream: TcpStream,
    material: &TlsMaterial,
    expected_device_id: &str,
    pin: PeerPin,
    announcement: &[u8],
    identity_deadline: Duration,
) -> Result<(BoxedReader, BoxedWriter, Vec<u8>, IdentityBody), LanError> {
    let mut tls_stream = tls::connect(stream, material, expected_device_id, pin)
        .await
        .map_err(LanError::Tls)?;
    let peer_certificate_der = tls::client_peer_certificate(&tls_stream).map_err(LanError::Tls)?;
    let inner_identity =
        exchange_identity_over(&mut tls_stream, announcement, identity_deadline).await?;
    let (read_half, write_half) = tokio::io::split(tls_stream);
    Ok((
        Box::new(read_half),
        Box::new(write_half),
        peer_certificate_der,
        inner_identity,
    ))
}

async fn establish_server_session(
    stream: TcpStream,
    material: &TlsMaterial,
    expected_device_id: &str,
    pin: PeerPin,
    announcement: &[u8],
    identity_deadline: Duration,
) -> Result<(BoxedReader, BoxedWriter, Vec<u8>, IdentityBody), LanError> {
    let mut tls_stream = tls::accept(stream, material, expected_device_id, pin)
        .await
        .map_err(LanError::Tls)?;
    let peer_certificate_der = tls::server_peer_certificate(&tls_stream).map_err(LanError::Tls)?;
    let inner_identity =
        exchange_identity_over(&mut tls_stream, announcement, identity_deadline).await?;
    let (read_half, write_half) = tokio::io::split(tls_stream);
    Ok((
        Box::new(read_half),
        Box::new(write_half),
        peer_certificate_der,
        inner_identity,
    ))
}

/// Send this device's identity and read the peer's, as both sides do inside
/// TLS.
async fn exchange_identity_over<S>(
    stream: &mut S,
    announcement: &[u8],
    deadline: Duration,
) -> Result<IdentityBody, LanError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_identity(stream, announcement, deadline).await?;
    receive_identity(stream, deadline).await
}

async fn send_identity<S>(
    stream: &mut S,
    identity: &[u8],
    deadline: Duration,
) -> Result<(), LanError>
where
    S: AsyncWrite + Unpin,
{
    timeout(deadline, stream.write_all(identity))
        .await
        .map_err(|_| LanError::IdentityTimeout)?
        .map_err(LanError::Socket)
}

/// Read the dialer's plaintext identity, rejecting one addressed to another
/// device or protocol version.
async fn receive_dial_identity(
    stream: &mut TcpStream,
    local_device_id: &str,
    deadline: Duration,
) -> Result<IdentityBody, LanError> {
    let identity = receive_identity(stream, deadline).await?;
    let target_device_id = identity.extra.get("targetDeviceId");
    if target_device_id.is_some_and(|target| target.as_str() != Some(local_device_id)) {
        return Err(LanError::InvalidIdentity);
    }
    // KDE Connect Android sends the version as a string, so accept either.
    let target_version = identity.extra.get("targetProtocolVersion");
    if target_version.is_some_and(|target| {
        target.as_u64() != Some(PROTOCOL_VERSION.into())
            && target.as_str() != Some(PROTOCOL_VERSION.to_string().as_str())
    }) {
        return Err(LanError::InvalidIdentity);
    }
    Ok(identity)
}

async fn receive_identity<S>(stream: &mut S, deadline: Duration) -> Result<IdentityBody, LanError>
where
    S: AsyncRead + Unpin,
{
    timeout(deadline, async {
        let mut codec = PacketCodec::new(MAX_IDENTITY_LINE);
        let mut buffer = [0_u8; 2048];
        loop {
            let length = stream.read(&mut buffer).await.map_err(LanError::Socket)?;
            if length == 0 {
                return Err(LanError::ConnectionClosed);
            }
            let packets = codec
                .decode(&buffer[..length])
                .map_err(|_| LanError::InvalidIdentity)?;
            if packets.is_empty() {
                continue;
            }
            if packets.len() != 1 || codec.buffered_len() != 0 {
                return Err(LanError::InvalidIdentity);
            }
            let identity = packet_identity(packets.into_iter().next().expect("one packet"))?;
            return Ok(identity);
        }
    })
    .await
    .map_err(|_| LanError::IdentityTimeout)?
}

fn decode_identity(datagram: &[u8]) -> Option<IdentityBody> {
    let mut codec = PacketCodec::new(MAX_DISCOVERY_DATAGRAM);
    let packets = codec.decode(datagram).ok()?;
    if packets.len() != 1 || codec.buffered_len() != 0 {
        return None;
    }
    packet_identity(packets.into_iter().next()?)
        .ok()
        .filter(|identity| tcp_port(identity).is_some())
}

fn packet_identity(packet: Packet) -> Result<IdentityBody, LanError> {
    if packet.packet_type != "kdeconnect.identity" {
        return Err(LanError::InvalidIdentity);
    }
    let identity: IdentityBody = packet.body_as().map_err(|_| LanError::InvalidIdentity)?;
    identity.validate().map_err(|_| LanError::InvalidIdentity)?;
    if identity.protocol_version != PROTOCOL_VERSION {
        return Err(LanError::InvalidIdentity);
    }
    Ok(identity)
}

fn tcp_port(identity: &IdentityBody) -> Option<u16> {
    identity
        .extra
        .get("tcpPort")?
        .as_u64()?
        .try_into()
        .ok()
        .filter(|port| TCP_PORT_RANGE.contains(port))
}

fn encode_identity(local: &LocalDeviceInfo, tcp_port: u16) -> Result<Vec<u8>, LanError> {
    let mut extra = Map::new();
    extra.insert("tcpPort".into(), json!(tcp_port));
    encode_identity_with(local, extra)
}

/// The plaintext identity a dialer sends, addressed to the discovered peer.
fn encode_dial_identity(local: &LocalDeviceInfo, peer: &IdentityBody) -> Result<Vec<u8>, LanError> {
    let mut extra = Map::new();
    extra.insert("targetDeviceId".into(), json!(peer.device_id));
    extra.insert("targetProtocolVersion".into(), json!(peer.protocol_version));
    encode_identity_with(local, extra)
}

fn encode_identity_with(
    local: &LocalDeviceInfo,
    extra: Map<String, serde_json::Value>,
) -> Result<Vec<u8>, LanError> {
    let identity = IdentityBody {
        device_id: local.device_id.clone(),
        device_name: local.device_name.clone(),
        device_type: local.device_type,
        incoming_capabilities: local.incoming_capabilities.clone(),
        outgoing_capabilities: local.outgoing_capabilities.clone(),
        protocol_version: PROTOCOL_VERSION,
        extra,
    };
    identity
        .validate()
        .map_err(|_| LanError::InvalidLocalIdentity)?;
    let packet = Packet::from_body(timestamp_number(), "kdeconnect.identity", &identity)
        .map_err(|_| LanError::InvalidLocalIdentity)?;
    PacketCodec::new(MAX_IDENTITY_LINE)
        .encode(&packet)
        .map_err(|_| LanError::InvalidLocalIdentity)
}

fn timestamp_number() -> serde_json::Number {
    serde_json::Number::from(unix_millis())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn validate_local(local: &LocalDeviceInfo) -> Result<(), LanError> {
    let identity = IdentityBody {
        device_id: local.device_id.clone(),
        device_name: local.device_name.clone(),
        device_type: local.device_type,
        incoming_capabilities: local.incoming_capabilities.clone(),
        outgoing_capabilities: local.outgoing_capabilities.clone(),
        protocol_version: 8,
        extra: Map::from_iter([("tcpPort".into(), Value::from(1))]),
    };
    identity
        .validate()
        .map_err(|_| LanError::InvalidLocalIdentity)
}

fn bind_udp(address: SocketAddr) -> Result<UdpSocket, LanError> {
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(LanError::Socket)?;
    socket.set_reuse_address(true).map_err(LanError::Socket)?;
    // Without SO_REUSEPORT, the kernel delivers each inbound discovery
    // datagram to only one of several sockets sharing this port (whichever
    // bound it "wins"), so a second local instance would never receive
    // announcements. SO_REUSEPORT makes the kernel fan broadcasts out to
    // every listener instead, letting multiple local instances (and the
    // real KDE Connect daemon) discover each other on the shared port.
    #[cfg(unix)]
    socket.set_reuse_port(true).map_err(LanError::Socket)?;
    socket.set_broadcast(true).map_err(LanError::Socket)?;
    socket
        .bind(&SockAddr::from(address))
        .map_err(LanError::Socket)?;
    socket.set_nonblocking(true).map_err(LanError::Socket)?;
    UdpSocket::from_std(socket.into()).map_err(LanError::Socket)
}

async fn bind_tcp(ip: Ipv4Addr, ports: RangeInclusive<u16>) -> Result<TcpListener, LanError> {
    bind_listener(ip, ports)
        .await
        .map_err(LanError::Socket)?
        .ok_or(LanError::NoTcpPort)
}

/// Listen on `ip` at the first port in `ports` that no other socket holds,
/// on `ip` or on an address overlapping it (the wildcard address for a
/// specific one, 127.0.0.1 for the wildcard). `None` if every port is taken.
///
/// With `SO_REUSEADDR`, which tokio sets, macOS lets a listener on
/// 127.0.0.1 share a port with another process's on the wildcard address
/// (a running Ferry or KDE Connect on 1716), and the other way round; dials
/// to 127.0.0.1 then reach the more specific listener, not necessarily the
/// one that announced the port. Linux refuses both binds already.
pub(crate) async fn bind_listener(
    ip: Ipv4Addr,
    ports: RangeInclusive<u16>,
) -> std::io::Result<Option<TcpListener>> {
    for port in ports {
        let listener = match TcpListener::bind(SocketAddrV4::new(ip, port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error),
        };
        if overlapping_address_bound(ip, port)? {
            debug!(%ip, port, "port shared with an overlapping address; skipping");
            continue;
        }
        return Ok(Some(listener));
    }
    Ok(None)
}

/// Whether another socket is bound to `port` on the address overlapping
/// `ip`. The probe is bound after this process's listener, so of two
/// processes binding overlapping addresses at once, at least one sees the
/// other. `SO_REUSEADDR` keeps connections in TIME_WAIT from counting.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn overlapping_address_bound(ip: Ipv4Addr, port: u16) -> std::io::Result<bool> {
    let overlapping = if ip.is_unspecified() {
        Ipv4Addr::LOCALHOST
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    let probe = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    probe.set_reuse_address(true)?;
    match probe.bind(&SockAddr::from(SocketAddrV4::new(overlapping, port))) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Ok(true),
        Err(error) => Err(error),
    }
}

/// Linux doesn't let listeners share a port across overlapping addresses.
/// Windows is unchecked.
#[cfg(not(all(unix, not(any(target_os = "linux", target_os = "android")))))]
fn overlapping_address_bound(_ip: Ipv4Addr, _port: u16) -> std::io::Result<bool> {
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Incoming,
    Outgoing,
}

struct ConnectionEntry {
    id: u64,
    direction: Direction,
    established: bool,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct Reservation {
    id: u64,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct ConnectionRegistry {
    next_id: AtomicU64,
    entries: Mutex<BTreeMap<String, ConnectionEntry>>,
}

impl ConnectionRegistry {
    fn reserve_outgoing(&self, local_id: &str, peer_id: &str) -> Option<Reservation> {
        self.reserve(local_id, peer_id, Direction::Outgoing)
    }

    fn reserve_incoming(&self, local_id: &str, peer_id: &str) -> Option<Reservation> {
        self.reserve(local_id, peer_id, Direction::Incoming)
    }

    fn reserve(&self, local_id: &str, peer_id: &str, direction: Direction) -> Option<Reservation> {
        let preferred = if local_id < peer_id {
            Direction::Outgoing
        } else {
            Direction::Incoming
        };
        let mut entries = self.entries.lock().ok()?;
        if let Some(existing) = entries.get(peer_id) {
            if existing.established || existing.direction == preferred || direction != preferred {
                return None;
            }
            existing.cancellation.cancel();
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        entries.insert(
            peer_id.to_owned(),
            ConnectionEntry {
                id,
                direction,
                established: false,
                cancellation: cancellation.clone(),
            },
        );
        Some(Reservation { id, cancellation })
    }

    /// Tie-breaking applies to competing handshakes, never to a live
    /// authenticated connection whose queued actions must not be lost.
    fn establish(&self, peer_id: &str, id: u64) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let Some(entry) = entries.get_mut(peer_id) else {
            return false;
        };
        if entry.id != id || entry.cancellation.is_cancelled() {
            return false;
        }
        entry.established = true;
        true
    }

    fn release(&self, peer_id: &str, id: u64) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        if entries.get(peer_id).is_some_and(|entry| entry.id == id) {
            entries.remove(peer_id);
            true
        } else {
            false
        }
    }

    fn cancel_all(&self) {
        if let Ok(entries) = self.entries.lock() {
            for entry in entries.values() {
                entry.cancellation.cancel();
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum LanError {
    #[error("LAN socket operation failed")]
    Socket(#[source] std::io::Error),
    #[error("no KDE Connect TCP port is available")]
    NoTcpPort,
    #[error("local identity is invalid")]
    InvalidLocalIdentity,
    #[error("peer identity is invalid")]
    InvalidIdentity,
    #[error("peer closed before sending its identity")]
    ConnectionClosed,
    #[error("peer identity exchange timed out")]
    IdentityTimeout,
    #[error("TLS handshake or certificate verification failed")]
    Tls(#[source] tls::TlsError),
    #[error("LAN service task failed")]
    Task(#[source] tokio::task::JoinError),
    #[error("LAN service did not shut down before its deadline")]
    ShutdownDeadline,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tie_breaking_replaces_pending_handshakes_but_preserves_live_connections() {
        let registry = ConnectionRegistry::default();
        let pending = registry.reserve_incoming("a", "b").unwrap();
        let preferred = registry.reserve_outgoing("a", "b").unwrap();
        assert!(pending.cancellation.is_cancelled());
        assert!(!registry.establish("b", pending.id));
        assert!(registry.establish("b", preferred.id));

        let registry = ConnectionRegistry::default();
        let live = registry.reserve_incoming("a", "b").unwrap();
        assert!(registry.establish("b", live.id));
        assert!(registry.reserve_outgoing("a", "b").is_none());
        assert!(!live.cancellation.is_cancelled());
    }
}

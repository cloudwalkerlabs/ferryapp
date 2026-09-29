//! A stand-in for KDE Connect for Android, for exercising file browsing
//! without a phone.
//!
//! It speaks just enough of the protocol: it hears a desktop's UDP
//! announcement, dials it the way Android does (TCP dialer, TLS server),
//! accepts any pairing request, and answers `kdeconnect.sftp.request` with
//! an offer for its own SFTP server. That server behaves like Android's:
//! its host key is the phone's TLS key, it accepts the paired desktop's TLS
//! key or the one-off password, and it serves a directory on disk as the
//! phone's storage. It also shares notifications as Android does, when a
//! test posts one, and records what the desktop asks of them.

#![allow(dead_code)]

use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ferry::{
    config::LocalIdentity,
    plugins::{
        battery::PACKET_TYPE as BATTERY_PACKET_TYPE,
        browse::{PACKET_TYPE as SFTP_PACKET_TYPE, REQUEST_PACKET_TYPE},
        connectivity::PACKET_TYPE as CONNECTIVITY_PACKET_TYPE,
        notifications::{
            ACTION_PACKET_TYPE as NOTIFICATION_ACTION_PACKET_TYPE,
            PACKET_TYPE as NOTIFICATION_PACKET_TYPE,
            REPLY_PACKET_TYPE as NOTIFICATION_REPLY_PACKET_TYPE,
            REQUEST_PACKET_TYPE as NOTIFICATION_REQUEST_PACKET_TYPE,
        },
    },
    protocol::{DeviceType, IdentityBody, Packet, PacketCodec},
    store::Store,
    transport::tls::{self, PeerPin, TlsMaterial},
};
use russh::{
    Channel, ChannelId,
    keys::{PrivateKey, pkcs8::decode_pkcs8, ssh_key},
    server::{Auth, Msg, Session},
};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use serde_json::{Map, Value, json};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub const PHONE_NAME: &str = "Fake Phone";

/// How the phone answers a request to browse its files.
#[derive(Clone, Debug)]
pub enum BrowseReply {
    /// Offer these storage roots, as `(path, name)` pairs.
    Serve(Vec<(String, String)>),
    /// Refuse with this `errorMessage`, as Android does without permission.
    Refuse(String),
}

pub struct FakePhoneConfig {
    /// The name the phone announces; the tests use [`PHONE_NAME`].
    pub name: String,
    /// Holds the phone's identity.
    pub data_dir: PathBuf,
    /// Served as `/` over SFTP.
    pub storage: PathBuf,
    pub reply: BrowseReply,
    /// Serve SFTP with a key other than the phone's TLS key, as an
    /// impostor would.
    pub wrong_host_key: bool,
    /// Only connect to the desktop with this device id.
    pub desktop_id: Option<String>,
    /// Where to listen for the desktop's discovery announcements.
    pub discovery_bind: SocketAddr,
}

/// What the phone saw, for assertions.
#[derive(Default)]
pub struct PhoneLog {
    pub browse_requests: AtomicUsize,
    pub key_logins: AtomicUsize,
    pub password_logins: AtomicUsize,
    pub sftp_sessions: AtomicUsize,
    /// SSH connections currently open.
    pub open_connections: AtomicUsize,
    /// The `kdeconnect.notification.*` packets the desktop sent.
    pub notification_packets: Mutex<Vec<Packet>>,
    /// Icons the desktop fetched.
    pub icons_served: AtomicUsize,
}

struct Shared {
    name: String,
    config_reply: BrowseReply,
    desktop_certificate: Mutex<Option<Vec<u8>>>,
    password: Mutex<Option<String>>,
    packets: Mutex<Option<mpsc::Sender<Packet>>>,
    sftp_port: u16,
    log: Arc<PhoneLog>,
    identity: Arc<LocalIdentity>,
    /// The desktop connected to, once it is.
    desktop_id: Mutex<Option<String>>,
}

pub struct FakePhone {
    pub device_id: String,
    pub log: Arc<PhoneLog>,
    discovery_addr: SocketAddr,
    shared: Arc<Shared>,
    cancellation: CancellationToken,
    tasks: JoinSet<()>,
}

impl FakePhone {
    pub async fn start(config: FakePhoneConfig) -> Self {
        let store = Store::open(&config.data_dir).await.unwrap();
        let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
        let device_id = identity.device_id().to_owned();
        let host_key = if config.wrong_host_key {
            let impostor = LocalIdentity::load_or_create(&Store::open_in_memory().await.unwrap())
                .await
                .unwrap();
            decode_pkcs8(impostor.private_key_der(), None).unwrap()
        } else {
            decode_pkcs8(identity.private_key_der(), None).unwrap()
        };

        let sftp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let log = Arc::new(PhoneLog::default());
        let shared = Arc::new(Shared {
            name: config.name,
            config_reply: config.reply,
            desktop_certificate: Mutex::new(None),
            password: Mutex::new(None),
            packets: Mutex::new(None),
            sftp_port: sftp_listener.local_addr().unwrap().port(),
            log: log.clone(),
            identity: identity.clone(),
            desktop_id: Mutex::new(None),
        });
        let udp = bind_discovery(config.discovery_bind);
        let discovery_addr = udp.local_addr().unwrap();
        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(serve_sftp(
            sftp_listener,
            host_key,
            config.storage,
            shared.clone(),
            cancellation.clone(),
        ));
        tasks.spawn(connect_to_desktop(
            udp,
            identity,
            config.desktop_id,
            shared.clone(),
            cancellation.clone(),
        ));
        Self {
            device_id,
            log,
            discovery_addr,
            shared,
            cancellation,
            tasks,
        }
    }

    /// Where the phone listens for discovery announcements.
    pub fn discovery_addr(&self) -> SocketAddr {
        self.discovery_addr
    }

    /// Send a packet to the connected desktop.
    pub async fn send(&self, packet: Packet) {
        let sender = self.shared.packets.lock().unwrap().clone();
        sender
            .expect("connected to a desktop")
            .send(packet)
            .await
            .unwrap();
    }

    /// Tell the desktop the SFTP server stopped, as Android does when its
    /// plugin reloads.
    pub async fn announce_server_stopped(&self) {
        self.send(
            Packet::from_body(0_u64, SFTP_PACKET_TYPE, &json!({"serverRunning": false})).unwrap(),
        )
        .await;
    }

    /// Report the battery, as Android does when it changes.
    pub async fn report_battery(&self, charge: i64, charging: bool) {
        self.send(battery_report(charge, charging)).await;
    }

    /// Report one SIM's mobile signal, as Android does when it changes.
    pub async fn report_connectivity(&self, network_type: &str, signal_strength: i64) {
        self.send(connectivity_report(network_type, signal_strength))
            .await;
    }

    /// Post (or update) a notification, as Android does: `body` is the
    /// packet's body, and `icon`, if any, is offered as its payload on a
    /// port of its own, served once over TLS.
    pub async fn post_notification(&self, body: Value, icon: Option<Vec<u8>>) {
        let mut packet = Packet::from_body(0_u64, NOTIFICATION_PACKET_TYPE, &body).unwrap();
        if let Some(icon) = icon {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            packet.payload_size = Some(icon.len() as i64);
            packet.payload_transfer_info = Some(json!({"port": port}).as_object().unwrap().clone());
            let shared = self.shared.clone();
            let cancellation = self.cancellation.clone();
            tokio::spawn(async move {
                let accepted = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { return };
                let desktop_id = shared.desktop_id.lock().unwrap().clone().unwrap();
                let material = TlsMaterial::new(
                    shared.identity.certificate_der(),
                    shared.identity.private_key_der(),
                );
                let Ok(mut stream) =
                    tls::accept(stream, &material, &desktop_id, PeerPin::Unpinned).await
                else {
                    return;
                };
                if stream.write_all(&icon).await.is_ok() {
                    let _ = stream.shutdown().await;
                    shared.log.icons_served.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        self.send(packet).await;
    }

    pub async fn stop(mut self) {
        self.cancellation.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

impl Drop for FakePhone {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn bind_discovery(address: SocketAddr) -> UdpSocket {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    socket.set_reuse_address(true).unwrap();
    #[cfg(unix)]
    socket.set_reuse_port(true).unwrap();
    socket.set_broadcast(true).unwrap();
    socket.bind(&SockAddr::from(address)).unwrap();
    socket.set_nonblocking(true).unwrap();
    UdpSocket::from_std(socket.into()).unwrap()
}

fn identity_packet(device_id: &str, name: &str, extra: Map<String, Value>) -> Vec<u8> {
    let identity = IdentityBody {
        device_id: device_id.into(),
        device_name: name.into(),
        device_type: DeviceType::Phone,
        incoming_capabilities: vec![
            REQUEST_PACKET_TYPE.into(),
            "kdeconnect.ping".into(),
            "kdeconnect.findmyphone.request".into(),
            NOTIFICATION_REQUEST_PACKET_TYPE.into(),
            NOTIFICATION_REPLY_PACKET_TYPE.into(),
            NOTIFICATION_ACTION_PACKET_TYPE.into(),
        ],
        outgoing_capabilities: vec![
            SFTP_PACKET_TYPE.into(),
            "kdeconnect.ping".into(),
            BATTERY_PACKET_TYPE.into(),
            CONNECTIVITY_PACKET_TYPE.into(),
            NOTIFICATION_PACKET_TYPE.into(),
        ],
        protocol_version: 8,
        extra,
    };
    let mut line = PacketCodec::new(64 * 1024)
        .encode(&Packet::from_body(0_u64, "kdeconnect.identity", &identity).unwrap())
        .unwrap();
    if !line.ends_with(b"\n") {
        line.push(b'\n');
    }
    line
}

/// Wait for the desktop's announcement, then dial it and exchange packets
/// until the connection ends or the phone stops.
async fn connect_to_desktop(
    udp: UdpSocket,
    identity: Arc<LocalIdentity>,
    desktop_id: Option<String>,
    shared: Arc<Shared>,
    cancellation: CancellationToken,
) {
    let mut datagram = vec![0_u8; 8 * 1024];
    let (desktop_addr, desktop_id) = loop {
        let received = tokio::select! {
            _ = cancellation.cancelled() => return,
            received = udp.recv_from(&mut datagram) => received,
        };
        let Ok((length, from)) = received else {
            continue;
        };
        let Ok(announced) = serde_json::from_slice::<Value>(&datagram[..length]) else {
            continue;
        };
        let body = &announced["body"];
        let (Some(id), Some(port)) = (body["deviceId"].as_str(), body["tcpPort"].as_u64()) else {
            continue;
        };
        if id == identity.device_id() || desktop_id.as_deref().is_some_and(|wanted| wanted != id) {
            continue;
        }
        break (SocketAddr::new(from.ip(), port as u16), id.to_owned());
    };
    eprintln!("fake phone: dialing {desktop_id} at {desktop_addr}");

    let mut stream = TcpStream::connect(desktop_addr).await.unwrap();
    let mut dial_extra = Map::new();
    dial_extra.insert("targetDeviceId".into(), json!(desktop_id));
    dial_extra.insert("targetProtocolVersion".into(), json!(8));
    stream
        .write_all(&identity_packet(
            identity.device_id(),
            &shared.name,
            dial_extra,
        ))
        .await
        .unwrap();
    let material = TlsMaterial::new(identity.certificate_der(), identity.private_key_der());
    let tls_stream = tls::accept(stream, &material, &desktop_id, PeerPin::Unpinned)
        .await
        .unwrap();
    let desktop_certificate = tls::server_peer_certificate(&tls_stream).unwrap();
    *shared.desktop_id.lock().unwrap() = Some(desktop_id.clone());
    let (mut reader, mut writer) = tokio::io::split(tls_stream);
    writer
        .write_all(&identity_packet(
            identity.device_id(),
            &shared.name,
            Map::new(),
        ))
        .await
        .unwrap();
    eprintln!("fake phone: connected");

    let (sender, mut outgoing) = mpsc::channel::<Packet>(16);
    *shared.packets.lock().unwrap() = Some(sender.clone());
    let codec = PacketCodec::new(64 * 1024);
    let writer_cancellation = cancellation.clone();
    tokio::spawn(async move {
        loop {
            let packet = tokio::select! {
                _ = writer_cancellation.cancelled() => return,
                packet = outgoing.recv() => packet,
            };
            let Some(packet) = packet else { return };
            let mut line = codec.encode(&packet).unwrap();
            if !line.ends_with(b"\n") {
                line.push(b'\n');
            }
            if writer.write_all(&line).await.is_err() {
                return;
            }
        }
    });

    let mut codec = PacketCodec::new(64 * 1024);
    let mut buffer = vec![0_u8; 16 * 1024];
    // The first packet inside TLS is the desktop's identity.
    let mut seen_identity = false;
    loop {
        let read = tokio::select! {
            _ = cancellation.cancelled() => return,
            read = reader.read(&mut buffer) => read,
        };
        let Ok(read) = read else {
            eprintln!("fake phone: connection failed");
            return;
        };
        if read == 0 {
            eprintln!("fake phone: disconnected");
            return;
        }
        for packet in codec.decode(&buffer[..read]).unwrap() {
            if !seen_identity {
                seen_identity = true;
                continue;
            }
            eprintln!("fake phone: received {}", packet.packet_type);
            match packet.packet_type.as_str() {
                "kdeconnect.pair" if packet.body["pair"] == json!(true) => {
                    *shared.desktop_certificate.lock().unwrap() = Some(desktop_certificate.clone());
                    let accept =
                        Packet::from_body(0_u64, "kdeconnect.pair", &json!({"pair": true}))
                            .unwrap();
                    let _ = sender.send(accept).await;
                    // Android's battery plugin reports as soon as it loads.
                    let _ = sender.send(battery_report(PHONE_BATTERY, false)).await;
                    // So does its connectivity report plugin.
                    let _ = sender
                        .send(connectivity_report(PHONE_NETWORK, PHONE_SIGNAL))
                        .await;
                }
                REQUEST_PACKET_TYPE => {
                    shared.log.browse_requests.fetch_add(1, Ordering::SeqCst);
                    let _ = sender.send(browse_reply(&shared)).await;
                }
                NOTIFICATION_REQUEST_PACKET_TYPE
                | NOTIFICATION_REPLY_PACKET_TYPE
                | NOTIFICATION_ACTION_PACKET_TYPE => {
                    shared
                        .log
                        .notification_packets
                        .lock()
                        .unwrap()
                        .push(packet.clone());
                }
                _ => {}
            }
        }
    }
}

/// The charge the phone reports right after pairing.
pub const PHONE_BATTERY: i64 = 73;

fn battery_report(charge: i64, charging: bool) -> Packet {
    Packet::from_body(
        0_u64,
        BATTERY_PACKET_TYPE,
        &json!({"currentCharge": charge, "isCharging": charging, "thresholdEvent": 0}),
    )
    .unwrap()
}

/// The network and signal level (0 to 4) the phone reports right after
/// pairing, for its one SIM.
pub const PHONE_NETWORK: &str = "LTE";
pub const PHONE_SIGNAL: i64 = 3;

fn connectivity_report(network_type: &str, signal_strength: i64) -> Packet {
    Packet::from_body(
        0_u64,
        CONNECTIVITY_PACKET_TYPE,
        &json!({"signalStrengths": {
            "1": {"networkType": network_type, "signalStrength": signal_strength}
        }}),
    )
    .unwrap()
}

fn browse_reply(shared: &Shared) -> Packet {
    let body = match &shared.config_reply {
        BrowseReply::Serve(roots) => {
            let password: String = uuid::Uuid::new_v4().simple().to_string();
            *shared.password.lock().unwrap() = Some(password.clone());
            json!({
                "ip": "127.0.0.1",
                "port": shared.sftp_port,
                "user": "kdeconnect",
                "password": password,
                "path": "/",
                "multiPaths": roots.iter().map(|(path, _)| path).collect::<Vec<_>>(),
                "pathNames": roots.iter().map(|(_, name)| name).collect::<Vec<_>>(),
            })
        }
        BrowseReply::Refuse(message) => json!({"errorMessage": message}),
    };
    Packet::from_body(0_u64, SFTP_PACKET_TYPE, &body).unwrap()
}

async fn serve_sftp(
    listener: TcpListener,
    host_key: PrivateKey,
    storage: PathBuf,
    shared: Arc<Shared>,
    cancellation: CancellationToken,
) {
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    loop {
        let accepted = tokio::select! {
            _ = cancellation.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else { continue };
        let handler = SshHandler {
            shared: shared.clone(),
            storage: storage.clone(),
            channels: HashMap::new(),
        };
        let config = config.clone();
        let cancellation = cancellation.clone();
        let log = shared.log.clone();
        tokio::spawn(async move {
            log.open_connections.fetch_add(1, Ordering::SeqCst);
            if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                tokio::select! {
                    _ = cancellation.cancelled() => {}
                    _ = session => {}
                }
            }
            log.open_connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

struct SshHandler {
    shared: Arc<Shared>,
    storage: PathBuf,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for SshHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let paired = self.shared.desktop_certificate.lock().unwrap().clone();
        let matches = paired.is_some_and(|certificate| {
            certificate_public_key(&certificate).as_ref() == Some(public_key.key_data())
        });
        if user == "kdeconnect" && matches {
            self.shared.log.key_logins.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let expected = self.shared.password.lock().unwrap().clone();
        if user == "kdeconnect" && expected.as_deref() == Some(password) {
            self.shared
                .log
                .password_logins
                .fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.channels.remove(&channel_id)) {
            ("sftp", Some(channel)) => {
                self.shared.log.sftp_sessions.fetch_add(1, Ordering::SeqCst);
                session.channel_success(channel_id)?;
                let storage = Storage {
                    root: self.storage.clone(),
                    handles: HashMap::new(),
                    next_handle: 0,
                };
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), storage));
            }
            _ => session.channel_failure(channel_id)?,
        }
        Ok(())
    }
}

/// The SSH form of a certificate's public key.
fn certificate_public_key(certificate_der: &[u8]) -> Option<ssh_key::public::KeyData> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate_der).ok()?;
    match certificate.public_key().parsed().ok()? {
        x509_parser::public_key::PublicKey::EC(point) => {
            ssh_key::public::EcdsaPublicKey::from_sec1_bytes(point.data())
                .ok()
                .map(ssh_key::public::KeyData::Ecdsa)
        }
        _ => None,
    }
}

enum OpenHandle {
    File(fs::File),
    Directory { path: PathBuf, listed: bool },
}

/// An SFTP file system over a directory on disk.
struct Storage {
    root: PathBuf,
    handles: HashMap<String, OpenHandle>,
    next_handle: u64,
}

impl Storage {
    fn local(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let relative = Path::new(path.trim_start_matches('/'));
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(self.root.join(relative))
    }

    fn add_handle(&mut self, handle: OpenHandle) -> String {
        self.next_handle += 1;
        let id = self.next_handle.to_string();
        self.handles.insert(id.clone(), handle);
        id
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn io_status(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for Storage {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.local(&filename)?;
        let mut options = fs::OpenOptions::new();
        options
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE) || pflags.contains(OpenFlags::APPEND))
            .append(pflags.contains(OpenFlags::APPEND))
            .truncate(pflags.contains(OpenFlags::TRUNCATE));
        if pflags.contains(OpenFlags::CREATE) {
            if pflags.contains(OpenFlags::EXCLUDE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        let file = options.open(path).map_err(io_status)?;
        Ok(Handle {
            id,
            handle: self.add_handle(OpenHandle::File(file)),
        })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(OpenHandle::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(io_status)?;
        let mut data = vec![0_u8; len as usize];
        let read = file.read(&mut data).map_err(io_status)?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(OpenHandle::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(io_status)?;
        file.write_all(&data).map_err(io_status)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = fs::symlink_metadata(self.local(&path)?).map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: (&metadata).into(),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = fs::metadata(self.local(&path)?).map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: (&metadata).into(),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(OpenHandle::File(file)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let metadata = file.metadata().map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: (&metadata).into(),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = self.local(&path)?;
        if !fs::metadata(&path).map_err(io_status)?.is_dir() {
            return Err(StatusCode::Failure);
        }
        Ok(Handle {
            id,
            handle: self.add_handle(OpenHandle::Directory {
                path,
                listed: false,
            }),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(OpenHandle::Directory { path, listed }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *listed {
            return Err(StatusCode::Eof);
        }
        *listed = true;
        let mut files = Vec::new();
        for entry in fs::read_dir(path).map_err(io_status)? {
            let entry = entry.map_err(io_status)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(io_status)?;
            let mut attrs: FileAttributes = (&metadata).into();
            if metadata.file_type().is_symlink() {
                attrs.set_symlink(true);
                attrs.set_regular(false);
                attrs.set_dir(false);
            }
            files.push(File::new(entry.file_name().to_string_lossy(), attrs));
        }
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        fs::remove_file(self.local(&filename)?).map_err(io_status)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        fs::create_dir(self.local(&path)?).map_err(io_status)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        fs::remove_dir(self.local(&path)?).map_err(io_status)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let normalized = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        Ok(Name {
            id,
            files: vec![File::dummy(normalized)],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let to = self.local(&newpath)?;
        // SFTP v3 renames never replace the target.
        if fs::symlink_metadata(&to).is_ok() {
            return Err(StatusCode::Failure);
        }
        fs::rename(self.local(&oldpath)?, to).map_err(io_status)?;
        Ok(ok(id))
    }
}

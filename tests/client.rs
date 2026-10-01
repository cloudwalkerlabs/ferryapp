//! The control socket's client against fake daemons: each serves only the
//! methods its test needs, through the real server.

use std::{
    future::pending,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ferry::{
    client::{
        Client, ClientError, ClipboardWatchUpdate, DeviceWatchUpdate, Next, TransferWatchUpdate,
    },
    core::{
        CoreError, CoreEvent, DeviceReachability, DeviceSnapshot, EventData, PairingDirection,
        PairingSnapshot, PairingStatus, PluginEvent, TransferDirection, TransferSnapshot,
        TransferStatus,
    },
    plugins::{
        clipboard::{
            ClipboardSnapshot,
            rpc::{GetClipboard, SetClipboard},
        },
        ping::rpc::Ping,
    },
    protocol::DeviceType,
    rpc::{
        ForgetDevice, GetPairing, GetTransfer, ListDevices, Methods, RpcError, RpcServer, Subscribe,
    },
};
use tempfile::TempDir;
use tokio::{sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A daemon serving `methods` on a socket of its own.
struct FakeDaemon {
    path: PathBuf,
    server: RpcServer,
    _directory: TempDir,
}

impl FakeDaemon {
    async fn start(methods: Methods) -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("ferry.sock");
        let server = RpcServer::start(path.clone(), methods, CancellationToken::new())
            .await
            .unwrap();
        Self {
            path,
            server,
            _directory: directory,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.path).await.unwrap()
    }
}

fn device() -> DeviceSnapshot {
    DeviceSnapshot {
        device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        device_name: "Peer Phone".into(),
        device_type: DeviceType::Phone,
        protocol_version: 8,
        incoming_capabilities: vec!["kdeconnect.clipboard".into()],
        outgoing_capabilities: vec!["kdeconnect.share.request".into()],
        reachability: DeviceReachability::Connected,
        paired: true,
        pairing: false,
        last_seen_at: 10,
        plugins: Default::default(),
        addresses: Vec::new(),
    }
}

fn pairing_snapshot(status: PairingStatus) -> PairingSnapshot {
    PairingSnapshot {
        id: Uuid::from_u128(1),
        device_id: device().device_id,
        device_name: "Peer Phone".into(),
        direction: PairingDirection::Outgoing,
        status,
        verification_code: Some("ABCDEF12".into()),
        created_at: 10,
        expires_at: 40,
        error_code: None,
    }
}

fn transfer_snapshot() -> TransferSnapshot {
    TransferSnapshot {
        id: Uuid::from_u128(2),
        device_id: device().device_id,
        device_name: "Peer Phone".into(),
        direction: TransferDirection::Outgoing,
        status: TransferStatus::Transferring,
        file_name: "payload.txt".into(),
        total_bytes: 12,
        transferred_bytes: 3,
        created_at: 10,
        updated_at: 11,
        error_code: None,
        saved_path: None,
    }
}

fn clipboard(text: &str) -> ClipboardSnapshot {
    ClipboardSnapshot {
        text: text.into(),
        updated_at: 10,
        source_device_id: None,
        sync_enabled: true,
    }
}

fn core_event(event: EventData) -> CoreEvent {
    CoreEvent {
        sequence: 1,
        timestamp: 12,
        event,
    }
}

#[tokio::test]
async fn calls_and_streams_carry_typed_params_and_answers() {
    let mut methods = Methods::new();
    methods.add((), |(), ListDevices {}| async {
        Ok::<_, RpcError>(vec![device()])
    });
    methods.add((), |(), Ping { device_id, message }| async move {
        assert_eq!(device_id, device().device_id);
        assert_eq!(message.as_deref(), Some("hello"));
        Ok::<_, RpcError>(())
    });
    methods.add((), |(), GetPairing { pairing_id }| async move {
        assert_eq!(pairing_id, Uuid::from_u128(1));
        Ok::<_, RpcError>(pairing_snapshot(PairingStatus::AwaitingConfirmation))
    });
    methods.add((), |(), SetClipboard { text }| async move {
        Ok::<_, RpcError>(clipboard(&text))
    });
    // A subscription that sends one event, then ends.
    methods.add_stream((), |(), Subscribe {}, items| async move {
        items.send(&None).await;
        let changed = PluginEvent::new(&clipboard("changed")).unwrap();
        items
            .send(&Some(core_event(EventData::Plugin(changed))))
            .await;
        Ok::<_, RpcError>(())
    });
    let daemon = FakeDaemon::start(methods).await;
    let client = daemon.client().await;

    assert_eq!(client.call(ListDevices {}).await.unwrap(), vec![device()]);
    client
        .call(Ping {
            device_id: device().device_id,
            message: Some("hello".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .call(GetPairing {
                pairing_id: Uuid::from_u128(1)
            })
            .await
            .unwrap()
            .status,
        PairingStatus::AwaitingConfirmation
    );
    assert_eq!(
        client
            .call(SetClipboard {
                text: "updated".into()
            })
            .await
            .unwrap()
            .text,
        "updated"
    );

    let mut events = client.stream(Subscribe {}).await.unwrap();
    assert!(matches!(events.next().await.unwrap(), Next::Item(None)));
    let Next::Item(Some(event)) = events.next().await.unwrap() else {
        panic!("expected an event");
    };
    let EventData::Plugin(event) = event.event else {
        panic!("expected a plugin event");
    };
    assert_eq!(event.decode::<ClipboardSnapshot>().unwrap().text, "changed");
    assert!(matches!(events.next().await.unwrap(), Next::Done(())));
}

#[tokio::test]
async fn requests_on_one_connection_are_answered_as_they_finish() {
    let mut methods = Methods::new();
    // Never answers until the daemon stops.
    methods.add((), |(), GetTransfer { .. }| async {
        pending::<Result<TransferSnapshot, RpcError>>().await
    });
    methods.add((), |(), ListDevices {}| async {
        Ok::<_, RpcError>(vec![device()])
    });
    let daemon = FakeDaemon::start(methods).await;
    let client = Arc::new(daemon.client().await);

    let waiting = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call(GetTransfer {
                    transfer_id: Uuid::from_u128(2),
                })
                .await
        }
    });
    // The slow request doesn't hold up a later one.
    assert_eq!(
        timeout(Duration::from_secs(2), client.call(ListDevices {}))
            .await
            .unwrap()
            .unwrap(),
        vec![device()]
    );

    // A daemon that stops fails what is still waiting.
    daemon.server.shutdown().await;
    let result = timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Err(ClientError::Disconnected)),
        "{result:?}"
    );
}

/// Snapshots served, and a subscription that reports a gap right after it
/// starts, as one that fell behind does.
fn lagging_daemon(reads: Arc<AtomicUsize>) -> Methods {
    let mut methods = Methods::new();
    methods.add(reads.clone(), |reads, ListDevices {}| async move {
        reads.fetch_add(1, Ordering::SeqCst);
        Ok::<_, RpcError>(vec![device()])
    });
    methods.add(reads.clone(), |reads, GetClipboard {}| async move {
        reads.fetch_add(1, Ordering::SeqCst);
        Ok::<_, RpcError>(clipboard("current"))
    });
    methods.add(reads, |reads, GetTransfer { transfer_id }| async move {
        assert_eq!(transfer_id, Uuid::from_u128(2));
        reads.fetch_add(1, Ordering::SeqCst);
        Ok::<_, RpcError>(transfer_snapshot())
    });
    methods.add_stream((), |(), Subscribe {}, items| async move {
        items.send(&None).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        Err::<(), _>(RpcError::failed("events_lagged", "fell behind"))
    });
    methods
}

#[tokio::test]
async fn watch_modes_refetch_snapshots_after_a_gap() {
    let reads = Arc::new(AtomicUsize::new(0));
    let daemon = FakeDaemon::start(lagging_daemon(reads.clone())).await;
    let client = daemon.client().await;

    let watched = |reads: &Arc<AtomicUsize>| {
        reads.store(0, Ordering::SeqCst);
        let cancellation = CancellationToken::new();
        (cancellation.clone(), cancellation, reads.clone())
    };

    let (cancellation, stop, count) = watched(&reads);
    timeout(
        Duration::from_secs(2),
        client.watch_devices(cancellation, move |update| {
            if matches!(update, DeviceWatchUpdate::Snapshot(_)) && count.load(Ordering::SeqCst) >= 2
            {
                stop.cancel();
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();

    let (cancellation, stop, count) = watched(&reads);
    timeout(
        Duration::from_secs(2),
        client.watch_clipboard(cancellation, move |update| {
            if matches!(update, ClipboardWatchUpdate::Snapshot(_))
                && count.load(Ordering::SeqCst) >= 2
            {
                stop.cancel();
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();

    let (cancellation, stop, count) = watched(&reads);
    timeout(
        Duration::from_secs(2),
        client.watch_transfer(Uuid::from_u128(2), cancellation, move |update| {
            if matches!(update, TransferWatchUpdate::Snapshot(_))
                && count.load(Ordering::SeqCst) >= 2
            {
                stop.cancel();
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn errors_are_distinct_and_actionable() {
    let mut methods = Methods::new();
    methods.add((), |(), GetPairing { .. }| async {
        Err::<PairingSnapshot, _>(CoreError::UnknownPairing)
    });
    methods.add((), |(), Ping { message, .. }| async move {
        match message.as_deref() {
            Some("unsupported") => Err(CoreError::UnsupportedByPeer),
            _ => Ok(()),
        }
    });
    let daemon = FakeDaemon::start(methods).await;
    let client = daemon.client().await;

    let missing = client
        .call(GetPairing {
            pairing_id: Uuid::max(),
        })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Some("pairing_not_found"));
    let refused = client
        .call(Ping {
            device_id: device().device_id,
            message: Some("unsupported".into()),
        })
        .await
        .unwrap_err();
    let ClientError::Rpc(error) = &refused else {
        panic!("expected the daemon's error, got {refused:?}");
    };
    assert_eq!(error.error_code(), "unsupported_by_peer");
    assert_eq!(
        refused.to_string(),
        "peer has not advertised support for this packet type"
    );
    // A method the daemon doesn't have, as an older one wouldn't.
    let unknown = client
        .call(ForgetDevice {
            device_id: device().device_id,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            unknown,
            ClientError::UnknownMethod {
                method: "devices.forget"
            }
        ),
        "{unknown:?}"
    );

    // Nothing at the path, or a socket a crashed daemon left behind.
    let directory = TempDir::new().unwrap();
    let absent = directory.path().join("ferry.sock");
    let error = Client::connect(&absent).await.err().unwrap();
    assert!(
        matches!(&error, ClientError::DaemonUnavailable { path } if *path == absent),
        "{error:?}"
    );
    assert!(error.to_string().contains("ferry-cli run"), "{error}");
    drop(std::os::unix::net::UnixListener::bind(&absent).unwrap());
    let error = Client::connect(&absent).await.err().unwrap();
    assert!(
        matches!(error, ClientError::DaemonUnavailable { .. }),
        "{error:?}"
    );
}

/// A daemon whose resources change right after each snapshot is taken:
/// every snapshot publishes an event, reaching only the subscriptions
/// already made, as the real event bus does.
fn racing_daemon() -> Methods {
    let (events, _) = broadcast::channel::<CoreEvent>(16);
    let publish = |events: &broadcast::Sender<CoreEvent>, event: EventData| {
        let _ = events.send(core_event(event));
    };
    let mut methods = Methods::new();
    methods.add(events.clone(), move |events, ListDevices {}| async move {
        publish(&events, EventData::DeviceUpdated(device()));
        Ok::<_, RpcError>(vec![device()])
    });
    methods.add(events.clone(), move |events, GetClipboard {}| async move {
        let changed = PluginEvent::new(&clipboard("changed")).unwrap();
        publish(&events, EventData::Plugin(changed));
        Ok::<_, RpcError>(clipboard("current"))
    });
    methods.add(
        events.clone(),
        move |events, GetTransfer { .. }| async move {
            // The transfer ends just after this snapshot of it is taken.
            let running = transfer_snapshot();
            let completed = TransferSnapshot {
                status: TransferStatus::Completed,
                transferred_bytes: running.total_bytes,
                ..running.clone()
            };
            publish(&events, EventData::TransferCompleted(completed));
            Ok::<_, RpcError>(running)
        },
    );
    methods.add_stream(events, |events, Subscribe {}, items| async move {
        let mut receiver = events.subscribe();
        items.send(&None).await;
        while let Ok(event) = receiver.recv().await {
            if !items.send(&Some(event)).await {
                break;
            }
        }
        Ok::<_, RpcError>(())
    });
    methods
}

#[tokio::test]
async fn watch_modes_see_changes_made_right_after_their_snapshot() {
    let daemon = FakeDaemon::start(racing_daemon()).await;
    let client = daemon.client().await;

    // A transfer that ends between the snapshot and the next event must
    // still end the watch, rather than leave it waiting forever.
    let mut updates = Vec::new();
    timeout(
        Duration::from_secs(2),
        client.watch_transfer(Uuid::from_u128(2), CancellationToken::new(), |update| {
            updates.push(match update {
                TransferWatchUpdate::Snapshot(transfer) => transfer.status,
                TransferWatchUpdate::Event(event) => match event.event {
                    EventData::TransferCompleted(transfer) => transfer.status,
                    other => panic!("unexpected event {other:?}"),
                },
            });
        }),
    )
    .await
    .expect("the watch missed the transfer's end")
    .unwrap();
    assert_eq!(
        updates,
        [TransferStatus::Transferring, TransferStatus::Completed]
    );

    let cancellation = CancellationToken::new();
    let stop = cancellation.clone();
    timeout(
        Duration::from_secs(2),
        client.watch_devices(cancellation, move |update| {
            if matches!(update, DeviceWatchUpdate::Event(_)) {
                stop.cancel();
            }
        }),
    )
    .await
    .expect("the watch missed a device change")
    .unwrap();

    let cancellation = CancellationToken::new();
    let stop = cancellation.clone();
    timeout(
        Duration::from_secs(2),
        client.watch_clipboard(cancellation, move |update| {
            if matches!(update, ClipboardWatchUpdate::Event(_)) {
                stop.cancel();
            }
        }),
    )
    .await
    .expect("the watch missed a clipboard change")
    .unwrap();
}

//! Client for the daemon's control socket ([`crate::rpc`]): one connection,
//! any number of requests on it at once, and the CLI's watch helpers.

use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};
use uuid::Uuid;

use crate::{
    core::{CoreEvent, DeviceSnapshot, EventData, PluginEventKind, TransferSnapshot},
    plugins::{
        clipboard::{ClipboardSnapshot, rpc::GetClipboard},
        notifications::{
            Notification, NotificationPosted, NotificationRemoved, rpc::ListNotifications,
        },
        telephony::{Call, CallMissed, rpc::GetCall},
    },
    rpc::{
        GetTransfer, ListDevices, MAX_LINE_BYTES, Message, Method, RpcError, STREAM, StreamItem,
        StreamMethod, Subscribe, code,
    },
};

type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// What arrives for one request: its stream items, then its answer.
enum Incoming {
    Item(Value),
    Answer(Result<Value, RpcError>),
}

type Pending = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Incoming>>>>;

/// A connection to a daemon. Dropping it closes the connection, which
/// drops whatever it asked for that is still running.
pub struct Client {
    writer: tokio::sync::Mutex<Writer>,
    pending: Pending,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
}

impl Client {
    /// Connect to the daemon whose data directory is `data_dir`.
    pub async fn for_data_dir(data_dir: &Path) -> Result<Self, ClientError> {
        Self::connect(&crate::rpc::socket_path(data_dir)).await
    }

    /// Connect to the daemon listening at `path`.
    #[cfg(unix)]
    pub async fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|source| match source.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    ClientError::DaemonUnavailable {
                        path: path.to_owned(),
                    }
                }
                _ => ClientError::Connect {
                    path: path.to_owned(),
                    source,
                },
            })?;
        let (read, write) = stream.into_split();
        Ok(Self::over(read, write))
    }

    #[cfg(not(unix))]
    pub async fn connect(_path: &Path) -> Result<Self, ClientError> {
        Err(ClientError::Unsupported)
    }

    /// A client over any byte stream to a server.
    pub fn over(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let pending = Pending::default();
        let reader = tokio::spawn(read_messages(read, pending.clone()));
        Self {
            writer: tokio::sync::Mutex::new(Box::new(write)),
            pending,
            next_id: AtomicU64::new(1),
            reader,
        }
    }

    /// Send `params` and wait for the answer.
    pub async fn call<M: Method>(&self, params: M) -> Result<M::Output, ClientError> {
        let (_, mut incoming) = self.send(&params).await?;
        loop {
            match incoming.recv().await {
                Some(Incoming::Item(_)) => {}
                Some(Incoming::Answer(answer)) => return decode::<M>(answer),
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    /// Send `params` and follow what it sends back: its items, then its
    /// answer.
    pub async fn stream<M: StreamMethod>(&self, params: M) -> Result<Streamed<M>, ClientError> {
        let (id, incoming) = self.send(&params).await?;
        Ok(Streamed {
            id,
            incoming,
            pending: self.pending.clone(),
            _method: std::marker::PhantomData,
        })
    }

    async fn send<M: Method>(
        &self,
        params: &M,
    ) -> Result<(u64, mpsc::UnboundedReceiver<Incoming>), ClientError> {
        let params = serde_json::to_value(params).map_err(|_| ClientError::InvalidRequest)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, sender);
        let line = Message::request(id, M::NAME, params).to_line();
        let mut writer = self.writer.lock().await;
        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(ClientError::Disconnected);
        }
        Ok((id, receiver))
    }

    /// Print-ready updates for the device list: a snapshot, then device
    /// events, and a fresh snapshot after a gap. Runs until `cancellation`.
    pub async fn watch_devices<F>(
        &self,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(DeviceWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.call(ListDevices {}),
            |update| {
                match update {
                    Watched::Snapshot(devices) => on_update(DeviceWatchUpdate::Snapshot(devices)),
                    Watched::Event(event) if is_device_event(&event.event) => {
                        on_update(DeviceWatchUpdate::Event(event));
                    }
                    Watched::Event(_) => {}
                }
                false
            },
        )
        .await
    }

    /// Like [`Client::watch_devices`], for the synced clipboard text.
    pub async fn watch_clipboard<F>(
        &self,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(ClipboardWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.call(GetClipboard {}),
            |update| {
                match update {
                    Watched::Snapshot(clipboard) => {
                        on_update(ClipboardWatchUpdate::Snapshot(clipboard));
                    }
                    Watched::Event(event)
                        if event.event.event_type() == ClipboardSnapshot::TYPE =>
                    {
                        on_update(ClipboardWatchUpdate::Event(event));
                    }
                    Watched::Event(_) => {}
                }
                false
            },
        )
        .await
    }

    /// Like [`Client::watch_devices`], for one device's notifications:
    /// the list, then each `notification.*` event about the device.
    pub async fn watch_notifications<F>(
        &self,
        device_id: &str,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(NotificationWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || {
                self.call(ListNotifications {
                    device_id: device_id.to_owned(),
                })
            },
            |update| {
                match update {
                    Watched::Snapshot(notifications) => {
                        on_update(NotificationWatchUpdate::Snapshot(notifications));
                    }
                    Watched::Event(event) => {
                        let EventData::Plugin(plugin_event) = &event.event else {
                            return false;
                        };
                        let about_device = plugin_event
                            .decode::<NotificationPosted>()
                            .map(|posted| posted.device_id)
                            .or_else(|| {
                                plugin_event
                                    .decode::<NotificationRemoved>()
                                    .map(|removed| removed.device_id)
                            })
                            .is_some_and(|id| id == device_id);
                        if about_device {
                            on_update(NotificationWatchUpdate::Event(event));
                        }
                    }
                }
                false
            },
        )
        .await
    }

    /// Like [`Client::watch_devices`], for one device's calls: the call
    /// going on, again each time it changes, and each missed call.
    pub async fn watch_call<F>(
        &self,
        device_id: &str,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(CallWatchUpdate) + Send,
    {
        // Device events also carry other changes (a battery report); only
        // a change of call is news.
        let mut last = None;
        self.watch(
            cancellation,
            || {
                self.call(GetCall {
                    device_id: device_id.to_owned(),
                })
            },
            |update| {
                let call = match update {
                    Watched::Snapshot(call) => call,
                    Watched::Event(event) => match &event.event {
                        EventData::DeviceConnected(device)
                        | EventData::DeviceUpdated(device)
                        | EventData::DeviceDisconnected(device)
                        | EventData::DeviceForgotten(device)
                            if device.device_id == device_id =>
                        {
                            Call::of(device)
                        }
                        EventData::Plugin(event) => {
                            if let Some(missed) = event
                                .decode::<CallMissed>()
                                .filter(|missed| missed.device_id == device_id)
                            {
                                on_update(CallWatchUpdate::Missed(missed));
                            }
                            return false;
                        }
                        _ => return false,
                    },
                };
                if last.as_ref() != Some(&call) {
                    last = Some(call.clone());
                    on_update(CallWatchUpdate::Call(call));
                }
                false
            },
        )
        .await
    }

    /// Like [`Client::watch_devices`], for one transfer, ending once the
    /// transfer has (or at once if it already had).
    pub async fn watch_transfer<F>(
        &self,
        transfer_id: Uuid,
        cancellation: CancellationToken,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(TransferWatchUpdate) + Send,
    {
        self.watch(
            cancellation,
            || self.call(GetTransfer { transfer_id }),
            |update| match update {
                Watched::Snapshot(transfer) => {
                    let terminal = transfer.status.is_terminal();
                    on_update(TransferWatchUpdate::Snapshot(transfer));
                    terminal
                }
                Watched::Event(event) => {
                    let Some(transfer) = transfer_from_event(&event.event)
                        .filter(|transfer| transfer.id == transfer_id)
                    else {
                        return false;
                    };
                    let terminal = transfer.status.is_terminal();
                    on_update(TransferWatchUpdate::Event(event));
                    terminal
                }
            },
        )
        .await
    }

    /// Follow a resource: its snapshot from `load`, then every event, and
    /// again after a gap in the events, until `on_update` returns `true`
    /// or `cancellation`. Subscribes before loading the snapshot, so a
    /// change made in between arrives as an event instead of being missed.
    async fn watch<S, Load, Loading, F>(
        &self,
        cancellation: CancellationToken,
        load: Load,
        mut on_update: F,
    ) -> Result<(), ClientError>
    where
        Load: Fn() -> Loading,
        Loading: Future<Output = Result<S, ClientError>>,
        F: FnMut(Watched<S>) -> bool,
    {
        loop {
            let mut events = self.stream(Subscribe {}).await?;
            // The daemon's first item says it has subscribed.
            match events.next().await? {
                Next::Item(None) => {}
                _ => return Err(ClientError::InvalidResponse),
            }
            if on_update(Watched::Snapshot(load().await?)) || cancellation.is_cancelled() {
                return Ok(());
            }
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => return Ok(()),
                    next = events.next() => next,
                };
                match next {
                    Ok(Next::Item(Some(event))) => {
                        if on_update(Watched::Event(event)) {
                            return Ok(());
                        }
                    }
                    Ok(Next::Item(None)) => {}
                    // The daemon is stopping.
                    Ok(Next::Done(())) => return Err(ClientError::Disconnected),
                    Err(ClientError::Rpc(error)) if error.error_code() == "events_lagged" => break,
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A [`StreamMethod`]'s items, then its answer.
pub struct Streamed<M: StreamMethod> {
    id: u64,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    pending: Pending,
    _method: std::marker::PhantomData<fn() -> M>,
}

pub enum Next<M: StreamMethod> {
    Item(M::Item),
    /// The answer: nothing follows it.
    Done(M::Output),
}

impl<M: StreamMethod> Streamed<M> {
    /// The next item, or the answer once every item has come.
    pub async fn next(&mut self) -> Result<Next<M>, ClientError> {
        match self.incoming.recv().await {
            Some(Incoming::Item(item)) => serde_json::from_value(item)
                .map(Next::Item)
                .map_err(|_| ClientError::InvalidResponse),
            Some(Incoming::Answer(answer)) => decode::<M>(answer).map(Next::Done),
            None => Err(ClientError::Disconnected),
        }
    }
}

impl<M: StreamMethod> Drop for Streamed<M> {
    fn drop(&mut self) {
        // Items still arriving for it are dropped.
        self.pending.lock().unwrap().remove(&self.id);
    }
}

fn decode<M: Method>(answer: Result<Value, RpcError>) -> Result<M::Output, ClientError> {
    match answer {
        Ok(result) => serde_json::from_value(result).map_err(|_| ClientError::InvalidResponse),
        Err(error) if error.code == code::METHOD_NOT_FOUND => {
            Err(ClientError::UnknownMethod { method: M::NAME })
        }
        Err(error) => Err(ClientError::Rpc(error)),
    }
}

/// Hand each message to the request it is for, until the connection
/// closes; then every request still waiting fails.
async fn read_messages(read: impl AsyncRead + Unpin, pending: Pending) {
    let mut lines = FramedRead::new(read, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
    while let Some(Ok(line)) = lines.next().await {
        let Ok(message) = serde_json::from_str::<Message>(&line) else {
            tracing::debug!("ignoring an unreadable message from the daemon");
            continue;
        };
        let (id, incoming) = if message.method.as_deref() == Some(STREAM) {
            let Some(Ok(item)) = message.params.map(serde_json::from_value::<StreamItem>) else {
                continue;
            };
            (item.id, Incoming::Item(item.item))
        } else {
            let answer = match message.error {
                Some(error) => Err(error),
                None => Ok(message.result.unwrap_or(Value::Null)),
            };
            (message.id.unwrap_or(Value::Null), Incoming::Answer(answer))
        };
        let Some(id) = id.as_u64() else { continue };
        let mut pending = pending.lock().unwrap();
        let answered = matches!(incoming, Incoming::Answer(_));
        if let Some(sender) = pending.get(&id) {
            let _ = sender.send(incoming);
        }
        if answered {
            pending.remove(&id);
        }
    }
    // Dropping the senders fails what is waiting on them.
    pending.lock().unwrap().clear();
}

pub enum DeviceWatchUpdate {
    Snapshot(Vec<DeviceSnapshot>),
    Event(CoreEvent),
}

pub enum NotificationWatchUpdate {
    Snapshot(Vec<Notification>),
    Event(CoreEvent),
}

pub enum CallWatchUpdate {
    /// The call going on now, or `None`.
    Call(Option<Call>),
    Missed(CallMissed),
}

#[derive(Debug)]
pub enum ClipboardWatchUpdate {
    Snapshot(ClipboardSnapshot),
    Event(CoreEvent),
}

pub enum TransferWatchUpdate {
    Snapshot(TransferSnapshot),
    Event(CoreEvent),
}

/// What [`Client::watch`] hands its callback.
enum Watched<S> {
    Snapshot(S),
    Event(CoreEvent),
}

fn is_device_event(event: &EventData) -> bool {
    matches!(
        event,
        EventData::DeviceDiscovered(_)
            | EventData::DeviceConnected(_)
            | EventData::DeviceUpdated(_)
            | EventData::DeviceDisconnected(_)
            | EventData::DeviceForgotten(_)
    )
}

fn transfer_from_event(event: &EventData) -> Option<&TransferSnapshot> {
    match event {
        EventData::TransferStarted(transfer)
        | EventData::TransferProgress(transfer)
        | EventData::TransferCompleted(transfer)
        | EventData::TransferFailed(transfer) => Some(transfer),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(
        "no Ferry is listening at {}; start one with `ferry-cli run`, or turn on \
         Command line access in the Ferry app's Settings",
        path.display()
    )]
    DaemonUnavailable { path: PathBuf },
    #[error("couldn't connect to {}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("command line access isn't available on this platform yet")]
    Unsupported,
    #[error("lost the connection to Ferry")]
    Disconnected,
    #[error("the running Ferry doesn't know {method}; is it an older version?")]
    UnknownMethod { method: &'static str },
    #[error(transparent)]
    Rpc(RpcError),
    #[error("couldn't encode the request")]
    InvalidRequest,
    #[error("Ferry sent an invalid answer")]
    InvalidResponse,
}

impl ClientError {
    /// The daemon's code for a method's failure, e.g. `device_not_found`.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Rpc(error) => Some(error.error_code()),
            _ => None,
        }
    }
}

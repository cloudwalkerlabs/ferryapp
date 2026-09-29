//! Transfers: the core service every feature that moves a file builds on
//! (sharing, browsing a device's files).
//!
//! A feature starts a transfer with [`Transfers::begin`] and gets a
//! [`TransferHandle`]. The handle owns the transfer's state machine: it
//! moves it through `connecting` and `transferring`, records progress
//! (publishing at most one `transfer.progress` event per
//! [`PROGRESS_EVENT_INTERVAL`]), and ends it as completed, failed or
//! cancelled. A handle dropped before it ends the transfer fails it (or
//! cancels it, if cancellation was asked for), so a task that panics or is
//! aborted never leaves a transfer running. The core lists transfers
//! (`GET /transfers`), cancels them (`DELETE /transfers/{id}`, a device
//! disconnecting), and waits for their tasks at shutdown.
//!
//! This module also has the pure helpers: destination filename
//! sanitization and collision-avoiding naming, kept free of sockets and
//! state so path sanitization can be tested in isolation.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use serde::{Deserialize, Serialize};

use super::{
    CoreError, DeviceSnapshot, EventBus, EventData, OperationErrorCode, settings::Settings,
};
use crate::transport::payload::{self, PayloadError};

/// Conservative default cap on a single incoming or outgoing transfer, in
/// bytes. This exists to keep a misbehaving or malicious peer from causing
/// unbounded disk use; it is not a KDE Connect protocol limit.
pub const DEFAULT_MAX_TRANSFER_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Default deadline for establishing an auxiliary payload connection.
pub const DEFAULT_PAYLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Configuration for the transfer subsystem.
#[derive(Clone, Debug)]
pub struct TransferConfig {
    pub download_dir: PathBuf,
    pub max_transfer_bytes: u64,
    pub payload_bind_ip: Ipv4Addr,
    pub payload_connect_timeout: Duration,
}

impl TransferConfig {
    pub fn new(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            max_transfer_bytes: DEFAULT_MAX_TRANSFER_BYTES,
            payload_bind_ip: Ipv4Addr::UNSPECIFIED,
            payload_connect_timeout: DEFAULT_PAYLOAD_CONNECT_TIMEOUT,
        }
    }

    pub fn with_max_transfer_bytes(mut self, value: u64) -> Self {
        self.max_transfer_bytes = value;
        self
    }

    pub fn with_payload_bind_ip(mut self, ip: Ipv4Addr) -> Self {
        self.payload_bind_ip = ip;
        self
    }

    pub fn with_payload_connect_timeout(mut self, value: Duration) -> Self {
        self.payload_connect_timeout = value;
        self
    }
}

/// Reduce a peer-declared filename to a single safe path component: strip
/// any directory parts, then reject anything empty, `.`, `..`, or containing
/// a NUL byte. This is the sole path-traversal defense for incoming
/// transfers: the sanitized name is later joined to the configured download
/// directory and never otherwise interpreted as a path.
pub fn sanitize_file_name(name: &str) -> Result<String, FileNameError> {
    if name.contains('\0') {
        return Err(FileNameError::Invalid);
    }
    let base = Path::new(name)
        .file_name()
        .ok_or(FileNameError::Invalid)?
        .to_str()
        .ok_or(FileNameError::Invalid)?;
    if base.is_empty() || base == "." || base == ".." {
        return Err(FileNameError::Invalid);
    }
    Ok(base.to_owned())
}

/// Return a destination path under `dir` for `file_name`, appending a
/// ` (n)` suffix before the extension if the name already exists, so a
/// completed transfer never silently overwrites an unrelated file.
pub fn unique_destination(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(file_name);
    let extension = path.extension().and_then(|value| value.to_str());
    for attempt in 1_u32.. {
        let candidate_name = match extension {
            Some(extension) => format!("{stem} ({attempt}).{extension}"),
            None => format!("{stem} ({attempt})"),
        };
        let candidate = dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 attempts exhausted")
}

/// Minimum gap between `transfer.progress` events for one transfer. Copy
/// loops report every 64 KiB chunk, which on a fast link would flood the
/// bounded event bus and make slow subscribers lag.
pub const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

/// Capacity of the channel an upload's bytes are streamed through (see
/// [`upload_channel`]). Small on purpose: the HTTP handler and the network
/// writer stay coupled by backpressure instead of one side racing ahead and
/// buffering the whole file in memory.
const UPLOAD_CHANNEL_CAPACITY: usize = 4;

/// A bounded channel for streaming an upload's bytes from the HTTP request
/// into a transfer's task, which drains it with [`TransferHandle::forward`].
pub fn upload_channel() -> (mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
    mpsc::channel(UPLOAD_CHANNEL_CAPACITY)
}

/// Size of the chunks [`forward_reader`] reads.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Stream what `reader` yields into an upload channel (see
/// [`upload_channel`]) until it ends, or until the transfer stops taking
/// chunks because it ended. Dropping the sender at the end tells the
/// transfer the upload is over, so a reader that fails part-way fails the
/// transfer as short.
pub async fn forward_reader<R>(mut reader: R, sender: mpsc::Sender<Bytes>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    loop {
        let mut chunk = bytes::BytesMut::with_capacity(READ_CHUNK_BYTES);
        if reader.read_buf(&mut chunk).await? == 0 {
            return Ok(());
        }
        if sender.send(chunk.freeze()).await.is_err() {
            return Ok(());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Connecting,
    Transferring,
    Completed,
    Cancelled,
    Failed,
}

impl TransferStatus {
    /// Whether the transfer has ended, one way or another.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Queued,
                Self::Connecting | Self::Transferring | Self::Cancelled | Self::Failed
            ) | (
                Self::Connecting,
                Self::Transferring | Self::Cancelled | Self::Failed
            ) | (
                Self::Transferring,
                Self::Completed | Self::Cancelled | Self::Failed
            )
        )
    }
}

/// Immutable view of a file transfer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferSnapshot {
    pub id: Uuid,
    pub device_id: String,
    pub device_name: String,
    pub direction: TransferDirection,
    pub status: TransferStatus,
    pub file_name: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<OperationErrorCode>,
    /// Where a completed incoming file was saved. It can differ from
    /// `file_name` when a file of that name already existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<PathBuf>,
}

/// Mutable core representation of a transfer operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    snapshot: TransferSnapshot,
}

impl Transfer {
    pub fn new(snapshot: TransferSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> TransferSnapshot {
        self.snapshot.clone()
    }

    pub fn transition(
        &mut self,
        next: TransferStatus,
        updated_at: u64,
        error_code: Option<OperationErrorCode>,
    ) -> Result<TransferSnapshot, TransferTransitionError> {
        let current = self.snapshot.status;
        if !current.can_transition_to(next) {
            return Err(TransferTransitionError { current, next });
        }
        self.snapshot.status = next;
        self.snapshot.updated_at = updated_at;
        self.snapshot.error_code = error_code;
        Ok(self.snapshot())
    }

    pub fn record_progress(
        &mut self,
        transferred_bytes: u64,
        updated_at: u64,
    ) -> Result<TransferSnapshot, TransferProgressError> {
        if self.snapshot.status != TransferStatus::Transferring {
            return Err(TransferProgressError::NotTransferring(self.snapshot.status));
        }
        if transferred_bytes < self.snapshot.transferred_bytes
            || transferred_bytes > self.snapshot.total_bytes
        {
            return Err(TransferProgressError::InvalidByteCount {
                previous: self.snapshot.transferred_bytes,
                next: transferred_bytes,
                total: self.snapshot.total_bytes,
            });
        }
        self.snapshot.transferred_bytes = transferred_bytes;
        self.snapshot.updated_at = updated_at;
        Ok(self.snapshot())
    }

    /// Mark an incoming transfer completed, recording where the file landed.
    pub fn complete(
        &mut self,
        saved_path: Option<PathBuf>,
        updated_at: u64,
    ) -> Result<TransferSnapshot, TransferTransitionError> {
        self.transition(TransferStatus::Completed, updated_at, None)?;
        self.snapshot.saved_path = saved_path;
        Ok(self.snapshot())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("invalid transfer transition from {current:?} to {next:?}")]
pub struct TransferTransitionError {
    pub current: TransferStatus,
    pub next: TransferStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum TransferProgressError {
    #[error("cannot update progress while transfer is {0:?}")]
    NotTransferring(TransferStatus),
    #[error("invalid transfer byte count {next}; previous is {previous} and total is {total}")]
    InvalidByteCount {
        previous: u64,
        next: u64,
        total: u64,
    },
}

/// Every transfer this daemon process knows about. Cheap to clone.
#[derive(Clone)]
pub struct Transfers {
    inner: Arc<Inner>,
}

struct Inner {
    config: TransferConfig,
    events: EventBus,
    /// Read for the download directory as each download starts.
    settings: Arc<Mutex<Settings>>,
    /// Never locked while calling out of this module.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Every transfer, including finished ones; never removed.
    records: BTreeMap<Uuid, Transfer>,
    /// Transfers whose handle hasn't ended them yet.
    active: HashMap<Uuid, Active>,
}

struct Active {
    device_id: String,
    cancellation: CancellationToken,
    /// The task running the transfer, once [`TransferHandle::spawn`] has
    /// started it; awaited at shutdown.
    task: Option<JoinHandle<()>>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        // The state stays consistent across a panic: every change is a
        // single insert, remove or transition.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, event: EventData) {
        let _ = self.events.publish(event);
    }
}

impl Transfers {
    pub(super) fn new(
        config: TransferConfig,
        events: EventBus,
        settings: Arc<Mutex<Settings>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                events,
                settings,
                state: Mutex::default(),
            }),
        }
    }

    pub(super) fn config(&self) -> &TransferConfig {
        &self.inner.config
    }

    /// The largest file a transfer may move, in either direction.
    pub fn max_bytes(&self) -> u64 {
        self.inner.config.max_transfer_bytes
    }

    /// Start a transfer with `device`, `queued`, and publish
    /// `transfer.started`. Validation (file name, size limit, whether the
    /// device can take it) is the caller's; a request the caller rejects
    /// can still be recorded, by beginning it and failing it at once.
    pub fn begin(
        &self,
        device: &DeviceSnapshot,
        direction: TransferDirection,
        file_name: String,
        total_bytes: u64,
    ) -> TransferHandle {
        self.begin_as(None, device, direction, file_name, total_bytes)
            .expect("a fresh id is never taken")
    }

    /// Like [`Self::begin`], but with `id`, when given, as the transfer's
    /// id: one a client chose so that it can follow the transfer before the
    /// request that starts it answers. Fails with
    /// [`CoreError::TransferExists`] if a transfer already has that id.
    pub fn begin_as(
        &self,
        id: Option<Uuid>,
        device: &DeviceSnapshot,
        direction: TransferDirection,
        file_name: String,
        total_bytes: u64,
    ) -> Result<TransferHandle, CoreError> {
        let mut state = self.inner.state();
        let id = match id {
            Some(id) if state.records.contains_key(&id) => return Err(CoreError::TransferExists),
            Some(id) => id,
            None => std::iter::repeat_with(Uuid::new_v4)
                .find(|id| !state.records.contains_key(id))
                .expect("the iterator is endless"),
        };
        let now = unix_millis();
        let transfer = Transfer::new(TransferSnapshot {
            id,
            device_id: device.device_id.clone(),
            device_name: device.device_name.clone(),
            direction,
            status: TransferStatus::Queued,
            file_name,
            total_bytes,
            transferred_bytes: 0,
            created_at: now,
            updated_at: now,
            error_code: None,
            saved_path: None,
        });
        let started = transfer.snapshot();
        let cancellation = CancellationToken::new();
        state.records.insert(id, transfer);
        state.active.insert(
            id,
            Active {
                device_id: device.device_id.clone(),
                cancellation: cancellation.clone(),
                task: None,
            },
        );
        drop(state);
        self.inner.publish(EventData::TransferStarted(started));
        Ok(TransferHandle {
            id,
            total_bytes,
            inner: self.inner.clone(),
            cancellation,
            last_progress_event: None,
            finished: false,
        })
    }

    /// Every transfer, oldest first by id.
    pub fn list(&self) -> Vec<TransferSnapshot> {
        self.inner
            .state()
            .records
            .values()
            .map(Transfer::snapshot)
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Option<TransferSnapshot> {
        self.inner.state().records.get(&id).map(Transfer::snapshot)
    }

    /// Ask a transfer to stop. Returns its snapshot as it is now: the
    /// transfer's task tears down its socket and partial file and marks it
    /// `cancelled` once it notices.
    pub fn cancel(&self, id: Uuid) -> Result<TransferSnapshot, CoreError> {
        let state = self.inner.state();
        let snapshot = state
            .records
            .get(&id)
            .map(Transfer::snapshot)
            .ok_or(CoreError::UnknownTransfer)?;
        if snapshot.status.is_terminal() {
            return Err(CoreError::InvalidTransferState);
        }
        if let Some(active) = state.active.get(&id) {
            active.cancellation.cancel();
        }
        Ok(snapshot)
    }

    /// Ask every transfer with `device_id` to stop, e.g. because its
    /// connection closed.
    pub(super) fn cancel_device(&self, device_id: &str) {
        for active in self.inner.state().active.values() {
            if active.device_id == device_id {
                active.cancellation.cancel();
            }
        }
    }

    /// Cancel every running transfer and give their tasks until `deadline`
    /// to clean up their sockets and partial files, aborting any that take
    /// longer. Called during daemon shutdown.
    pub(super) async fn shutdown(&self, deadline: Duration) {
        let active: Vec<Active> = self
            .inner
            .state()
            .active
            .drain()
            .map(|(_, active)| active)
            .collect();
        for transfer in &active {
            transfer.cancellation.cancel();
        }
        let until = tokio::time::Instant::now() + deadline;
        for task in active.into_iter().filter_map(|transfer| transfer.task) {
            let abort = task.abort_handle();
            if tokio::time::timeout_at(until, task).await.is_err() {
                abort.abort();
            }
        }
    }
}

/// A transfer in progress, held by the code moving its bytes. Ending it
/// ([`Self::complete`], [`Self::fail`], [`Self::cancelled`],
/// [`Self::finish`]) consumes the handle; dropping it without doing so
/// fails the transfer, or cancels it if cancellation was asked for.
pub struct TransferHandle {
    id: Uuid,
    total_bytes: u64,
    inner: Arc<Inner>,
    cancellation: CancellationToken,
    last_progress_event: Option<Instant>,
    finished: bool,
}

impl TransferHandle {
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// The transfer as clients see it now.
    pub fn snapshot(&self) -> TransferSnapshot {
        self.inner
            .state()
            .records
            .get(&self.id)
            .map(Transfer::snapshot)
            .expect("transfer records are never removed")
    }

    /// Cancelled when the user cancels the transfer, its device
    /// disconnects, or the daemon shuts down. Every blocking step of the
    /// transfer should race it.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Mark the transfer as setting up its connection.
    pub fn connecting(&self) {
        self.transition(TransferStatus::Connecting);
    }

    /// Mark the transfer as moving bytes; progress is recorded from now on.
    pub fn transferring(&self) {
        self.transition(TransferStatus::Transferring);
    }

    fn transition(&self, next: TransferStatus) {
        let mut state = self.inner.state();
        if let Some(record) = state.records.get_mut(&self.id)
            && let Err(error) = record.transition(next, unix_millis(), None)
        {
            tracing::debug!(transfer_id = %self.id, %error, "transfer transition refused");
        }
    }

    /// Record that `transferred` bytes of the total have moved. The
    /// snapshot always has the latest count; `transfer.progress` is
    /// published at most once per [`PROGRESS_EVENT_INTERVAL`], and for the
    /// final byte.
    pub fn progress(&mut self, transferred: u64) {
        let snapshot = {
            let mut state = self.inner.state();
            let Some(record) = state.records.get_mut(&self.id) else {
                return;
            };
            match record.record_progress(transferred, unix_millis()) {
                Ok(snapshot) => snapshot,
                Err(_) => return,
            }
        };
        let now = Instant::now();
        let due = self
            .last_progress_event
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_EVENT_INTERVAL);
        if due || snapshot.transferred_bytes == snapshot.total_bytes {
            self.last_progress_event = Some(now);
            self.inner.publish(EventData::TransferProgress(snapshot));
        }
    }

    /// Copy exactly the transfer's total from `reader` to `writer` in
    /// bounded chunks, recording progress; stops early if the transfer is
    /// cancelled. Doesn't end the transfer.
    pub async fn copy<R, W>(&mut self, reader: &mut R, writer: &mut W) -> Result<(), PayloadError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let cancellation = self.cancellation.clone();
        let total = self.total_bytes;
        payload::copy_exact(reader, writer, total, &cancellation, |transferred| {
            self.progress(transferred)
        })
        .await
    }

    /// Write the chunks of an upload (see [`upload_channel`]) to `writer`,
    /// recording progress, until the sender is dropped. Fails if the upload
    /// is shorter or longer than the transfer's total, or is cancelled.
    /// Doesn't end the transfer.
    pub async fn forward<W>(
        &mut self,
        chunks: &mut mpsc::Receiver<Bytes>,
        writer: &mut W,
    ) -> Result<(), PayloadError>
    where
        W: AsyncWrite + Unpin,
    {
        let cancellation = self.cancellation.clone();
        let total = self.total_bytes;
        payload::forward_channel(chunks, writer, total, &cancellation, |transferred| {
            self.progress(transferred)
        })
        .await
    }

    /// Save what `reader` yields into the download directory as
    /// `file_name`, or a numbered variant if that is taken, and end the
    /// transfer. The bytes go to a hidden partial file first, which is
    /// renamed only once the whole file has arrived and removed otherwise.
    /// `file_name` must already be sanitized ([`sanitize_file_name`]).
    pub async fn save_to_downloads<R>(mut self, reader: &mut R, file_name: &str)
    where
        R: AsyncRead + Unpin,
    {
        // Read once, so the partial and the final file share a directory
        // even if the setting changes mid-transfer.
        let download_dir = self
            .inner
            .settings
            .lock()
            .ok()
            .map(|settings| settings.snapshot().download_dir);
        let Some(download_dir) = download_dir else {
            return self.fail(OperationErrorCode::Internal);
        };
        if tokio::fs::create_dir_all(&download_dir).await.is_err() {
            return self.fail(OperationErrorCode::Internal);
        }
        let partial = download_dir.join(format!(".{}.part", self.id));
        let mut file = match tokio::fs::File::create(&partial).await {
            Ok(file) => file,
            Err(_) => return self.fail(OperationErrorCode::Internal),
        };
        self.transferring();
        let result = self.copy(reader, &mut file).await;
        drop(file);
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return self.finish(Err(error));
        }
        let destination = unique_destination(&download_dir, file_name);
        match tokio::fs::rename(&partial, &destination).await {
            Ok(()) => self.complete(Some(
                std::path::absolute(&destination).unwrap_or(destination),
            )),
            Err(_) => {
                let _ = tokio::fs::remove_file(&partial).await;
                self.fail(OperationErrorCode::Internal);
            }
        }
    }

    /// End the transfer as completed; `saved_path` is where an incoming
    /// file was saved.
    pub fn complete(mut self, saved_path: Option<PathBuf>) {
        self.end(TransferStatus::Completed, None, saved_path);
    }

    pub fn fail(mut self, error_code: OperationErrorCode) {
        self.end(TransferStatus::Failed, Some(error_code), None);
    }

    /// End the transfer as cancelled, once its task has stopped.
    pub fn cancelled(mut self) {
        self.end(TransferStatus::Cancelled, None, None);
    }

    /// End the transfer according to how moving its bytes went: completed,
    /// cancelled, or failed with `connection_failed`.
    pub fn finish(self, result: Result<(), PayloadError>) {
        match result {
            Ok(()) => self.complete(None),
            Err(PayloadError::Cancelled) => self.cancelled(),
            Err(_) => self.fail(OperationErrorCode::ConnectionFailed),
        }
    }

    /// Run the transfer on a task of its own, which cancellation and
    /// shutdown can then wait for.
    pub fn spawn<F, Fut>(self, run: F)
    where
        F: FnOnce(TransferHandle) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = self.id;
        let inner = self.inner.clone();
        let task = tokio::spawn(run(self));
        // A transfer that already ended has left `active`, and its task
        // needs no waiting for.
        if let Some(active) = inner.state().active.get_mut(&id) {
            active.task = Some(task);
        }
    }

    /// Move the transfer to a terminal state and publish it. A state the
    /// transfer can't move to from where it is (e.g. completing one that
    /// never started transferring) fails it instead, so it always ends.
    fn end(
        &mut self,
        status: TransferStatus,
        error_code: Option<OperationErrorCode>,
        saved_path: Option<PathBuf>,
    ) {
        self.finished = true;
        let snapshot = {
            let mut state = self.inner.state();
            state.active.remove(&self.id);
            let Some(record) = state.records.get_mut(&self.id) else {
                return;
            };
            let now = unix_millis();
            let ended = match status {
                TransferStatus::Completed => record.complete(saved_path, now),
                _ => record.transition(status, now, error_code),
            };
            match ended {
                Ok(snapshot) => snapshot,
                Err(_) if !record.snapshot().status.is_terminal() => {
                    match record.transition(
                        TransferStatus::Failed,
                        now,
                        Some(OperationErrorCode::Internal),
                    ) {
                        Ok(snapshot) => snapshot,
                        Err(_) => return,
                    }
                }
                Err(_) => return,
            }
        };
        // There is no `transfer.cancelled` event: a cancellation is
        // published as `transfer.failed` with status `cancelled` and no
        // error code, and clients tell the two apart by the status.
        self.inner.publish(match snapshot.status {
            TransferStatus::Completed => EventData::TransferCompleted(snapshot),
            _ => EventData::TransferFailed(snapshot),
        });
    }
}

impl Drop for TransferHandle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.cancellation.is_cancelled() {
            self.end(TransferStatus::Cancelled, None, None);
        } else {
            self.end(
                TransferStatus::Failed,
                Some(OperationErrorCode::Internal),
                None,
            );
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum FileNameError {
    #[error("file name is empty, absolute, or a path traversal attempt")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::settings::SettingsDefaults,
        core::{DeviceReachability, DeviceSnapshot},
        protocol::DeviceType,
    };

    /// A transfers service saving into `download_dir`, on a bus that holds
    /// `capacity` events.
    fn transfers(download_dir: &Path, capacity: usize) -> Transfers {
        let settings = Settings::new(SettingsDefaults {
            device_name: "Ferry".into(),
            download_dir: download_dir.to_owned(),
        });
        Transfers::new(
            TransferConfig::new(download_dir.to_owned()),
            EventBus::new(capacity).unwrap(),
            Arc::new(Mutex::new(settings)),
        )
    }

    fn device(device_id: &str) -> DeviceSnapshot {
        DeviceSnapshot {
            device_id: device_id.into(),
            device_name: "Peer".into(),
            device_type: DeviceType::Phone,
            protocol_version: 8,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            reachability: DeviceReachability::Connected,
            paired: true,
            pairing: false,
            last_seen_at: 1,
            plugins: Default::default(),
            addresses: Vec::new(),
        }
    }

    fn status(transfers: &Transfers, id: Uuid) -> TransferStatus {
        transfers.get(id).unwrap().status
    }

    #[test]
    fn progress_events_are_throttled_but_the_final_byte_is_published() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 1);
        let mut transfer = transfers.begin(
            &device("peer"),
            TransferDirection::Incoming,
            "f".into(),
            100,
        );
        transfer.transferring();
        let mut events = transfers.inner.events.subscribe();

        let mut published = Vec::new();
        for transferred in 1..=100 {
            transfer.progress(transferred);
            // The bus holds one event, so drain it after every report.
            while let Ok(event) = events.try_recv() {
                match event.event {
                    EventData::TransferProgress(snapshot) => {
                        published.push(snapshot.transferred_bytes);
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
        }

        assert_eq!(published, vec![1, 100]);
        assert_eq!(transfer.snapshot().transferred_bytes, 100);
    }

    #[test]
    fn a_client_chosen_id_is_used_once() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 8);
        let id = Uuid::new_v4();
        let transfer = transfers
            .begin_as(
                Some(id),
                &device("peer"),
                TransferDirection::Outgoing,
                "a".into(),
                1,
            )
            .unwrap();
        assert_eq!(transfer.id(), id);
        transfer.cancelled();
        assert!(matches!(
            transfers.begin_as(
                Some(id),
                &device("peer"),
                TransferDirection::Outgoing,
                "b".into(),
                1
            ),
            Err(CoreError::TransferExists)
        ));
        assert_eq!(transfers.get(id).unwrap().file_name, "a");
    }

    #[test]
    fn a_transfer_is_announced_when_it_begins_and_when_it_ends() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 8);
        let mut events = transfers.inner.events.subscribe();
        let transfer = transfers.begin(&device("peer"), TransferDirection::Outgoing, "f".into(), 0);
        let id = transfer.id();
        transfer.transferring();
        transfer.complete(None);

        assert!(matches!(
            events.try_recv().unwrap().event,
            EventData::TransferStarted(snapshot) if snapshot.status == TransferStatus::Queued
        ));
        assert!(matches!(
            events.try_recv().unwrap().event,
            EventData::TransferCompleted(snapshot) if snapshot.id == id
        ));
        assert!(matches!(
            transfers.cancel(id),
            Err(CoreError::InvalidTransferState)
        ));
        assert!(matches!(
            transfers.cancel(Uuid::nil()),
            Err(CoreError::UnknownTransfer)
        ));
    }

    #[test]
    fn a_handle_dropped_without_ending_its_transfer_fails_or_cancels_it() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 8);
        let dropped = transfers.begin(&device("peer"), TransferDirection::Incoming, "a".into(), 1);
        let dropped_id = dropped.id();
        drop(dropped);
        let failed = transfers.get(dropped_id).unwrap();
        assert_eq!(failed.status, TransferStatus::Failed);
        assert_eq!(failed.error_code, Some(OperationErrorCode::Internal));

        let cancelled =
            transfers.begin(&device("peer"), TransferDirection::Incoming, "b".into(), 1);
        let cancelled_id = cancelled.id();
        transfers.cancel(cancelled_id).unwrap();
        drop(cancelled);
        assert_eq!(status(&transfers, cancelled_id), TransferStatus::Cancelled);

        // Completing a transfer that never started moving bytes can't
        // succeed; it still ends.
        let early = transfers.begin(&device("peer"), TransferDirection::Incoming, "c".into(), 1);
        let early_id = early.id();
        early.complete(None);
        assert_eq!(status(&transfers, early_id), TransferStatus::Failed);
        assert!(transfers.inner.state().active.is_empty());
    }

    #[test]
    fn a_disconnect_cancels_only_that_devices_transfers() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 8);
        let one = transfers.begin(&device("one"), TransferDirection::Incoming, "a".into(), 1);
        let other = transfers.begin(&device("other"), TransferDirection::Incoming, "b".into(), 1);
        transfers.cancel_device("one");
        assert!(one.cancellation().is_cancelled());
        assert!(!other.cancellation().is_cancelled());
    }

    #[tokio::test]
    async fn shutdown_cancels_running_transfers_and_waits_for_them() {
        let directory = tempfile::tempdir().unwrap();
        let transfers = transfers(directory.path(), 8);
        let transfer = transfers.begin(&device("peer"), TransferDirection::Outgoing, "a".into(), 1);
        let id = transfer.id();
        transfer.spawn(|transfer| async move {
            transfer.cancellation().cancelled().await;
            // Cleaning up takes a moment; shutdown waits for it.
            tokio::task::yield_now().await;
            transfer.cancelled();
        });

        transfers.shutdown(Duration::from_secs(5)).await;
        assert_eq!(status(&transfers, id), TransferStatus::Cancelled);
    }

    #[tokio::test]
    async fn downloads_are_saved_whole_or_not_at_all() {
        let directory = tempfile::tempdir().unwrap();
        let downloads = directory.path().join("downloads");
        let transfers = transfers(&downloads, 8);

        let saved = transfers.begin(
            &device("peer"),
            TransferDirection::Incoming,
            "a.txt".into(),
            5,
        );
        let saved_id = saved.id();
        saved.save_to_downloads(&mut &b"hello"[..], "a.txt").await;
        let completed = transfers.get(saved_id).unwrap();
        assert_eq!(completed.status, TransferStatus::Completed);
        assert_eq!(completed.transferred_bytes, 5);
        let path = completed.saved_path.unwrap();
        assert_eq!(path, downloads.join("a.txt"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        // The source ends early: nothing is left behind.
        let short = transfers.begin(
            &device("peer"),
            TransferDirection::Incoming,
            "b.txt".into(),
            10,
        );
        let short_id = short.id();
        short.save_to_downloads(&mut &b"hello"[..], "b.txt").await;
        let failed = transfers.get(short_id).unwrap();
        assert_eq!(failed.status, TransferStatus::Failed);
        assert_eq!(
            failed.error_code,
            Some(OperationErrorCode::ConnectionFailed)
        );
        let names: Vec<_> = std::fs::read_dir(&downloads)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.txt"]);
    }

    #[test]
    fn sanitize_strips_directory_components() {
        assert_eq!(sanitize_file_name("photo.jpg").unwrap(), "photo.jpg");
        assert_eq!(sanitize_file_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(sanitize_file_name("/etc/shadow").unwrap(), "shadow");
        assert_eq!(
            sanitize_file_name("a/b/c/report.pdf").unwrap(),
            "report.pdf"
        );
    }

    #[test]
    fn sanitize_rejects_degenerate_names() {
        assert_eq!(sanitize_file_name(""), Err(FileNameError::Invalid));
        assert_eq!(sanitize_file_name("."), Err(FileNameError::Invalid));
        assert_eq!(sanitize_file_name(".."), Err(FileNameError::Invalid));
        assert_eq!(sanitize_file_name("../.."), Err(FileNameError::Invalid));
        assert_eq!(sanitize_file_name("a/.."), Err(FileNameError::Invalid));
        assert_eq!(sanitize_file_name("bad\0name"), Err(FileNameError::Invalid));
    }

    #[test]
    fn unique_destination_avoids_overwriting_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let first = unique_destination(directory.path(), "photo.jpg");
        assert_eq!(first, directory.path().join("photo.jpg"));
        std::fs::write(&first, b"existing").unwrap();

        let second = unique_destination(directory.path(), "photo.jpg");
        assert_eq!(second, directory.path().join("photo (1).jpg"));
        std::fs::write(&second, b"existing").unwrap();

        let third = unique_destination(directory.path(), "photo.jpg");
        assert_eq!(third, directory.path().join("photo (2).jpg"));
    }

    fn transfer(status: TransferStatus) -> Transfer {
        Transfer::new(TransferSnapshot {
            id: Uuid::nil(),
            device_id: "740bd4b9b4184ee497d6caf1da8151be".into(),
            device_name: "FOSS Phone".into(),
            direction: TransferDirection::Outgoing,
            status,
            file_name: "photo.jpg".into(),
            total_bytes: 10,
            transferred_bytes: 0,
            created_at: 100,
            updated_at: 100,
            error_code: None,
            saved_path: None,
        })
    }

    #[test]
    fn transfer_rejects_invalid_transitions_and_progress() {
        let mut transfer = transfer(TransferStatus::Queued);
        assert!(
            transfer
                .transition(TransferStatus::Completed, 101, None)
                .is_err()
        );
        transfer
            .transition(TransferStatus::Transferring, 102, None)
            .unwrap();
        transfer.record_progress(8, 103).unwrap();
        assert!(matches!(
            transfer.record_progress(7, 104),
            Err(TransferProgressError::InvalidByteCount { .. })
        ));
        assert!(transfer.record_progress(11, 104).is_err());
    }

    #[test]
    fn transfer_snapshots_use_stable_camel_case_json_names() {
        let transfer_json =
            serde_json::to_value(transfer(TransferStatus::Queued).snapshot()).unwrap();
        assert_eq!(transfer_json["fileName"], "photo.jpg");
        assert_eq!(transfer_json["totalBytes"], 10);
    }
}

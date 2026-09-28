//! Browse: a paired device's files, over the SFTP server it runs
//! (ARCHITECTURE.md §12).
//!
//! The first request for a device opens a session with it (see
//! [`session`]), which later requests share. Listing, previews, and
//! creating, moving and deleting files work on the session directly;
//! downloads and uploads run as transfers of the core's transfers service,
//! so they are listed, report progress and can be cancelled through
//! `/transfers` like any other.
//!
//! Browsing is one-way: this build asks devices to serve their files
//! (`kdeconnect.sftp.request`) but serves none.
//!
//! Never log a file name, a path, or the offer's password.

pub(crate) mod files;
mod http;
pub mod packet;
mod session;
mod ssh;

use std::{
    io,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use russh_sftp::{
    client::{error::Error as SftpStatusError, fs::Metadata},
    protocol::{FileType, OpenFlags, StatusCode},
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use uuid::Uuid;

pub use files::{DirectoryListing, FileEntry, FileKind};
pub use packet::{
    PACKET_TYPE, REQUEST_PACKET_TYPE, SftpBody, SftpReply, SftpRequestBody, SftpRoot,
    build_request_packet,
};
pub use session::RemoteFileContent;

use files::{
    join_remote_path, normalize_remote_path, numbered_name, split_remote_path, validate_remote_name,
};
use session::{RemoteSession, Sessions};

use crate::{
    core::{
        CoreError, DeviceSnapshot, OperationErrorCode, Plugin, PluginContext, TransferDirection,
        TransferHandle, TransferSnapshot, forward_reader, sanitize_file_name, upload_channel,
    },
    protocol::Packet,
    transport::payload::PayloadError,
};

/// The plugin's id.
pub const ID: &str = "browse";

/// Browsing a device's files, and the session open with each device.
#[derive(Default)]
pub struct BrowsePlugin {
    sessions: Sessions,
}

#[async_trait::async_trait]
impl Plugin for BrowsePlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[REQUEST_PACKET_TYPE]
    }

    async fn handle_packet(&self, _ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let device_id = device.device_id.as_str();
        match packet
            .body_as::<SftpBody>()
            .ok()
            .and_then(|body| body.reply())
        {
            Some(reply) => self.sessions.handle_reply(device_id, reply),
            None => tracing::debug!(device_id, "dropping unrecognized sftp packet"),
        }
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(self, ctx)
    }

    fn streaming_routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::streaming_routes(self, ctx)
    }

    async fn disconnected(&self, _ctx: &PluginContext, device_id: &str) {
        self.sessions.close(device_id);
    }

    async fn unpaired(&self, _ctx: &PluginContext, device_id: &str) {
        self.sessions.close(device_id);
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.sessions.shutdown())
    }
}

/// Why a local file couldn't be uploaded to a device.
#[derive(Debug, Error)]
pub enum UploadPathError {
    /// The device or the core refused the upload.
    #[error(transparent)]
    Browse(#[from] BrowseError),
    /// The file couldn't be opened, or isn't a regular file.
    #[error("the file couldn't be read")]
    File(#[source] io::Error),
}

/// Why a request to browse a device's files failed.
#[derive(Debug, Error)]
pub enum BrowseError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("remote path must be absolute, without `.` or `..` segments")]
    InvalidPath,
    #[error("the device isn't sharing its files")]
    Unavailable { reason: Option<String> },
    #[error("no such file or directory on the device")]
    NotFound,
    #[error("a file or directory of that name already exists on the device")]
    Exists,
    #[error("the device denied access to that file or directory")]
    PermissionDenied,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("the device's file server doesn't hold the device's paired key")]
    HostKeyMismatch,
    #[error("browsing the device's files failed")]
    Failed,
    #[error("the device took too long to answer")]
    TimedOut,
}

impl BrowseError {
    /// The code clients see for this error, as [`CoreError::code`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Core(error) => error.code(),
            Self::InvalidPath => "invalid_path",
            Self::Unavailable { .. } => "files_unavailable",
            Self::NotFound => "file_not_found",
            Self::Exists => "file_exists",
            Self::PermissionDenied => "file_permission_denied",
            Self::NotADirectory => "not_a_directory",
            Self::IsADirectory => "is_a_directory",
            Self::HostKeyMismatch => "files_host_key_mismatch",
            Self::Failed => "files_failed",
            Self::TimedOut => "files_timed_out",
        }
    }
}

impl BrowsePlugin {
    /// List a directory on a device, or, with no `path`, the storage roots
    /// it exposes.
    pub async fn list_files(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        path: Option<&str>,
    ) -> Result<DirectoryListing, BrowseError> {
        let session = self.sessions.get(ctx, device_id).await?;
        let Some(path) = path else {
            return Ok(DirectoryListing {
                path: None,
                entries: session
                    .roots
                    .iter()
                    .filter_map(|root| {
                        Some(FileEntry {
                            name: root.name.clone(),
                            path: normalize_remote_path(&root.path).ok()?,
                            kind: FileKind::Directory,
                            size: None,
                            modified_at: None,
                        })
                    })
                    .collect(),
            });
        };
        let path = normalize_path(path)?;
        let entries = match session.sftp().read_dir(path.as_str()).await {
            Ok(entries) => entries,
            Err(error) => {
                // Reading a file as a directory fails with a generic status;
                // tell that case apart.
                if let Ok(metadata) = session.sftp().metadata(path.as_str()).await
                    && !metadata.is_dir()
                {
                    return Err(BrowseError::NotADirectory);
                }
                return Err(self.remote_error(device_id, &session, error));
            }
        };
        let mut listed = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            let entry_path = join_remote_path(&path, &name);
            let mut metadata = entry.metadata();
            // Show a link as what it points to, so a linked directory can be
            // opened; a dangling link stays a link.
            if metadata.file_type() == FileType::Symlink
                && let Ok(target) = session.sftp().metadata(entry_path.as_str()).await
            {
                metadata = target;
            }
            listed.push(file_entry(entry_path, name, &metadata));
        }
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(DirectoryListing {
            path: Some(path),
            entries: listed,
        })
    }

    /// Open a file on a device for reading, e.g. to show a preview.
    pub async fn open_file(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        path: &str,
    ) -> Result<RemoteFileContent, BrowseError> {
        let path = normalize_path(path)?;
        let (_, name) = split_remote_path(&path).ok_or(BrowseError::IsADirectory)?;
        let session = self.sessions.get(ctx, device_id).await?;
        let size = self.file_size(device_id, &session, &path).await?;
        let file = session
            .sftp()
            .open(path.as_str())
            .await
            .map_err(|error| self.remote_error(device_id, &session, error))?;
        Ok(RemoteFileContent {
            name: name.to_owned(),
            size,
            file,
            _session: session,
        })
    }

    /// Start downloading a file from a device into the download directory,
    /// as an incoming transfer.
    pub async fn download(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        path: &str,
    ) -> Result<TransferSnapshot, BrowseError> {
        let path = normalize_path(path)?;
        let (_, name) = split_remote_path(&path).ok_or(BrowseError::IsADirectory)?;
        let file_name = sanitize_file_name(name).map_err(|_| CoreError::InvalidFileName)?;
        let session = self.sessions.get(ctx, device_id).await?;
        let total = self.file_size(device_id, &session, &path).await?;
        let transfers = ctx.transfers();
        let limit = transfers.max_bytes();
        if total > limit {
            return Err(CoreError::TransferTooLarge { limit }.into());
        }
        let device = ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;
        let transfer = transfers.begin(
            &device,
            TransferDirection::Incoming,
            file_name.clone(),
            total,
        );
        let started = transfer.snapshot();
        transfer.spawn(move |transfer| run_download(transfer, session, path, file_name));
        Ok(started)
    }

    /// Start uploading a file into `directory` on a device, as an outgoing
    /// transfer. The file is created before this returns, under `file_name`
    /// or, if that is taken, a numbered variant of it, so an upload never
    /// replaces an existing file. The caller streams the content into the
    /// returned sender; an upload that ends early is removed from the
    /// device. The transfer gets `id` if the client chose one.
    pub async fn upload(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        directory: &str,
        file_name: &str,
        declared_size: u64,
        id: Option<Uuid>,
    ) -> Result<(TransferSnapshot, mpsc::Sender<Bytes>), BrowseError> {
        let directory = normalize_path(directory)?;
        validate_remote_name(file_name).map_err(|_| CoreError::InvalidFileName)?;
        let limit = ctx.transfers().max_bytes();
        if declared_size > limit {
            return Err(CoreError::TransferTooLarge { limit }.into());
        }
        // Checked again when the transfer begins; this spares creating a
        // file for a request that would be refused.
        if id.is_some_and(|id| ctx.transfers().get(id).is_some()) {
            return Err(CoreError::TransferExists.into());
        }
        let device = ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;
        let session = self.sessions.get(ctx, device_id).await?;
        let metadata = session
            .sftp()
            .metadata(directory.as_str())
            .await
            .map_err(|error| self.remote_error(device_id, &session, error))?;
        if !metadata.is_dir() {
            return Err(BrowseError::NotADirectory);
        }

        let (path, mut remote) = self
            .create_unique_file(device_id, &session, &directory, file_name)
            .await?;
        let (_, final_name) = split_remote_path(&path).expect("created under a directory");
        let transfer = match ctx.transfers().begin_as(
            id,
            &device,
            TransferDirection::Outgoing,
            final_name.to_owned(),
            declared_size,
        ) {
            Ok(transfer) => transfer,
            Err(error) => {
                let _ = remote.shutdown().await;
                let _ = session.sftp().remove_file(path.as_str()).await;
                return Err(error.into());
            }
        };
        let started = transfer.snapshot();
        let (chunk_tx, chunk_rx) = upload_channel();
        transfer.spawn(move |transfer| run_upload(transfer, session, path, remote, chunk_rx));
        Ok((started, chunk_tx))
    }

    /// Upload the local file at `path` into `directory` on a device, under
    /// its own name: an [`upload`](Self::upload) of its size, streamed from
    /// disk. Resolves once the whole file has gone into the transfer, or the
    /// transfer has ended first, with the transfer as it is then, as
    /// `POST /devices/{id}/files/upload` answers. Run it on the daemon's
    /// runtime.
    pub async fn upload_path(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        directory: &str,
        path: &Path,
    ) -> Result<TransferSnapshot, UploadPathError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(UploadPathError::File)?;
        let metadata = file.metadata().await.map_err(UploadPathError::File)?;
        if !metadata.is_file() {
            return Err(UploadPathError::File(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            )));
        }
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = metadata.len();
        let (started, sender) = self
            .upload(ctx, device_id, directory, &file_name, size, None)
            .await?;
        // A file that grows meanwhile uploads what it had; one that shrinks,
        // or fails part-way, fails the transfer as short.
        if let Err(error) = forward_reader(file.take(size), sender).await {
            tracing::debug!(device_id, %error, "reading a file to upload failed");
        }
        Ok(ctx.transfers().get(started.id).unwrap_or(started))
    }

    /// Create `name` in `directory` without replacing anything: on a
    /// collision, try `name (1)`, `name (2)`, and so on.
    async fn create_unique_file(
        &self,
        device_id: &str,
        session: &Arc<RemoteSession>,
        directory: &str,
        name: &str,
    ) -> Result<(String, russh_sftp::client::fs::File), BrowseError> {
        const MAX_ATTEMPTS: u32 = 1000;
        for attempt in 0..MAX_ATTEMPTS {
            let candidate = if attempt == 0 {
                name.to_owned()
            } else {
                numbered_name(name, attempt)
            };
            let path = join_remote_path(directory, &candidate);
            // SFTP v3 servers report "already exists" as a generic failure,
            // so check first; `EXCLUDE` still guards against a race.
            if self.exists(device_id, session, &path).await? {
                continue;
            }
            return match session
                .sftp()
                .open_with_flags(
                    path.as_str(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await
            {
                Ok(file) => Ok((path, file)),
                Err(error) => Err(self.remote_error(device_id, session, error)),
            };
        }
        Err(BrowseError::Exists)
    }

    /// Create a directory on a device.
    pub async fn create_directory(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        path: &str,
    ) -> Result<FileEntry, BrowseError> {
        let path = normalize_path(path)?;
        split_remote_path(&path).ok_or(BrowseError::Exists)?;
        let session = self.sessions.get(ctx, device_id).await?;
        if self.exists(device_id, &session, &path).await? {
            return Err(BrowseError::Exists);
        }
        session
            .sftp()
            .create_dir(path.as_str())
            .await
            .map_err(|error| self.remote_error(device_id, &session, error))?;
        self.entry(device_id, &session, path).await
    }

    /// Move or rename a file or directory on a device. Never replaces an
    /// existing file.
    pub async fn move_file(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        from: &str,
        to: &str,
    ) -> Result<FileEntry, BrowseError> {
        let from = normalize_path(from)?;
        let to = normalize_path(to)?;
        let session = self.sessions.get(ctx, device_id).await?;
        if is_protected(&session, &from) || split_remote_path(&to).is_none() {
            return Err(BrowseError::InvalidPath);
        }
        if self.exists(device_id, &session, &to).await? {
            return Err(BrowseError::Exists);
        }
        session
            .sftp()
            .rename(from.as_str(), to.as_str())
            .await
            .map_err(|error| self.remote_error(device_id, &session, error))?;
        self.entry(device_id, &session, to).await
    }

    /// Delete a file, or a directory with everything in it, on a device.
    /// Symbolic links are removed, never followed. The storage roots
    /// themselves can't be deleted.
    pub async fn delete(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        path: &str,
    ) -> Result<(), BrowseError> {
        let path = normalize_path(path)?;
        let session = self.sessions.get(ctx, device_id).await?;
        if is_protected(&session, &path) {
            return Err(BrowseError::InvalidPath);
        }
        let sftp = session.sftp();
        let remote_error = |error| self.remote_error(device_id, &session, error);

        // Depth first: a directory is removed once everything under it has
        // been.
        let mut pending = vec![(path, false)];
        while let Some((path, visited)) = pending.pop() {
            let metadata = sftp
                .symlink_metadata(path.as_str())
                .await
                .map_err(remote_error)?;
            if !metadata.is_dir() {
                sftp.remove_file(path.as_str())
                    .await
                    .map_err(remote_error)?;
            } else if visited {
                sftp.remove_dir(path.as_str()).await.map_err(remote_error)?;
            } else {
                let children = sftp.read_dir(path.as_str()).await.map_err(remote_error)?;
                pending.push((path.clone(), true));
                pending.extend(
                    children.map(|entry| (join_remote_path(&path, &entry.file_name()), false)),
                );
            }
            session.touch();
        }
        Ok(())
    }

    async fn exists(
        &self,
        device_id: &str,
        session: &Arc<RemoteSession>,
        path: &str,
    ) -> Result<bool, BrowseError> {
        match session.sftp().symlink_metadata(path).await {
            Ok(_) => Ok(true),
            Err(SftpStatusError::Status(status))
                if status.status_code == StatusCode::NoSuchFile =>
            {
                Ok(false)
            }
            Err(error) => Err(self.remote_error(device_id, session, error)),
        }
    }

    async fn entry(
        &self,
        device_id: &str,
        session: &Arc<RemoteSession>,
        path: String,
    ) -> Result<FileEntry, BrowseError> {
        let metadata = session
            .sftp()
            .symlink_metadata(path.as_str())
            .await
            .map_err(|error| self.remote_error(device_id, session, error))?;
        let name = split_remote_path(&path)
            .map(|(_, name)| name.to_owned())
            .unwrap_or_else(|| "/".to_owned());
        Ok(file_entry(path, name, &metadata))
    }

    /// The size of the regular file at `path`, following links.
    async fn file_size(
        &self,
        device_id: &str,
        session: &Arc<RemoteSession>,
        path: &str,
    ) -> Result<u64, BrowseError> {
        let metadata = session
            .sftp()
            .metadata(path)
            .await
            .map_err(|error| self.remote_error(device_id, session, error))?;
        if metadata.is_dir() {
            return Err(BrowseError::IsADirectory);
        }
        metadata.size.ok_or(BrowseError::Failed)
    }

    /// Map an SFTP failure to a browse error. A failure that ended the
    /// connection also drops the session, so the next request opens a new
    /// one.
    fn remote_error(
        &self,
        device_id: &str,
        session: &RemoteSession,
        error: SftpStatusError,
    ) -> BrowseError {
        match error {
            SftpStatusError::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => BrowseError::NotFound,
                StatusCode::PermissionDenied => BrowseError::PermissionDenied,
                _ => BrowseError::Failed,
            },
            SftpStatusError::Timeout => {
                self.drop_session_if_closed(device_id, session);
                BrowseError::TimedOut
            }
            _ => {
                self.drop_session_if_closed(device_id, session);
                BrowseError::Failed
            }
        }
    }

    fn drop_session_if_closed(&self, device_id: &str, session: &RemoteSession) {
        if session.is_closed() {
            self.sessions.close(device_id);
        }
    }
}

/// Whether `path` is `/` or one of the device's storage roots, which the
/// user may browse but not move or delete.
fn is_protected(session: &RemoteSession, path: &str) -> bool {
    path == "/"
        || session
            .roots
            .iter()
            .filter_map(|root| normalize_remote_path(&root.path).ok())
            .any(|root| root == path)
}

/// Copy a file from a device into the download directory.
async fn run_download(
    transfer: TransferHandle,
    session: Arc<RemoteSession>,
    path: String,
    file_name: String,
) {
    let cancellation = transfer.cancellation();
    let mut remote = tokio::select! {
        _ = cancellation.cancelled() => return transfer.cancelled(),
        result = session.sftp().open(path.as_str()) => match result {
            Ok(file) => file,
            Err(_) => return transfer.fail(OperationErrorCode::ConnectionFailed),
        },
    };
    transfer.save_to_downloads(&mut remote, &file_name).await;
    let _ = remote.shutdown().await;
    session.touch();
}

/// Write an upload into the file created for it on a device, removing the
/// file if the upload doesn't finish.
async fn run_upload(
    mut transfer: TransferHandle,
    session: Arc<RemoteSession>,
    path: String,
    mut remote: russh_sftp::client::fs::File,
    mut chunks: mpsc::Receiver<Bytes>,
) {
    transfer.transferring();
    let mut result = transfer.forward(&mut chunks, &mut remote).await;
    // Closing the handle is when a server reports a failed write.
    let closed = remote.shutdown().await;
    if result.is_ok()
        && let Err(error) = closed
    {
        result = Err(PayloadError::Socket(error));
    }
    if result.is_err() {
        let _ = session.sftp().remove_file(path.as_str()).await;
    }
    session.touch();
    transfer.finish(result);
}

fn normalize_path(path: &str) -> Result<String, BrowseError> {
    normalize_remote_path(path).map_err(|_| BrowseError::InvalidPath)
}

fn file_entry(path: String, name: String, metadata: &Metadata) -> FileEntry {
    let kind = match metadata.file_type() {
        FileType::Dir => FileKind::Directory,
        FileType::File => FileKind::File,
        FileType::Symlink => FileKind::Symlink,
        FileType::Other => FileKind::Other,
    };
    FileEntry {
        name,
        path,
        kind,
        size: (kind == FileKind::File).then_some(metadata.size).flatten(),
        modified_at: metadata.mtime.map(|seconds| u64::from(seconds) * 1000),
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        Core,
        testing::{handle_with_plugin, make_identity},
    };

    const PHONE: &str = "740bd4b9b4184ee497d6caf1da8151be";

    /// A paired, connected device advertising `capabilities`; its packets
    /// arrive on the returned receiver.
    async fn phone(handle: &Core, capabilities: &[&str]) -> mpsc::Receiver<Packet> {
        let identity = make_identity(
            PHONE,
            capabilities.iter().map(|value| value.to_string()).collect(),
        );
        handle.discover_device(&identity, true, 1).unwrap();
        let (tx, rx) = mpsc::channel(4);
        handle
            .register_connection(PHONE, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        rx
    }

    #[tokio::test]
    async fn only_devices_that_serve_files_are_asked() {
        let (handle, plugin, _commands) = handle_with_plugin(BrowsePlugin::default()).await;
        let ctx = handle.plugin_context();
        assert!(matches!(
            plugin.list_files(&ctx, PHONE, None).await,
            Err(BrowseError::Core(CoreError::UnknownDevice))
        ));
        let mut packets = phone(&handle, &["kdeconnect.ping"]).await;
        assert!(matches!(
            plugin.list_files(&ctx, PHONE, None).await,
            Err(BrowseError::Core(CoreError::UnsupportedByPeer))
        ));
        assert!(packets.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_device_that_cannot_serve_says_why() {
        let (handle, plugin, _commands) = handle_with_plugin(BrowsePlugin::default()).await;
        let ctx = handle.plugin_context();
        let mut packets = phone(&handle, &[REQUEST_PACKET_TYPE]).await;

        let listing = tokio::spawn({
            let plugin = plugin.clone();
            let ctx = ctx.clone();
            async move { plugin.list_files(&ctx, PHONE, None).await }
        });
        let request = packets.recv().await.unwrap();
        assert_eq!(request.packet_type, REQUEST_PACKET_TYPE);
        let reply = Packet::from_body(
            2_u64,
            PACKET_TYPE,
            &json!({"errorMessage": "No storage permission"}),
        )
        .unwrap();
        handle.handle_peer_packet(PHONE, reply).await;

        match listing.await.unwrap() {
            Err(BrowseError::Unavailable { reason }) => {
                assert_eq!(reason.as_deref(), Some("No storage permission"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn waiting_for_an_offer_ends_when_the_device_disconnects() {
        let (handle, plugin, _commands) = handle_with_plugin(BrowsePlugin::default()).await;
        let ctx = handle.plugin_context();
        let mut packets = phone(&handle, &[REQUEST_PACKET_TYPE]).await;

        let listing = tokio::spawn({
            let plugin = plugin.clone();
            let ctx = ctx.clone();
            async move { plugin.list_files(&ctx, PHONE, None).await }
        });
        packets.recv().await.unwrap();
        handle.unregister_connection(PHONE).await;

        assert!(matches!(
            listing.await.unwrap(),
            Err(BrowseError::Core(CoreError::DeviceNotConnected))
        ));
    }
}

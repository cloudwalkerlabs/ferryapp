//! Browse's control methods. Paths on the device are absolute; a local
//! file to upload is an absolute path on this machine.

use std::{path::PathBuf, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::io::AsyncReadExt;

use super::{BrowseError, BrowsePlugin, DirectoryListing, FileEntry, UploadPathError};
use crate::{
    core::{PluginContext, TransferSnapshot},
    plugins::share::rpc::not_absolute,
    rpc::{ErrorCode, Methods, StreamMethod, define_methods},
};

/// How much of a file each `files.read` item carries, before base64.
const READ_CHUNK_BYTES: usize = 64 * 1024;

define_methods! {
    /// List a directory on a paired device, or, without `path`, the
    /// storage roots it shares. The first request opens a browse session
    /// with the device, which can take a few seconds.
    "files.list" => ListFiles { device_id: String, #[serde(default)] path: Option<String> } -> DirectoryListing;
    /// Read a file's content from a paired device: base64 chunks as stream
    /// items, then the number of bytes sent. To keep a copy,
    /// `files.download` saves it as a transfer instead.
    "files.read" => ReadFile { device_id: String, path: String } -> u64;
    /// Save a file from a paired device into the download directory, as an
    /// incoming transfer; answers once it has started.
    "files.download" => DownloadFile { device_id: String, path: String } -> TransferSnapshot;
    /// Upload the local file at `path` into `directory` on a paired device,
    /// as an outgoing transfer. A name that is taken gets a ` (n)` suffix.
    /// Answers as `share.file` does.
    "files.upload" => UploadFile { device_id: String, directory: String, path: PathBuf } -> TransferSnapshot;
    /// Create a directory on a paired device.
    "files.mkdir" => CreateDirectory { device_id: String, path: String } -> FileEntry;
    /// Move or rename a file or directory on a paired device. Fails with
    /// `file_exists` rather than replacing anything.
    "files.move" => MoveFile { device_id: String, from: String, to: String } -> FileEntry;
    /// Delete a file, or a directory and everything in it, on a paired
    /// device.
    "files.delete" => DeleteFile { device_id: String, path: String } -> ();
}

impl StreamMethod for ReadFile {
    type Item = String;
}

impl ErrorCode for BrowseError {
    fn code(&self) -> &'static str {
        BrowseError::code(self)
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Unavailable { reason } => reason.clone(),
            _ => None,
        }
    }
}

impl ErrorCode for UploadPathError {
    fn code(&self) -> &'static str {
        match self {
            Self::Browse(error) => error.code(),
            Self::File(_) => "file_unreadable",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Browse(error) => ErrorCode::detail(error),
            Self::File(error) => Some(error.to_string()),
        }
    }
}

pub(super) fn add(plugin: Arc<BrowsePlugin>, ctx: PluginContext, methods: &mut Methods) {
    let state = || (plugin.clone(), ctx.clone());
    methods.add(
        state(),
        |(plugin, ctx), ListFiles { device_id, path }| async move {
            plugin.list_files(&ctx, &device_id, path.as_deref()).await
        },
    );
    methods.add_stream(
        state(),
        |(plugin, ctx), ReadFile { device_id, path }, items| async move {
            let mut content = plugin.open_file(&ctx, &device_id, &path).await?;
            let mut buffer = vec![0; READ_CHUNK_BYTES];
            let mut sent = 0;
            loop {
                let read = content
                    .read(&mut buffer)
                    .await
                    .map_err(|_| BrowseError::Failed)?;
                if read == 0 || !items.send(&STANDARD.encode(&buffer[..read])).await {
                    return Ok::<_, BrowseError>(sent);
                }
                sent += read as u64;
            }
        },
    );
    methods.add(
        state(),
        |(plugin, ctx), DownloadFile { device_id, path }| async move {
            plugin.download(&ctx, &device_id, &path).await
        },
    );
    methods.add(
        state(),
        |(plugin, ctx),
         UploadFile {
             device_id,
             directory,
             path,
         }| async move {
            if !path.is_absolute() {
                return Err(UploadPathError::File(not_absolute()));
            }
            plugin
                .upload_path(&ctx, &device_id, &directory, &path)
                .await
        },
    );
    methods.add(
        state(),
        |(plugin, ctx), CreateDirectory { device_id, path }| async move {
            plugin.create_directory(&ctx, &device_id, &path).await
        },
    );
    methods.add(
        state(),
        |(plugin, ctx),
         MoveFile {
             device_id,
             from,
             to,
         }| async move { plugin.move_file(&ctx, &device_id, &from, &to).await },
    );
    methods.add(
        state(),
        |(plugin, ctx), DeleteFile { device_id, path }| async move {
            plugin.delete(&ctx, &device_id, &path).await
        },
    );
}

//! Share's control methods.

use std::path::PathBuf;

use super::{SendPathError, ShareTextError, send_path, send_text, send_url};
use crate::{
    core::{PluginContext, TransferSnapshot},
    rpc::{ErrorCode, Methods, define_methods},
};

define_methods! {
    /// Send text to a paired, connected device.
    "share.text" => ShareText { device_id: String, text: String } -> ();
    /// Send a link to a paired, connected device, which opens it.
    "share.url" => ShareUrl { device_id: String, url: String } -> ();
    /// Send the file at `path`, an absolute path on this machine, to a
    /// paired device, under its own name. Answers once the whole file has
    /// gone into the transfer, or the transfer has ended first, with the
    /// transfer as it is then; a client that disconnects before then ends
    /// the transfer as short.
    "share.file" => ShareFile { device_id: String, path: PathBuf } -> TransferSnapshot;
}

impl ErrorCode for ShareTextError {
    fn code(&self) -> &'static str {
        ShareTextError::code(self)
    }
}

impl ErrorCode for SendPathError {
    fn code(&self) -> &'static str {
        match self {
            Self::Core(error) => error.code(),
            Self::File(_) => "file_unreadable",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Core(_) => None,
            Self::File(error) => Some(error.to_string()),
        }
    }
}

pub(super) fn add(ctx: PluginContext, methods: &mut Methods) {
    methods.add(
        ctx.clone(),
        |ctx, ShareText { device_id, text }| async move { send_text(&ctx, &device_id, text) },
    );
    methods.add(ctx.clone(), |ctx, ShareUrl { device_id, url }| async move {
        send_url(&ctx, &device_id, &url)
    });
    methods.add(ctx, |ctx, ShareFile { device_id, path }| async move {
        if !path.is_absolute() {
            return Err(SendPathError::File(not_absolute()));
        }
        send_path(&ctx, &device_id, &path).await
    });
}

/// A local path given over the socket must not depend on the daemon's
/// working directory.
pub(crate) fn not_absolute() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "the path must be absolute",
    )
}

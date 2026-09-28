//! The core's errors, and the failure codes it reports to clients.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{EventBusError, PairingTransitionError};
use crate::store::StoreError;

/// Public, non-sensitive failure categories safe to return to API clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationErrorCode {
    ConnectionFailed,
    ProtocolError,
    TimedOut,
    Unavailable,
    Internal,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("command queue capacity must be greater than zero")]
    InvalidCommandCapacity,
    #[error("core command queue is full")]
    CommandQueueFull,
    #[error("core command queue is closed")]
    CommandQueueClosed,
    #[error("core state is unavailable")]
    StateUnavailable,
    #[error("core event bus could not be created")]
    EventBus(#[from] EventBusError),
    #[error("unknown device")]
    UnknownDevice,
    #[error("discovery address must be a unicast IPv4 address")]
    InvalidDiscoveryAddress,
    #[error("device is already paired")]
    AlreadyPaired,
    #[error("a pairing session is already in progress for this device")]
    PairingInProgress,
    #[error("device does not have a live connection")]
    DeviceNotConnected,
    #[error("device is not paired")]
    NotPaired,
    #[error("peer has not advertised support for this packet type")]
    UnsupportedByPeer,
    #[error("peer certificate is invalid")]
    InvalidPeerCertificate,
    #[error("unknown pairing")]
    UnknownPairing,
    #[error("this pairing direction does not accept local confirmation")]
    InvalidPairingDirection,
    #[error("pairing is not in a state that allows this operation")]
    InvalidPairingState,
    #[error("invalid pairing transition")]
    InvalidTransition(#[from] PairingTransitionError),
    #[error("store operation failed")]
    Store(#[source] StoreError),
    #[error("file name must not be empty")]
    InvalidFileName,
    #[error("declared transfer size exceeds the {limit}-byte limit")]
    TransferTooLarge { limit: u64 },
    #[error("unknown transfer")]
    UnknownTransfer,
    #[error("a transfer with this id already exists")]
    TransferExists,
    #[error("device name must be 1 to 32 characters without reserved punctuation")]
    InvalidDeviceName,
    #[error("download directory must be an absolute path that can be created")]
    InvalidDownloadDir,
    #[error("a setting's value is invalid")]
    InvalidSettings,
    #[error("transfer is not in a state that allows this operation")]
    InvalidTransferState,
    #[error("internal core error")]
    Internal,
}

impl CoreError {
    /// The code clients see for this error: the HTTP API's `problem+json`
    /// `code`, and what the UI words its message from.
    pub fn code(&self) -> &'static str {
        match self {
            Self::CommandQueueFull => "command_queue_full",
            Self::CommandQueueClosed => "application_unavailable",
            Self::UnknownDevice => "device_not_found",
            Self::InvalidDiscoveryAddress => "invalid_address",
            Self::UnknownPairing => "pairing_not_found",
            Self::AlreadyPaired => "already_paired",
            Self::PairingInProgress => "pairing_in_progress",
            Self::DeviceNotConnected => "device_not_connected",
            Self::InvalidPairingDirection => "invalid_pairing_direction",
            Self::InvalidPairingState | Self::InvalidTransition(_) => "invalid_pairing_state",
            Self::NotPaired => "device_not_paired",
            Self::UnsupportedByPeer => "unsupported_by_peer",
            Self::InvalidFileName => "invalid_file_name",
            Self::TransferTooLarge { .. } => "transfer_too_large",
            Self::UnknownTransfer => "transfer_not_found",
            Self::TransferExists => "transfer_exists",
            Self::InvalidDeviceName => "invalid_device_name",
            Self::InvalidDownloadDir => "invalid_download_dir",
            Self::InvalidSettings => "invalid_settings",
            Self::InvalidTransferState => "invalid_transfer_state",
            Self::InvalidCommandCapacity
            | Self::StateUnavailable
            | Self::EventBus(_)
            | Self::InvalidPeerCertificate
            | Self::Store(_)
            | Self::Internal => "internal_error",
        }
    }
}

//! Share: send a file, text or a link to a paired device, and save files
//! it sends here.
//!
//! Sending (`POST /devices/{id}/share`, a streamed upload, or
//! [`send_path`], a local file, for the UI) offers the file with a `kdeconnect.share.request` that advertises a payload port, and
//! streams the upload's bytes to the device once it dials in. Receiving
//! dials the port a device's `kdeconnect.share.request` advertises and
//! saves the file in the download directory. Both run as transfers of the
//! core's transfers service, so they are listed, report progress and can be
//! cancelled through `/transfers` like any other.
//!
//! One file per request, in both directions. `kdeconnect.share.request.update`
//! (the size of a multi-file batch) isn't claimed, so the core drops it.
//!
//! A request with `text` or `url` instead of a file ([`send_text`],
//! [`send_url`], `POST /devices/{id}/share/text` and `/share/url`) has no
//! payload. One received is published as `share.received`
//! ([`ReceivedShare`]), a one-off like `ping.received`: a web link
//! ([`is_web_link`]) as a link, anything else, other links included, as
//! text. What happens then is the app's: it opens links in the browser
//! and copies text to the clipboard.
//!
//! Never log a file name, file contents, or shared text or links.

mod http;
pub mod packet;

use std::{
    io,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{io::AsyncReadExt, sync::mpsc};
use uuid::Uuid;

pub use packet::{
    PACKET_TYPE, ShareRequestBody, ShareRequestUpdateBody, ShareTextBody, ShareUrlBody,
    UPDATE_PACKET_TYPE, build_request_packet, build_text_packet, build_update_packet,
    build_url_packet, payload_port,
};

use crate::{
    core::{
        CoreError, DeviceSnapshot, OperationErrorCode, PayloadPeer, Plugin, PluginContext,
        PluginEventKind, TransferDirection, TransferHandle, TransferSnapshot, forward_reader,
        sanitize_file_name, upload_channel,
    },
    protocol::Packet,
};

/// The plugin's id.
pub const ID: &str = "share";

pub struct SharePlugin;

impl Plugin for SharePlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        // KDE Connect checks for a file first, then text, then a link.
        if packet.body.contains_key("filename") || packet.payload_size.is_some() {
            receive(ctx, device, packet);
        } else if let Ok(ShareTextBody { text }) = packet.body_as() {
            receive_content(ctx, device, SharedContent::Text { text });
        } else if let Ok(ShareUrlBody { url }) = packet.body_as() {
            let content = if is_web_link(&url) {
                SharedContent::Link {
                    url: url.trim().to_owned(),
                }
            } else {
                SharedContent::Text { text: url }
            };
            receive_content(ctx, device, content);
        } else {
            tracing::debug!(
                device_id = device.device_id,
                "dropping share request with nothing to share"
            );
        }
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(ctx)
    }

    fn streaming_routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::streaming_routes(ctx)
    }
}

/// The most text, or the longest link, sent in one request, in UTF-8
/// bytes. A request is one line on the control channel, which a peer
/// reads up to 64 KiB, so this leaves room for the rest of the packet.
pub const MAX_SHARED_TEXT_BYTES: usize = 32 * 1024;

/// Text or a link shared by a paired device, published as
/// `share.received`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedShare {
    pub device_id: String,
    pub device_name: String,
    #[serde(flatten)]
    pub content: SharedContent,
}

impl PluginEventKind for ReceivedShare {
    const TYPE: &'static str = "share.received";
}

/// What a device shared: `{"kind": "text", "text": ...}` or
/// `{"kind": "link", "url": ...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SharedContent {
    Text {
        text: String,
    },
    /// An `http` or `https` link ([`is_web_link`]), trimmed.
    Link {
        url: String,
    },
}

/// Whether `text`, trimmed, is one `http` or `https` link: the only links
/// safe to hand to the browser without asking. Anything else a device
/// sends as a link (`file:`, `javascript:`, an app's own scheme) is treated
/// as text.
pub fn is_web_link(text: &str) -> bool {
    let text = text.trim();
    let rest = ["https://", "http://"].iter().find_map(|scheme| {
        text.get(..scheme.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
            .map(|_| &text[scheme.len()..])
    });
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && !rest.starts_with('/')
            && !rest
                .chars()
                .any(|char| char.is_whitespace() || char.is_control())
    })
}

/// Why text or a link wasn't sent.
#[derive(Debug, Error)]
pub enum ShareTextError {
    #[error("there is nothing to share")]
    Empty,
    #[error("shared text exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl ShareTextError {
    /// The code clients see for this error, as [`CoreError::code`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "share_empty",
            Self::TooLarge { .. } => "share_too_large",
            Self::Core(error) => error.code(),
        }
    }
}

/// Send `text` to a paired, connected device that accepts
/// `kdeconnect.share.request`. KDE Connect for Android copies it to its
/// clipboard. Refused, with a typed error, if it is blank or over
/// [`MAX_SHARED_TEXT_BYTES`].
pub fn send_text(ctx: &PluginContext, device_id: &str, text: String) -> Result<(), ShareTextError> {
    check_shared(&text)?;
    let packet = build_text_packet(unix_millis(), text).map_err(|_| CoreError::Internal)?;
    Ok(ctx.send(device_id, packet)?)
}

/// Send a link to a paired, connected device that accepts
/// `kdeconnect.share.request`, trimmed. KDE Connect opens it with the app
/// for its scheme (a browser for a web page). Refused, with a typed error,
/// if it is blank or over [`MAX_SHARED_TEXT_BYTES`].
pub fn send_url(ctx: &PluginContext, device_id: &str, url: &str) -> Result<(), ShareTextError> {
    let url = url.trim();
    check_shared(url)?;
    let packet =
        build_url_packet(unix_millis(), url.to_owned()).map_err(|_| CoreError::Internal)?;
    Ok(ctx.send(device_id, packet)?)
}

fn check_shared(text: &str) -> Result<(), ShareTextError> {
    if text.trim().is_empty() {
        return Err(ShareTextError::Empty);
    }
    if text.len() > MAX_SHARED_TEXT_BYTES {
        return Err(ShareTextError::TooLarge {
            limit: MAX_SHARED_TEXT_BYTES,
        });
    }
    Ok(())
}

/// Publish text or a link a paired device shared, unless it is blank.
/// Only its kind and length are logged.
fn receive_content(ctx: &PluginContext, device: &DeviceSnapshot, content: SharedContent) {
    let (kind, shared) = match &content {
        SharedContent::Text { text } => ("text", text),
        SharedContent::Link { url } => ("link", url),
    };
    let length = shared.len();
    if shared.trim().is_empty() {
        tracing::debug!(device_id = device.device_id, kind, "dropping a blank share");
        return;
    }
    tracing::debug!(device_id = device.device_id, kind, length, "share received");
    let _ = ctx.publish(&ReceivedShare {
        device_id: device.device_id.clone(),
        device_name: device.device_name.clone(),
        content,
    });
}

/// Start sending a file of `declared_size` bytes to a paired, connected
/// device that accepts `kdeconnect.share.request`, as a transfer with `id`
/// if the client chose one. Returns at once with the `queued` transfer and
/// a bounded sender to stream the file's bytes into; dropping the sender
/// ends the upload. The transfer fails if fewer or more bytes than declared
/// arrive.
pub fn send_file(
    ctx: &PluginContext,
    device_id: &str,
    file_name: String,
    declared_size: u64,
    id: Option<Uuid>,
) -> Result<(TransferSnapshot, mpsc::Sender<Bytes>), CoreError> {
    if file_name.trim().is_empty() {
        return Err(CoreError::InvalidFileName);
    }
    let limit = ctx.transfers().max_bytes();
    if declared_size > limit {
        return Err(CoreError::TransferTooLarge { limit });
    }
    ctx.can_send(device_id, PACKET_TYPE)?;
    let peer = ctx.payload_peer(device_id)?;
    let device = ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;

    let transfer = ctx.transfers().begin_as(
        id,
        &device,
        TransferDirection::Outgoing,
        file_name,
        declared_size,
    )?;
    let started = transfer.snapshot();
    let (sender, chunks) = upload_channel();
    let ctx = ctx.clone();
    let device_id = device_id.to_owned();
    transfer.spawn(move |transfer| send(ctx, device_id, peer, transfer, chunks));
    Ok((started, sender))
}

/// Why a local file couldn't be sent.
#[derive(Debug, Error)]
pub enum SendPathError {
    /// The core refused to start the transfer.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The file couldn't be opened, or isn't a regular file.
    #[error("the file couldn't be read")]
    File(#[source] io::Error),
}

/// Send the local file at `path` to a device, under its own name: a
/// [`send_file`] of its size, streamed from disk. Resolves once the whole
/// file has gone into the transfer, or the transfer has ended first (it
/// was cancelled or failed), with the transfer as it is then, as
/// `POST /devices/{id}/share` answers. Run it on the daemon's runtime.
pub async fn send_path(
    ctx: &PluginContext,
    device_id: &str,
    path: &Path,
) -> Result<TransferSnapshot, SendPathError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(SendPathError::File)?;
    let metadata = file.metadata().await.map_err(SendPathError::File)?;
    if !metadata.is_file() {
        return Err(SendPathError::File(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        )));
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let size = metadata.len();
    let (started, sender) = send_file(ctx, device_id, file_name, size, None)?;
    // A file that grows meanwhile sends what it had; one that shrinks, or
    // fails part-way, fails the transfer as short.
    if let Err(error) = forward_reader(file.take(size), sender).await {
        tracing::debug!(device_id, %error, "reading a file to send failed");
    }
    Ok(ctx.transfers().get(started.id).unwrap_or(started))
}

/// Offer the file on a fresh payload port, wait for the device to dial in,
/// then stream the upload to it.
async fn send(
    ctx: PluginContext,
    device_id: String,
    peer: PayloadPeer,
    mut transfer: TransferHandle,
    mut chunks: mpsc::Receiver<Bytes>,
) {
    transfer.connecting();
    let Ok(listener) = peer.listen().await else {
        return transfer.fail(OperationErrorCode::ConnectionFailed);
    };
    let snapshot = transfer.snapshot();
    let Ok(packet) = build_request_packet(
        unix_millis(),
        snapshot.file_name,
        None,
        snapshot.total_bytes,
        listener.port(),
    ) else {
        return transfer.fail(OperationErrorCode::Internal);
    };
    if ctx.send(&device_id, packet).is_err() {
        return transfer.fail(OperationErrorCode::ConnectionFailed);
    }

    let cancellation = transfer.cancellation();
    let mut stream = tokio::select! {
        _ = cancellation.cancelled() => return transfer.cancelled(),
        accepted = listener.accept() => match accepted {
            Ok(stream) => stream,
            Err(_) => return transfer.fail(OperationErrorCode::ConnectionFailed),
        },
    };
    transfer.transferring();
    let result = transfer.forward(&mut chunks, &mut stream).await;
    transfer.finish(result);
}

/// Handle a `kdeconnect.share.request` from a paired device: check the
/// file's name and size, then dial the advertised payload port and save
/// the file. A request whose name can't be made safe, or that is over the
/// size limit, is recorded as a failed transfer without dialing.
fn receive(ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
    let device_id = device.device_id.as_str();
    let Ok(body) = packet.body_as::<ShareRequestBody>() else {
        tracing::debug!(device_id, "dropping malformed share request");
        return;
    };
    let Some(total) = packet
        .payload_size
        .and_then(|size| u64::try_from(size).ok())
    else {
        tracing::debug!(device_id, "dropping share request without a payload size");
        return;
    };
    let Some(port) = packet.payload_transfer_info.as_ref().and_then(payload_port) else {
        tracing::debug!(device_id, "dropping share request without a payload port");
        return;
    };
    let Ok(peer) = ctx.payload_peer(device_id) else {
        return;
    };

    let transfers = ctx.transfers();
    let file_name = sanitize_file_name(&body.filename);
    let display_name = file_name.clone().unwrap_or(body.filename);
    let transfer = transfers.begin(device, TransferDirection::Incoming, display_name, total);
    let file_name = match file_name {
        Ok(_) if total > transfers.max_bytes() => {
            return transfer.fail(OperationErrorCode::Unavailable);
        }
        Ok(file_name) => file_name,
        Err(_) => return transfer.fail(OperationErrorCode::ProtocolError),
    };
    transfer.spawn(move |transfer| async move {
        transfer.connecting();
        let cancellation = transfer.cancellation();
        let mut stream = tokio::select! {
            _ = cancellation.cancelled() => return transfer.cancelled(),
            connected = peer.connect(port) => match connected {
                Ok(stream) => stream,
                Err(_) => return transfer.fail(OperationErrorCode::ConnectionFailed),
            },
        };
        transfer.save_to_downloads(&mut stream, &file_name).await;
    });
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
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        Core, EventData, TransferStatus,
        testing::{handle_with_plugin, make_identity},
    };

    const PEER: &str = "740bd4b9b4184ee497d6caf1da8151be";

    /// A paired, connected peer that accepts share requests; its packets
    /// arrive on the returned receiver.
    fn paired_peer(handle: &Core) -> mpsc::Receiver<Packet> {
        let identity = make_identity(PEER, vec![PACKET_TYPE.into()]);
        handle.discover_device(&identity, true, 1).unwrap();
        let (tx, rx) = mpsc::channel(4);
        handle
            .register_connection(PEER, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .unwrap();
        rx
    }

    #[test]
    fn sending_is_refused_before_a_transfer_is_recorded() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let ctx = handle.plugin_context();
        assert!(matches!(
            send_file(&ctx, PEER, "a.txt".into(), 1, None),
            Err(CoreError::UnknownDevice)
        ));
        let _packets = paired_peer(&handle);
        assert!(matches!(
            send_file(&ctx, PEER, " ".into(), 1, None),
            Err(CoreError::InvalidFileName)
        ));
        assert!(matches!(
            send_file(&ctx, PEER, "a.txt".into(), u64::MAX, None),
            Err(CoreError::TransferTooLarge { .. })
        ));
        assert!(ctx.transfers().list().is_empty());
    }

    #[tokio::test]
    async fn a_path_that_is_not_a_readable_file_is_refused_before_a_transfer() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let _packets = paired_peer(&handle);
        let ctx = handle.plugin_context();
        let folder = tempfile::tempdir().unwrap();

        for path in [folder.path().to_owned(), folder.path().join("missing.txt")] {
            assert!(
                matches!(
                    send_path(&ctx, PEER, &path).await,
                    Err(SendPathError::File(_))
                ),
                "{path:?}"
            );
        }
        let file = folder.path().join("notes.txt");
        std::fs::write(&file, "notes").unwrap();
        assert!(matches!(
            send_path(&ctx, "unknown", &file).await,
            Err(SendPathError::Core(CoreError::UnknownDevice))
        ));
        assert!(ctx.transfers().list().is_empty());
    }

    #[tokio::test]
    async fn sending_offers_the_file_on_a_payload_port() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let mut packets = paired_peer(&handle);
        let ctx = handle.plugin_context();

        let (started, _sender) = send_file(&ctx, PEER, "notes.txt".into(), 5, None).unwrap();
        assert_eq!(started.status, TransferStatus::Queued);
        assert_eq!(started.direction, TransferDirection::Outgoing);

        let offer = tokio::time::timeout(std::time::Duration::from_secs(2), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.packet_type, PACKET_TYPE);
        assert_eq!(offer.payload_size, Some(5));
        assert!(
            offer
                .payload_transfer_info
                .as_ref()
                .and_then(payload_port)
                .is_some()
        );
        assert_eq!(
            offer.body_as::<ShareRequestBody>().unwrap().filename,
            "notes.txt"
        );

        handle.cancel_transfer(started.id).unwrap();
        handle
            .shutdown_transfers(std::time::Duration::from_secs(1))
            .await;
        assert_eq!(
            ctx.transfers().get(started.id).unwrap().status,
            TransferStatus::Cancelled
        );
    }

    #[test]
    fn requests_that_cannot_be_saved_safely_are_recorded_as_failed() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let _packets = paired_peer(&handle);

        let traversal = build_request_packet(1_u64, "..".into(), None, 10, 1741).unwrap();
        handle.handle_peer_packet(PEER, traversal);
        let too_large =
            build_request_packet(2_u64, "big.bin".into(), None, u64::MAX, 1741).unwrap();
        handle.handle_peer_packet(PEER, too_large);

        let transfers = handle.transfers().list();
        let mut codes: Vec<_> = transfers
            .iter()
            .map(|transfer| {
                assert_eq!(transfer.status, TransferStatus::Failed);
                assert_eq!(transfer.direction, TransferDirection::Incoming);
                transfer.error_code
            })
            .collect();
        codes.sort_by_key(|code| format!("{code:?}"));
        assert_eq!(
            codes,
            [
                Some(OperationErrorCode::ProtocolError),
                Some(OperationErrorCode::Unavailable)
            ]
        );
    }

    #[test]
    fn only_single_http_and_https_links_are_web_links() {
        for link in [
            "https://kde.org",
            "  HTTP://example.com/a?b=c#d \n",
            "https://[::1]:8080/",
        ] {
            assert!(is_web_link(link), "{link:?}");
        }
        for other in [
            "",
            "https://",
            "http:///etc/passwd",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "intent://scan/#Intent;scheme=zxing;end",
            "ftp://example.com",
            "https://a.example b.example",
            "see https://kde.org",
            "https://kde.org\nhttps://example.com",
        ] {
            assert!(!is_web_link(other), "{other:?}");
        }
    }

    #[test]
    fn text_and_links_are_sent_without_a_payload() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let mut packets = paired_peer(&handle);
        let ctx = handle.plugin_context();

        send_text(&ctx, PEER, "hello".into()).unwrap();
        let sent = packets.try_recv().unwrap();
        assert_eq!(sent.packet_type, PACKET_TYPE);
        assert_eq!(sent.body_as::<ShareTextBody>().unwrap().text, "hello");
        assert_eq!(sent.payload_size, None);

        send_url(&ctx, PEER, " https://kde.org \n").unwrap();
        let sent = packets.try_recv().unwrap();
        assert_eq!(
            sent.body_as::<ShareUrlBody>().unwrap().url,
            "https://kde.org"
        );
        assert!(!sent.body.contains_key("text"));
    }

    #[test]
    fn blank_oversized_or_undeliverable_shares_are_refused() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let ctx = handle.plugin_context();
        assert!(matches!(
            send_text(&ctx, PEER, "hi".into()),
            Err(ShareTextError::Core(CoreError::UnknownDevice))
        ));
        let mut packets = paired_peer(&handle);
        assert!(matches!(
            send_text(&ctx, PEER, " \n".into()),
            Err(ShareTextError::Empty)
        ));
        assert!(matches!(
            send_url(&ctx, PEER, ""),
            Err(ShareTextError::Empty)
        ));
        let long = "a".repeat(MAX_SHARED_TEXT_BYTES + 1);
        let error = send_text(&ctx, PEER, long.clone()).unwrap_err();
        assert_eq!(error.code(), "share_too_large");
        assert!(matches!(
            send_url(&ctx, PEER, &long),
            Err(ShareTextError::TooLarge { .. })
        ));
        assert!(packets.try_recv().is_err());
    }

    #[test]
    fn shared_text_and_web_links_are_published_and_other_links_become_text() {
        let (handle, _plugin, _commands) = handle_with_plugin(SharePlugin);
        let _packets = paired_peer(&handle);
        let mut events = handle.event_bus().subscribe();
        let mut received = |packet: Packet| {
            handle.handle_peer_packet(PEER, packet);
            // The test bus holds one event, so check each as it lands.
            match events.try_recv() {
                Ok(event) => match event.event {
                    EventData::Plugin(event) => event.decode::<ReceivedShare>().map(|share| {
                        assert_eq!(
                            (share.device_id.as_str(), share.device_name.as_str()),
                            (PEER, "Peer")
                        );
                        share.content
                    }),
                    other => panic!("unexpected event {other:?}"),
                },
                Err(_) => None,
            }
        };
        let text = |text: &str| SharedContent::Text { text: text.into() };

        assert_eq!(
            received(build_text_packet(1_u64, "hello\nworld".into()).unwrap()),
            Some(text("hello\nworld"))
        );
        assert_eq!(
            received(build_url_packet(2_u64, "https://kde.org/ ".into()).unwrap()),
            Some(SharedContent::Link {
                url: "https://kde.org/".into()
            })
        );
        assert_eq!(
            received(build_url_packet(3_u64, "file:///etc/passwd".into()).unwrap()),
            Some(text("file:///etc/passwd"))
        );
        assert_eq!(
            received(build_text_packet(4_u64, " ".into()).unwrap()),
            None
        );
        let empty = Packet::from_body(5_u64, PACKET_TYPE, &serde_json::json!({})).unwrap();
        assert_eq!(received(empty), None);
        assert!(handle.transfers().list().is_empty());
    }

    #[test]
    fn a_received_share_serializes_with_its_kind() {
        let share = ReceivedShare {
            device_id: PEER.into(),
            device_name: "Peer".into(),
            content: SharedContent::Link {
                url: "https://kde.org".into(),
            },
        };
        let value = serde_json::to_value(&share).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "deviceId": PEER,
                "deviceName": "Peer",
                "kind": "link",
                "url": "https://kde.org",
            })
        );
        assert_eq!(
            serde_json::from_value::<ReceivedShare>(value).unwrap(),
            share
        );
    }
}

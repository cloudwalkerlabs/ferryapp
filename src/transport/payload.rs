//! Auxiliary TLS payload connections used to move file transfer bytes.
//!
//! Unlike the persistent control connection in [`super::lan`], a payload
//! connection exists for exactly one file transfer: one side binds an
//! ephemeral TCP listener in the same port range KDE Connect uses for control
//! connections (`1716..=1764`), advertises the chosen port in the control
//! packet's `payloadTransferInfo`, and the other side dials it. Both ends
//! reuse the mutually authenticated TLS handshake and certificate pinning
//! from [`super::tls`]; this module only adds the bounded, chunked byte
//! movement on top of an already-authenticated stream.
//!
//! Every copy helper here works in fixed-size chunks and never buffers a
//! whole file in memory, and every blocking point is cancellation-aware so a
//! cancelled or disconnected transfer cannot leak a task or a socket.

use std::{net::Ipv4Addr, ops::RangeInclusive, time::Duration};

use bytes::Bytes;
use futures_util::{StreamExt, stream::FuturesUnordered};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::tls::{self, PeerPin, TlsError, TlsMaterial};

/// Chunk size used for every payload read/write. Bounds peak memory use for
/// a transfer to a small, fixed multiple of this value regardless of file
/// size.
pub const PAYLOAD_CHUNK_SIZE: usize = 64 * 1024;

/// Bind an ephemeral TCP listener for one payload transfer, scanning `ports`
/// the same way the control-channel listener does.
pub async fn bind_payload_listener(
    bind_ip: Ipv4Addr,
    ports: RangeInclusive<u16>,
) -> Result<(TcpListener, u16), PayloadError> {
    let listener = super::lan::bind_listener(bind_ip, ports)
        .await
        .map_err(PayloadError::Socket)?
        .ok_or(PayloadError::NoPort)?;
    let port = listener.local_addr().map_err(PayloadError::Socket)?.port();
    Ok((listener, port))
}

/// Accept the expected device's connection on a payload listener and
/// upgrade it to TLS, acting as the server (used by the sending side, which
/// is dialed by the receiver).
///
/// The port was free a moment ago, so a dial meant for its previous owner
/// can still arrive: a device that is late for a transfer already cancelled
/// or timed out, whose port this one now reuses. Taking only the first
/// connection would fail this transfer on that stray and reset the real
/// one queued behind it. So connections whose handshake fails are dropped,
/// and the listener keeps accepting until `deadline`. Handshakes run side
/// by side, so a stray that stalls can't hold up the device's.
pub async fn accept_payload_connection(
    listener: TcpListener,
    deadline: Duration,
    material: &TlsMaterial,
    expected_device_id: &str,
    pin: PeerPin,
) -> Result<tokio_rustls::server::TlsStream<TcpStream>, PayloadError> {
    let mut last_error = None;
    let accepting = async {
        let mut handshakes = FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, from) = accepted.map_err(PayloadError::Socket)?;
                    let pin = pin.clone();
                    handshakes.push(async move {
                        (from, tls::accept(stream, material, expected_device_id, pin).await)
                    });
                }
                Some((from, handshake)) = handshakes.next() => match handshake {
                    Ok(stream) => return Ok(stream),
                    Err(error) => {
                        debug!(%from, %error, "dropping a payload connection that failed its handshake");
                        last_error = Some(error);
                    }
                },
            }
        }
    };
    match timeout(deadline, accepting).await {
        Ok(result) => result,
        // Nobody authenticated in time; if someone tried, say why they failed.
        Err(_) => Err(last_error.map_or(PayloadError::ConnectTimeout, PayloadError::Tls)),
    }
}

/// Dial a payload listener advertised by a peer and upgrade to TLS, acting
/// as the client (used by the receiving side).
pub async fn connect_payload(
    addr: std::net::SocketAddr,
    deadline: Duration,
    material: &TlsMaterial,
    expected_device_id: &str,
    pin: PeerPin,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, PayloadError> {
    let stream = timeout(deadline, TcpStream::connect(addr))
        .await
        .map_err(|_| PayloadError::ConnectTimeout)?
        .map_err(PayloadError::Socket)?;
    tls::connect(stream, material, expected_device_id, pin)
        .await
        .map_err(PayloadError::Tls)
}

/// Copy exactly `total` bytes from `reader` to `writer` in bounded chunks,
/// reporting cumulative progress after each chunk. Cancellation-aware: a
/// cancelled token aborts the copy promptly rather than running it to
/// completion or to disconnect.
///
/// Used for the network-to-disk hop of an incoming transfer.
pub async fn copy_exact<R, W>(
    reader: &mut R,
    writer: &mut W,
    total: u64,
    cancellation: &CancellationToken,
    mut on_progress: impl FnMut(u64),
) -> Result<(), PayloadError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; PAYLOAD_CHUNK_SIZE];
    let mut transferred: u64 = 0;
    while transferred < total {
        let remaining = (total - transferred) as usize;
        let want = remaining.min(buffer.len());
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(PayloadError::Cancelled),
            result = reader.read(&mut buffer[..want]) => result.map_err(PayloadError::Socket)?,
        };
        if read == 0 {
            return Err(PayloadError::IncompleteTransfer { transferred, total });
        }
        tokio::select! {
            _ = cancellation.cancelled() => return Err(PayloadError::Cancelled),
            result = writer.write_all(&buffer[..read]) => result.map_err(PayloadError::Socket)?,
        };
        transferred += read as u64;
        on_progress(transferred);
    }
    writer.flush().await.map_err(PayloadError::Socket)?;
    Ok(())
}

/// Forward exactly `total` bytes received from a bounded channel of chunks
/// to `writer`, reporting cumulative progress. Used for the API-upload-to-
/// network hop of an outgoing transfer: the HTTP handler streams multipart
/// chunks into the channel without ever holding the complete file, and this
/// loop drains it into the payload connection with the same bound.
pub async fn forward_channel<W>(
    receiver: &mut mpsc::Receiver<Bytes>,
    writer: &mut W,
    total: u64,
    cancellation: &CancellationToken,
    mut on_progress: impl FnMut(u64),
) -> Result<(), PayloadError>
where
    W: AsyncWrite + Unpin,
{
    let mut transferred: u64 = 0;
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(PayloadError::Cancelled),
            chunk = receiver.recv() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.is_empty() {
            continue;
        }
        let new_total = transferred
            .checked_add(chunk.len() as u64)
            .ok_or(PayloadError::DeclaredSizeExceeded)?;
        if new_total > total {
            return Err(PayloadError::DeclaredSizeExceeded);
        }
        tokio::select! {
            _ = cancellation.cancelled() => return Err(PayloadError::Cancelled),
            result = writer.write_all(&chunk) => result.map_err(PayloadError::Socket)?,
        };
        transferred = new_total;
        on_progress(transferred);
    }
    if transferred != total {
        return Err(PayloadError::IncompleteTransfer { transferred, total });
    }
    writer.flush().await.map_err(PayloadError::Socket)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("no payload TCP port is available")]
    NoPort,
    #[error("payload socket operation failed")]
    Socket(#[source] std::io::Error),
    #[error("payload connection was not established before the deadline")]
    ConnectTimeout,
    #[error("payload TLS handshake failed")]
    Tls(#[source] TlsError),
    #[error("payload transfer was cancelled")]
    Cancelled,
    #[error("payload stream ended after {transferred} of {total} declared bytes")]
    IncompleteTransfer { transferred: u64, total: u64 },
    #[error("payload stream exceeded its declared size")]
    DeclaredSizeExceeded,
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;

    #[tokio::test]
    async fn copy_exact_moves_bytes_in_bounded_chunks_and_reports_progress() {
        let data = vec![7_u8; PAYLOAD_CHUNK_SIZE * 3 + 10];
        let mut reader = std::io::Cursor::new(data.clone());
        let mut writer = Vec::new();
        let mut progress = Vec::new();
        copy_exact(
            &mut reader,
            &mut writer,
            data.len() as u64,
            &CancellationToken::new(),
            |transferred| progress.push(transferred),
        )
        .await
        .unwrap();
        assert_eq!(writer, data);
        assert!(progress.windows(2).all(|window| window[0] <= window[1]));
        assert_eq!(*progress.last().unwrap(), data.len() as u64);
    }

    #[tokio::test]
    async fn copy_exact_reports_incomplete_transfer_on_early_eof() {
        let mut reader = std::io::Cursor::new(vec![1_u8; 10]);
        let mut writer = Vec::new();
        let error = copy_exact(
            &mut reader,
            &mut writer,
            20,
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            PayloadError::IncompleteTransfer {
                transferred: 10,
                total: 20
            }
        ));
    }

    #[tokio::test]
    async fn copy_exact_stops_promptly_when_cancelled() {
        let mut reader = tokio::io::repeat(0);
        let mut writer = Vec::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = copy_exact(&mut reader, &mut writer, u64::MAX, &cancellation, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Cancelled));
    }

    async fn identity() -> crate::config::LocalIdentity {
        let store = crate::store::Store::open_in_memory().await.unwrap();
        crate::config::LocalIdentity::load_or_create(&store)
            .await
            .unwrap()
    }

    fn material(identity: &crate::config::LocalIdentity) -> TlsMaterial {
        TlsMaterial::new(identity.certificate_der(), identity.private_key_der())
    }

    #[tokio::test]
    async fn a_payload_listener_waits_past_stray_connections_for_its_device() {
        let sender = identity().await;
        let device = identity().await;
        let stranger = identity().await;
        let (listener, port) = bind_payload_listener(Ipv4Addr::LOCALHOST, 0..=0)
            .await
            .unwrap();
        let address = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let accepting = tokio::spawn({
            let material = material(&sender);
            let device_id = device.device_id().to_owned();
            let pin = PeerPin::Pinned(device.certificate_der().to_vec());
            async move {
                accept_payload_connection(
                    listener,
                    Duration::from_secs(5),
                    &material,
                    &device_id,
                    pin,
                )
                .await
            }
        });

        // A dial that never speaks TLS, then one from another device (late
        // for a transfer whose port this was): neither may end the wait.
        let _silent = TcpStream::connect(address).await.unwrap();
        let _ = connect_payload(
            address,
            Duration::from_secs(1),
            &material(&stranger),
            sender.device_id(),
            PeerPin::Unpinned,
        )
        .await;
        let _dialed = connect_payload(
            address,
            Duration::from_secs(1),
            &material(&device),
            sender.device_id(),
            PeerPin::Pinned(sender.certificate_der().to_vec()),
        )
        .await
        .unwrap();

        let accepted = accepting.await.unwrap().unwrap();
        assert_eq!(
            tls::server_peer_certificate(&accepted).unwrap(),
            device.certificate_der()
        );
    }

    #[tokio::test]
    async fn a_payload_listener_reports_why_the_only_dial_failed() {
        let sender = identity().await;
        let device = identity().await;
        let stranger = identity().await;
        let (listener, port) = bind_payload_listener(Ipv4Addr::LOCALHOST, 0..=0)
            .await
            .unwrap();
        let accepting = tokio::spawn({
            let material = material(&sender);
            let device_id = device.device_id().to_owned();
            let pin = PeerPin::Pinned(device.certificate_der().to_vec());
            async move {
                accept_payload_connection(
                    listener,
                    Duration::from_millis(500),
                    &material,
                    &device_id,
                    pin,
                )
                .await
            }
        });
        let _ = connect_payload(
            (Ipv4Addr::LOCALHOST, port).into(),
            Duration::from_secs(1),
            &material(&stranger),
            sender.device_id(),
            PeerPin::Unpinned,
        )
        .await;

        let error = accepting.await.unwrap().unwrap_err();
        assert!(matches!(error, PayloadError::Tls(_)), "{error:?}");
    }

    /// A minimal `AsyncWrite` test double that fails after a fixed number of
    /// bytes, simulating a disk-write failure without requiring a real
    /// read-only filesystem in CI.
    struct FailingWriter {
        allowed: usize,
        written: usize,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.written >= self.allowed {
                return Poll::Ready(Err(std::io::Error::other("simulated disk write failure")));
            }
            let take = buf.len().min(self.allowed - self.written);
            self.written += take;
            Poll::Ready(Ok(take))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn copy_exact_propagates_disk_write_failures() {
        let mut reader = std::io::Cursor::new(vec![9_u8; PAYLOAD_CHUNK_SIZE + 1]);
        let mut writer = FailingWriter {
            allowed: 10,
            written: 0,
        };
        let error = copy_exact(
            &mut reader,
            &mut writer,
            (PAYLOAD_CHUNK_SIZE + 1) as u64,
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, PayloadError::Socket(_)));
    }

    #[tokio::test]
    async fn forward_channel_moves_chunks_and_detects_short_upload() {
        let (tx, mut rx) = mpsc::channel(4);
        let sender = tokio::spawn(async move {
            tx.send(Bytes::from_static(b"hello ")).await.unwrap();
            tx.send(Bytes::from_static(b"world")).await.unwrap();
        });
        let mut writer = Vec::new();
        forward_channel(&mut rx, &mut writer, 11, &CancellationToken::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(writer, b"hello world");
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn forward_channel_rejects_declared_size_overrun() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"too much data")).unwrap();
        drop(tx);
        let mut writer = Vec::new();
        let error = forward_channel(&mut rx, &mut writer, 4, &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::DeclaredSizeExceeded));
    }

    #[tokio::test]
    async fn forward_channel_detects_early_channel_close() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"short")).unwrap();
        drop(tx);
        let mut writer = Vec::new();
        let error = forward_channel(&mut rx, &mut writer, 100, &CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PayloadError::IncompleteTransfer {
                transferred: 5,
                total: 100
            }
        ));
    }
}

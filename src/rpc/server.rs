//! Serving [`Methods`] on a Unix socket: one daemon per socket, only to
//! its own user, each connection's requests run concurrently and dropped
//! when it closes.

use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    sync::CancellationToken,
    task::TaskTracker,
};
use tracing::{debug, info, warn};

use super::{MAX_LINE_BYTES, Message, Methods, RpcError, code};

/// The longest path a socket can be bound at: `sun_path`'s size less its
/// terminating NUL.
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH: usize = 107;
#[cfg(not(target_os = "linux"))]
const MAX_SOCKET_PATH: usize = 103;

/// How many answers and stream items may wait for a slow client before a
/// handler waits too.
const OUTGOING_QUEUE: usize = 64;

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("another Ferry is already serving {}", .0.display())]
    InUse(PathBuf),
    #[error("{} is in the way and isn't a socket", .0.display())]
    NotASocket(PathBuf),
    #[error(
        "{} is too long for a socket ({MAX_SOCKET_PATH} bytes at most); use a shorter data directory",
        .0.display()
    )]
    PathTooLong(PathBuf),
    #[error("couldn't listen on {}", .0.display())]
    Bind(PathBuf, #[source] io::Error),
}

/// A running server. Stops, and removes its socket, on [`Self::shutdown`]
/// or when the cancellation token it was started with is cancelled.
pub struct RpcServer {
    path: PathBuf,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl RpcServer {
    /// Listen at `path`, unless another daemon already does. A socket file
    /// nobody answers on is a crashed daemon's and is replaced; anything
    /// else there is left alone.
    pub async fn start(
        path: PathBuf,
        methods: Methods,
        shutdown: CancellationToken,
    ) -> Result<Self, ServeError> {
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(ServeError::PathTooLong(path));
        }
        claim(&path).await?;
        let listener =
            UnixListener::bind(&path).map_err(|error| ServeError::Bind(path.clone(), error))?;
        // Only its user may connect. Peers are checked too, which also
        // covers the moment before this.
        let owner = std::fs::metadata(&path)
            .and_then(|metadata| {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                // The socket was made by this process: its owner is this
                // process's effective user.
                Ok(metadata.uid())
            })
            .map_err(|error| {
                let _ = std::fs::remove_file(&path);
                ServeError::Bind(path.clone(), error)
            })?;
        info!(path = %path.display(), "control socket listening");

        let methods = Arc::new(methods);
        let accepting = shutdown.clone();
        let socket = path.clone();
        let task = tokio::spawn(async move {
            let connections = TaskTracker::new();
            loop {
                let stream = tokio::select! {
                    _ = accepting.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            warn!(%error, "accepting a control connection failed");
                            continue;
                        }
                    },
                };
                match stream.peer_cred() {
                    Ok(peer) if peer.uid() == owner => {}
                    Ok(peer) => {
                        warn!(
                            uid = peer.uid(),
                            "refused a control connection from another user"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(%error, "couldn't tell who connected; refused");
                        continue;
                    }
                }
                connections.spawn(serve(stream, methods.clone(), accepting.clone()));
            }
            drop(listener);
            remove_socket(&socket);
            connections.close();
            if timeout(Duration::from_secs(5), connections.wait())
                .await
                .is_err()
            {
                warn!("control connections didn't end in time");
            }
        });

        Ok(Self {
            path,
            shutdown,
            task: Some(task),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop listening, end every connection, and remove the socket.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Make way for a socket at `path`: fine if nothing is there, or a socket
/// nobody answers on (removed); not if a daemon answers, or it isn't a
/// socket.
pub async fn claim(path: &Path) -> Result<(), ServeError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ServeError::Bind(path.to_owned(), error)),
    };
    if !metadata.file_type().is_socket() {
        return Err(ServeError::NotASocket(path.to_owned()));
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(ServeError::InUse(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            debug!(path = %path.display(), "replacing a stale control socket");
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| ServeError::Bind(path.to_owned(), error))
        }
        Err(error) => Err(ServeError::Bind(path.to_owned(), error)),
    }
}

fn remove_socket(path: &Path) {
    let is_socket =
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket());
    if is_socket && let Err(error) = std::fs::remove_file(path) {
        warn!(%error, "couldn't remove the control socket");
    }
}

/// Answer one connection until it closes or the server stops. Requests run
/// concurrently, each answered when it finishes; those still running when
/// the client goes are dropped.
async fn serve(stream: UnixStream, methods: Arc<Methods>, shutdown: CancellationToken) {
    let (read, mut write) = stream.into_split();
    let (out, mut outgoing) = mpsc::channel::<String>(OUTGOING_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(line) = outgoing.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = FramedRead::new(read, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
    let mut requests = JoinSet::new();
    loop {
        let line = tokio::select! {
            _ = shutdown.cancelled() => break,
            // Reap finished requests so the set doesn't grow.
            Some(_) = requests.join_next(), if !requests.is_empty() => continue,
            line = lines.next() => line,
        };
        let line = match line {
            Some(Ok(line)) => line,
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                let error = RpcError::new(code::INVALID_REQUEST, "line_too_long", "line too long");
                let _ = out
                    .send(Message::answer(Value::Null, Err(error)).to_line())
                    .await;
                break;
            }
            Some(Err(LinesCodecError::Io(_))) | None => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some((id, future)) = dispatch(&methods, &line, &out) else {
            continue;
        };
        let out = out.clone();
        requests.spawn(async move {
            let reply = future.await;
            if let Some(id) = id {
                let _ = out.send(Message::answer(id, reply).to_line()).await;
            }
        });
    }
    // The client is gone (or the server stopping): drop what it asked for.
    requests.shutdown().await;
    drop(out);
    let _ = writer.await;
}

type Pending = futures_util::future::BoxFuture<'static, Result<Value, RpcError>>;

/// Start the request on `line`: its id (`None` for a notification, which
/// gets no answer) and the future answering it. Called in the order
/// requests arrive, so a handler's synchronous part (e.g. subscribing to
/// events) runs before a later request's. A line that isn't a request is
/// answered at once.
fn dispatch(
    methods: &Methods,
    line: &str,
    out: &mpsc::Sender<String>,
) -> Option<(Option<Value>, Pending)> {
    let reject = |id: Value, code: i64, error_code: &str, message: String| {
        let error = RpcError::new(code, error_code, message);
        let _ = out.try_send(Message::answer(id, Err(error)).to_line());
        None
    };
    let message: Message = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            return reject(
                Value::Null,
                code::PARSE_ERROR,
                "parse_error",
                error.to_string(),
            );
        }
    };
    let id = message.id;
    let Some(method) = message.method else {
        return reject(
            id.unwrap_or(Value::Null),
            code::INVALID_REQUEST,
            "invalid_request",
            "a request needs a method".to_owned(),
        );
    };
    let params = message.params.unwrap_or(Value::Null);
    if !(params.is_object() || params.is_null()) {
        return reject(
            id.unwrap_or(Value::Null),
            code::INVALID_PARAMS,
            "invalid_params",
            "params must be an object".to_owned(),
        );
    }
    let future = methods.call(
        &method,
        params,
        id.clone().unwrap_or(Value::Null),
        out.clone(),
    );
    Some((id, future))
}

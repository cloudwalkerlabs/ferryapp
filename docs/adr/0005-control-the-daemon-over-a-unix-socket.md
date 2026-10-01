# 0005. Control the daemon over a Unix socket with JSON-RPC

- Status: Accepted
- Date: 2026-10-01
- Supersedes: the local HTTP API (`/api/v1`), its bearer token and the
  app's `core.api` port and token

## Context

`ferry-cli` controls a daemon (`ferry-cli run`, or the app once Settings →
Command line access is on) over an HTTP API on `127.0.0.1:24816`. Loopback
TCP is open to every local user and to anything a browser can be talked
into sending there, so the app's API needs a bearer token, kept in its
store, shown and copied in Settings, and renewed when it leaks; the CLI's
own daemon has none by default. Every instance needs a port, which
collides between the owner's app and test or agent runs on the same
machine.

The HTTP surface is also most of the daemon's accidental complexity: Axum
routes per plugin (`routes`, `streaming_routes`), `ApiProblem` with an
HTTP status per error, body limits and a request deadline that uploads
must escape, multipart uploads forwarded chunk by chunk with an idle
timeout and a lingering drain (`api/upload.rs`), Server-Sent Events, and a
`reqwest` client with a method per route (`client.rs`). Each action is
written four times: the plugin's typed function (which the app calls
directly), its `http.rs` handler, its `ApiClient` method, and the CLI
command.

The CLI and the daemon always run on the same machine as the same user,
and they share a filesystem.

## Decision

**Transport.** The daemon listens on a Unix domain socket, `ferry.sock` in
its data directory; the CLI connects to the one in the data directory it is
given (`--data-dir`/`FERRY_DATA_DIR`, the platform's config directory by
default), so a CLI and a daemon started with the same data directory meet
with no port or token. The socket is made `0600` right after it is bound,
and every accepted connection's peer must have the daemon's effective user
id (`SO_PEERCRED` on Linux, `getpeereid` on macOS, through tokio's
`peer_cred`); others are closed unanswered. There is no token.

**One daemon per socket.** Before binding, the daemon connects to an
existing `ferry.sock`. If something answers, another daemon serves this
data directory: `ferry-cli run` refuses to start, and the app refuses to
turn Command line access on (it stays off and Settings shows why). If the
connect is refused, the file is a leftover from a crash and is removed;
anything there that isn't a socket is never removed. The daemon removes
its socket when it stops.

**Platforms.** Linux and macOS (`cfg(unix)`). On Windows the app has no
Command line access and `ferry-cli`'s client commands say they are
unsupported; the protocol is written over any `AsyncRead + AsyncWrite`, so
a named pipe can be added later without changing it. A socket path may not
be longer than 107 bytes (103 on macOS); a data directory too deep for one
fails with an error naming the path.

**Protocol.** JSON-RPC 2.0, one message per line (newline-delimited JSON;
`serde_json` escapes newlines inside strings). A request has an `id`, a
`method` like `share.text` and object `params`; the answer is a `result`
or an `error`. Requests on one connection run concurrently and are
answered as they finish. Errors carry the daemon's error code as before:
`{"code": -32000, "message": "...", "data": {"code": "not_paired",
"detail": "..."}}`; the standard codes cover unparseable lines, unknown
methods and invalid params. A line over 1 MiB closes the connection.

**Streams.** A method may send items before its answer, as notifications
`{"method": "stream", "params": {"id": <request id>, "item": ...}}`.
`events.subscribe` sends every core event this way and never answers
unless the subscriber lags, when it answers with the error `events_lagged`
so the client takes a fresh snapshot and subscribes again (what an SSE
reconnect did). `files.read` sends a remote file's content as base64
chunks, then answers with its size.

**Cancellation.** When a connection closes, its requests still running are
dropped, so interrupting the CLI cancels what it was waiting for, as
closing an HTTP request did.

**Files by path.** The CLI sends the absolute path of a local file instead
of its bytes (`share.file`, `files.upload`); the daemon reads it with
`share::send_path` and `BrowsePlugin::upload_path`, which the app already
uses. The request answers once the whole file has gone into the transfer,
as the upload's response did, so interrupting the CLI before then ends the
transfer as short once the device connects for it (until then it waits
for the device, as it did over HTTP). Multipart uploads, body limits and the idle timeout and
drain go away.

**One definition per method.** A method is its params type:

```rust
pub trait Method: Serialize + DeserializeOwned + Send + 'static {
    const NAME: &'static str;
    type Output: Serialize + DeserializeOwned + Send + 'static;
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShareText { pub device_id: String, pub text: String }
impl Method for ShareText { const NAME: &'static str = "share.text"; type Output = (); }
```

It lives with the feature's typed function, in the plugin's `rpc.rs`
(which replaces `http.rs`). `Plugin::methods` (which replaces `routes` and
`streaming_routes`) registers a handler for each with the plugin's state
captured, and the core registers its own (`status`, `devices.*`,
`pairings.*`, `transfers.*`, `settings.*`, `events.subscribe`):

```rust
methods.add(move |params: ShareText| {
    let ctx = ctx.clone();
    async move { send_text(&ctx, &params.device_id, params.text) }
});
```

The handler's error converts into the protocol's error through the error
code each error type already has. The client has one generic
`call(params)` and `stream(params)` instead of a method per route; the
CLI and the daemon are one crate, so the request and answer types can't
drift apart. The app keeps calling the typed functions directly: it gains
nothing from going through serialization.

`rpc` holds the protocol, the method registry and the server; `client`
holds the connection and the CLI's watch helpers (snapshot, then events,
again after a gap). No JSON-RPC library: the framing and dispatch are a
few hundred lines, and the available crates bring HTTP or WebSocket
transports this doesn't use.

## Consequences

- `axum`, `tower`, `tower-http`, `reqwest` and `async-stream` leave the
  dependency tree, with `api.rs`, `api/upload.rs`, every `http.rs`, the
  API token (`config/token.rs`) and the port and token in `core.api`,
  which keeps only whether the app serves the socket.
- The app's Settings → Command line access loses its setup snippet and
  token buttons: a CLI run by the same user finds the app by itself, and
  nobody else can connect. The app's `--api-port`/`--api-token` become
  `--cli-access`, which turns the socket on for one run.
- `ferry-cli` loses `--api-host`, `--api-port`, `--api-token`,
  `FERRY_API_URL` and `FERRY_API_TOKEN`.
- Test and agent runs no longer pick API ports: their own data directory
  is all that keeps their control socket apart.
- Another user on the machine, a script running as another user, or a
  client on another machine can no longer control a daemon. Nobody did by
  design; the token made it possible.
- Nothing outside this repository spoke the HTTP API, so there is no
  compatibility shim. A CLI talking to an older running daemon fails to
  connect (no socket) or gets `method not found`, and says so.
- Windows loses command line access until a named-pipe transport exists.

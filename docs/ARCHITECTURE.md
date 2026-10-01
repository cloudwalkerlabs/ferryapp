# Ferry architecture

The system as implemented today: module boundaries, data flow, the
control socket, and the pairing and transfer state machines. Read it before making a
change.

Protocol research and the MVP's implementation plan are in
[`docs/archive/`](archive/); both are finished, and this document
supersedes them.

## 1. Shape of the system

Ferry is a Cargo workspace: the `ferry` package (one library,
`src/lib.rs`, plus a CLI binary that is a thin client of it), and the
`ferry-gui` package (`gui/`), the desktop app.

```text
CLI (ferry-cli)       ─┬── control socket (JSON-RPC, ferry.sock) ──┐
Other automation      ─┘                                          ├── core + plugins ── KDE Connect transport
Desktop app (gui/)    ─── in-process: snapshots, events, typed calls ───┘
```

The CLI reaches the daemon only through its control socket, `ferry.sock`
in the data directory (§8, [`adr/0005`](adr/0005-control-the-daemon-over-a-unix-socket.md)).
The desktop app runs the daemon in its own process and calls the core and
the plugins' typed Rust functions directly (§9); its daemon can serve the
socket too, so the CLI can drive the instance the app shows. Sockets, pairing, trust and
transfer state all live in the daemon, behind `core::Core`, never in a
frontend. The UI's design decisions are in
[`adr/0001`](adr/0001-native-ui-in-iced.md).

## 2. Module map and dependency direction

The daemon is a small **core** and a fixed set of **plugins**, one per
feature. The core owns devices, connections, pairing, trust, transfers,
settings and the event bus; a plugin owns one feature's packets, state,
control methods and events, and reaches the core only through its `PluginContext`.
The plugins are fixed at compile time, listed in `plugins::builtin()` (the `BuiltinPlugin` enum, [`adr/0004`](adr/0004-dispatch-plugins-through-a-fixed-enum.md)):
nothing is loaded at runtime and there is no plugin ABI. Module visibility
and review keep the boundaries, in one crate.

```text
binary (src/bin/ferry-cli) → daemon, client, config
gui (ferry-gui) → daemon, ui, plugins::builtin_parts    [feature "gui"]
ui → core (snapshots, events), protocol (types only)         [feature "gui"]
ui::features → ui (shell messages, widgets), plugins/* (typed APIs), core   [feature "gui"]
daemon → core, plugins::builtin, rpc, transport (the composition root)
rpc → core (the core's methods; the plugins' are added through the core)
plugins/* → core (Plugin, PluginContext), rpc (Method, Methods, ErrorCode), protocol
core → config, transport, protocol, rpc (Methods, for the plugin hook)
transport::lan → core (it registers connections and delivers packets)
client → rpc (the protocol and method types), core and plugins/* (their types)
```

`protocol` and `transport` never depend on Clap or the control
protocol's types. The core never names a feature: it calls the plugins only through
`BuiltinPlugin`, the one enum in `plugins` that forwards the `Plugin`
contract, and `daemon` is the one place that picks them. Plugins never
import each other; what two features need (transfers, payload connections)
is a core service. In the UI only `ui::features` names features: the rest
of `ui` (the shell) calls `Features`, and imports `plugins` only to carry
the three instances it hands to them. `core` and `plugins` never import
`ui`.

| Module | File(s) | Responsibility |
| --- | --- | --- |
| `protocol` | `src/protocol/{mod,packet,codec,verification}.rs` | Wire packet envelope, identity/pairing body types, bounded newline-delimited JSON codec, the protocol-v8 verification-code function. No I/O. |
| `config` | `src/config/{mod,control,identity}.rs` | Local device identity (UUID + self-signed cert, stored under `core.identity` and never replaced once made), and `COMMAND_LINE_ACCESS` (`core.api`, the key the old HTTP API's settings had): whether the app serves its control socket. |
| `store` | `src/store/{mod,schema,config,devices}.rs`, `src/store/migrations/` | The daemon's data in one SQLite database, `ferry.db` in the data directory ([`adr/0002`](adr/0002-store-the-daemons-data-in-sqlite.md), [`archive/PLAN_STORE.md`](archive/PLAN_STORE.md)). `schema`: opening the database runs the migrations it hasn't had (`migrations/<number>-<name>/up.sql`, embedded; the first creates every table), and refuses one a newer build migrated further; a schema change is a new migration, never an edit to a shipped one. `config`: typed, watchable values, each named by a `ConfigKey<T, S>` its owner declares, global or per device (`PerDevice`, reached with `.of(id)`); for a value that no longer decodes, `get` returns missing and `get_strict` an error. Write transactions take the database's lock up front, so a CLI daemon and the app on one data directory take turns. `devices`: paired devices' pinned certificates, with the name, type and capabilities each last reported over an authenticated connection; removing one removes its per-device values. Store opens, reads, writes and watch registration are async, with `deadpool-sqlite` executing SQLite on blocking workers ([`adr/0003`](adr/0003-use-async-sqlite-pools-and-plugin-callbacks.md)). File databases use WAL, one writer and up to three readers; the writer orders whole transactions and post-commit notifications. `Store::cached` reads process-local config snapshots without I/O; it and watches observe this store's commits, not another process's writes. TLS pins are loaded asynchronously before the handshake. Tests await `Store::open_in_memory()`, which shares one connection for all operations. |
| `transport` | `src/transport/{lan,tls,payload}.rs` | UDP discovery, TCP control-channel connect/accept, the rustls TLS handshake and certificate pinning, and the auxiliary TLS payload connection for file transfer. `LanConfig::loopback` (the daemon's `--discovery-loopback`) binds discovery to `127.255.255.255` and the control listener to `127.0.0.1`, so nothing on the LAN can discover or reach the instance. Instances on this machine still can: on Linux a socket bound to `0.0.0.0:1716` (a Ferry or KDE Connect not on loopback) also receives broadcasts to `127.255.255.255:1716`. `--discovery-port` (`RunRequest::discovery_port`, loopback only) moves discovery to another port, so tests and agents keep their instances apart from those. It takes `LanCommand`s (announce now, announce to one address) from the core. |
| `core` | `src/core.rs` | `Core`, the cloneable handle to everything below: its state, construction, status, settings, and running the plugins' hooks. One `RwLock` holds what must change together (devices, connections, pairings); transfers, settings and each plugin's state have their own locks. |
| | `src/core/devices.rs` | The device registry and `DeviceSnapshot` (its `plugins` map is filled from each plugin's `device_state` when a snapshot leaves the core), discovery, forgetting a device, and keeping a paired device's trust record current. The registry starts with every paired device from the store, as `unavailable`, so paired devices are listed while offline. |
| | `src/core/connections.rs` | Registering and dropping authenticated control channels, routing each incoming packet to the plugin that claims its type (only from paired devices), sending to devices that advertised a packet type, and `LanCommand`, the channel to the LAN transport. |
| | `src/core/pairing.rs` | The pairing state machine (§4) in both directions, its timeouts, and the trust it writes or removes. |
| | `src/core/transfers.rs`, `src/core/payload.rs` | The transfers service (`Transfers`, `TransferHandle`: the state machine, progress throttling, cancellation and cleanup, for every feature that moves a file, §5), and payload connections for plugins (`PayloadPeer`: listen or dial with this device's certificate, or sign in to an SSH server on the device with its key, without handing out the key). |
| | `src/core/{plugin,events,settings,error}.rs` | The plugin API (`Plugin`, `PluginContext`, `PluginRegistry`, `Capabilities`, plugin events), the bounded event bus (plugin events travel as `EventData::Plugin` with the same `{type, data}` shape), the core's user settings (§7), and `CoreError`. |
| | `src/core/testing.rs` | A real core for unit tests: an in-memory store, no plugins or just the one under test, no LAN. |
| `plugins` | `src/plugins/mod.rs`, `src/plugins/{ping,findmyphone}/{mod,packet,rpc}.rs`, `src/plugins/{battery,connectivity}/{mod,packet}.rs`, `src/plugins/clipboard/{mod,packet,rpc,backend}.rs`, `src/plugins/clipboard/backend/system.rs`, `src/plugins/share/{mod,packet,rpc}.rs`, `src/plugins/browse/{mod,packet,rpc,session,ssh,files}.rs`, `src/plugins/notifications/{mod,packet,rpc}.rs`, `src/plugins/telephony/{mod,packet,rpc}.rs` | The features, each a `core::Plugin`. `builtin()` lists them; `builtin_parts()` builds the same list and also returns the clipboard, browse and notifications instances the UI keeps. Nothing here is behind `gui`. **Ping** owns its packet handling, the `ping.received` event and `ping.send`. **Find my phone** only sends, and owns `findmyphone.ring`. **Battery** adds `plugins.battery` to device snapshots and clears it in the `disconnected`/`unpaired` hooks. **Connectivity** does the same with `plugins.connectivity`, the peer's mobile signal per SIM. **Clipboard** owns the synced text, the `clipboard.*` methods (with its `clipboard.syncEnabled` setting), and its backends (the `ClipboardService` trait, `SystemClipboard` over `arboard` for the desktop clipboard, an in-memory one); it follows the desktop clipboard from its `started` hook and releases it in `shutdown` (§6). **Share** sends files (`share.file`) and saves files peers send, both as core transfers (§5); it also sends text and links (`share.text` and `share.url`) and publishes those a peer shares as `share.received` (the app opens web links and copies text to the clipboard). **Browse** owns the per-device SFTP sessions with peers' file servers and the `files.*` methods, and closes its sessions in the `disconnected`/`unpaired`/`shutdown` hooks (§12). **Notifications** keeps each paired, connected device's notifications in memory (`notifications.*`, `notification.posted`/`notification.removed`), asks for them in the `connected` and `paired` hooks, fetches their icons over payload connections, and drops them in `disconnected`/`unpaired`. Its per-device `notifications.enabled` key (a `PerDevice` config, on when unset) turns this off for one device (`notifications.setEnabled`): the device's notifications are dropped with a `notification.removed` each, packets from it are ignored, and it isn't asked for them; every paired device's snapshot says which as `plugins.notifications` (`{"enabled": bool}`), changed with `device.updated`; turning it back on asks a connected device again. **Telephony** adds the call going on on a phone to its snapshot as `plugins.telephony` (`telephony.call`), publishes `call.missed`, mutes the ringer (`telephony.mute`), and clears the call in `disconnected`/`unpaired`; it never logs callers' names or numbers. The capabilities advertised in the identity packet are the union over the plugins: ping, clipboard and share both ways; `kdeconnect.sftp.request` outgoing and `kdeconnect.sftp` incoming only (browses peers, serves no files); `kdeconnect.battery` incoming only (reads peers' batteries, reports none); `kdeconnect.connectivity_report` incoming only (reads peers' mobile signal, reports none); `kdeconnect.findmyphone.request` outgoing only (asks peers to ring, doesn't ring itself); `kdeconnect.notification` incoming and its `.request`, `.reply` and `.action` outgoing (shows peers' notifications, shares none of its own); `kdeconnect.telephony` incoming and `kdeconnect.telephony.request_mute` outgoing (shows a phone's calls; SMS is not handled). |
| `daemon` | `src/daemon.rs` | The composition root: `RunningService` builds the core with `plugins::builtin()`, applies the stored settings, starts the plugins, the LAN transport (advertising the core's capabilities) and the control socket (`daemon::ControlSwitch`), and stops them in order. `ferry-cli run` refuses to start, before anything else, while another daemon answers on its data directory's socket. Used by the CLI's `run` and the desktop app. `start_with` takes the plugin list from the caller (the desktop app, which keeps each plugin's UI half), and `core()` hands the running core to a frontend in the same process. |
| `rpc` | `src/rpc.rs`, `src/rpc/{methods,server}.rs` | The control protocol (§8, [`adr/0005`](adr/0005-control-the-daemon-over-a-unix-socket.md)): `Method` (a method is its params type, with its name and output; `StreamMethod` also sends items), `define_methods!` (declares params structs), `RpcError` and `ErrorCode` (how a feature's error becomes the protocol's), `Methods` (the registry handlers are added to, with the state they need), and the wire messages. `methods`: the core's methods and `all(core)`, every method a daemon answers. `server` (Unix only): `RpcServer` binds `ferry.sock` (`0600`, peers checked to be the same user), refuses a socket another daemon answers on, replaces one nobody does, runs each connection's requests concurrently and drops them when it closes. |
| `client` | `src/client.rs` | `Client`, the connection the CLI (and tests) use: `call(params)`, `stream(params)`, any number at once on one connection; and the CLI's watch helpers (snapshot, then events, again after a gap). |
| `src/bin/ferry-cli` | `cli.rs`, `main.rs` | The `ferry-cli` binary: argument parsing and daemon bootstrap only. Client commands connect to the socket in `--data-dir` (default: the platform's config directory, the app's). |
| `ui` | `src/ui/{mod,launch,shell,background,drops,actions,context,route,store,sync,activity,demo,error,i18n,widgets,testing,tests}.rs`, `src/ui/i18n/{format,pseudo}.rs`, `i18n/<lang>/ferry.ftl`, `src/ui/pages/*.rs`, `src/ui/overlay/*.rs`, `src/ui/desktop/*.rs`, `src/ui/features/{mod,ping,findmyphone,battery,connectivity,clipboard,share,notifications,telephony}.rs`, `src/ui/features/browse/{mod,view,preview,files,describe}.rs` | The desktop UI in iced, behind the `gui` feature ([`adr/0001`](adr/0001-native-ui-in-iced.md)). It runs in the daemon's process and reads the core directly (§9): `sync` subscribes to the event bus, takes a snapshot into `store`, and takes a fresh one after a lag. `mod` holds `App`, the one app `Message`, `update`'s dispatch, `view` and `subscription`; `launch` the entry points (`run`, `UiOptions`, `Started`), booting and Retry; `route` the typed routes. Each feature's UI is a module under `features/`, and `features/mod.rs` is the one place that lists them: the `Feature` message enum, and `Features`, whose functions the shell calls to fill its slots (the device card's status chips, the device page's and the tray's actions, the device page's per-device switches, drop targets, the file browser's page, the settings page's sections) and to pass on route changes and core events, calling each feature by name in `builtin()` order. Features ask the shell for things (toast, report, notify, show or withdraw a keyed desktop notification with buttons, navigate, pick files, confirm, prompt) through plain `Message`s built by `shell`'s helpers, carrying the `Origin` (window or tray) of the action that caused them; `shell` also holds the `App` side of those requests. The pages the core owns are shell code: devices, device, Add device, pairing (and the incoming pairing prompt, drawn over every page while a request waits), transfers, settings, About (version, author, links, third-party licenses), and the startup and error screens; `actions` is what they ask of the core. `drops` routes dropped files and the recipient chooser. `background` is the window's life (show, close to the tray, quit, placement), the tray and notifications. `desktop` holds the platform glue behind small traits so tests swap in fakes: the tray, notifications, dialogs (`rfd`), opening files and web links (`opener`), the saved window placement, the single-instance socket, the login item, and where the package put the third-party licenses. `i18n` loads the app's translations (Fluent files under `i18n/`, embedded) and chooses the language at start (`FERRY_LANG`, else the `language` setting, else the system's, falling back to en-US); `fl!` looks a message up (§13), `i18n::format` writes numbers and dates in the user's locale with ICU4X, and `i18n::pseudo` makes the en-XA pseudo-locale from en-US. `demo` fills the core with made-up devices for `--demo`; `tests` is the shell's shared test harness. |
| `ferry-gui` | `gui/src/main.rs` | The desktop app's composition root: flags (each also an environment variable), starting the daemon through `RunningService::start_with` and `plugins::builtin_parts()`, running `ui::run`, and shutting the daemon down after. |

### The `Plugin` trait

```rust
pub trait Plugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;                          // "ping"; names its config keys and device state
    fn incoming(&self) -> &'static [&'static str] { &[] }  // packet types it handles
    fn outgoing(&self) -> &'static [&'static str];         // packet types it sends
    fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) -> impl Future<Output = ()> + Send { async {} }
    fn methods(self: Arc<Self>, ctx: PluginContext, methods: &mut Methods) {}
    fn device_state(&self, ctx: &PluginContext, device: &DeviceSnapshot) -> Option<Value> { None }
    fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) -> impl Future<Output = ()> + Send { async {} }
    fn paired(&self, ctx: &PluginContext, device: &DeviceSnapshot) -> impl Future<Output = ()> + Send { async {} }
    fn disconnected(&self, ctx: &PluginContext, device_id: &str) -> impl Future<Output = ()> + Send { async {} }
    fn unpaired(&self, ctx: &PluginContext, device_id: &str) -> impl Future<Output = ()> + Send { async {} }
    fn started(self: Arc<Self>, ctx: &PluginContext) -> impl Future<Output = ()> + Send { async {} }
    fn shutdown(&self) -> BoxFuture<'_, ()> { Box::pin(async {}) }
}
```

Packet handlers and lifecycle hooks are native async methods returning
`impl Future + Send`, which the enum awaits directly; nothing is boxed per
callback (`shutdown` boxes once per plugin so the futures can be joined).
`plugins::BuiltinPlugin` holds each plugin's `Arc` and forwards every
method. Snapshots and metadata stay synchronous
and read memory only. The core passes the context into
each call instead of plugins storing it, so there is no `Arc` cycle
between the core and its plugins. The rules the core keeps:

- **Dispatch.** Pairing packets are the core's. Any other packet goes to
  the plugin whose `incoming()` claims its type, and only from a paired
  device; an unclaimed type is dropped. Two plugins claiming one type, or
  sharing an id, panic when the registry is built, so every test fails.
- **Locks.** The core never calls into a plugin while holding its state lock.
  A per-device async gate orders callbacks, pairing and disconnect cleanup;
  snapshots use brief memory locks and do not wait for that gate. A plugin
  must not hold a synchronous lock across an await or while calling the core.
  Callbacks must not await another lifecycle operation for the same device.
- **Async dispatch.** The LAN connection reads into a bounded packet queue,
  dispatches callbacks in order, and writes outgoing packets independently.
  A waiting callback does not prevent its outgoing packets reaching the peer.
  It must not await a reply dispatched on the same connection: longer work
  belongs in a task the plugin owns and cancels during cleanup. Transport
  termination drops the active packet callback before awaiting disconnect
  hooks. Socket EOF first allows the queued packets up to one second to
  drain, preserving a peer's final unpair packet; shutdown cancels immediately. Persistent core mutations finish their commit and memory updates
  in tracked tasks even if their caller is cancelled; shutdown drains them
  before stopping plugins.
- **Hooks.** `connected` runs after `device.connected` is published;
  `paired` runs after a connected device becomes paired (either side
  accepted) and its update is published, so work for every paired,
  connected device (as KDE Connect does when a plugin loads) goes in both.
  `disconnected` and `unpaired` run before the core publishes the device's
  new state, so state a plugin clears there needs no extra
  `device.updated`. `started` runs once in the daemon, inside the runtime,
  before the transport starts (never in a unit-test core); `shutdown` runs
  for every plugin concurrently after the control socket and transport
  have stopped and every transfer has ended.
- **Methods.** `methods()` adds a handler for each of the plugin's control
  methods, declared with `define_methods!` in its `rpc.rs` next to the
  typed functions they call, named `<feature>.<action>` (`ping.send`,
  `files.list`). A method taking a device has a `deviceId` param. A local
  file is an absolute path the daemon reads itself, never bytes over the
  socket. Two handlers for one name panic when the registry is built.
- **Device state.** A plugin adds to a device's snapshot under
  `plugins.<id>` by answering `device_state` (pulled whenever a snapshot
  leaves the core, given the snapshot without it, so it mustn't call
  `ctx.device`) and calls `ctx.device_changed(id)` when its answer
  changes.
- **Data and settings.** A plugin keeps what it stores, its settings
  included, in the store (`ctx.store()`), under `ConfigKey`s it declares
  as `<id>.<name>` (e.g. `clipboard.syncEnabled`), per device where the
  value belongs to one (`PerDevice`, removed when the device is
  unpaired). It can `watch` a key. The core's settings (§7) know nothing
  of plugins: a plugin serves its settings on its own resource, with its
  own snapshot and events, like any other state of its own. It never
  writes files of its own in the data directory.
- **Events and errors.** A plugin publishes its own event types
  (`ctx.publish(&T)` for `T: PluginEventKind`); on the wire they look like
  core events. Its error types implement `rpc::ErrorCode` (their code,
  and any detail from the device) in its `rpc.rs`; core errors convert
  with `?`.

`PluginContext` offers: `device(id)` and `device_changed(id)`;
`send(device, packet)` (paired, connected, and the peer advertised the
type), `can_send` and `broadcast(packet, except)`; `publish`;
`store()`; `transfers()`; and `payload_peer(device)`
for payload connections and SSH sign-in without the private key.

A new feature is:

- a module under `plugins/`: `mod.rs` implementing `core::Plugin` with the
  feature's typed Rust API and its unit tests against
  `core::testing::handle_with_plugin`, and `rpc.rs` for its control
  methods;
- one line in `builtin_plugins!` (the enum variant and its forwarding) and
  one in `plugins::builtin_parts()`, in `src/plugins/mod.rs`;
- its UI in `src/ui/features/<name>.rs` over the same API, plus its lines
  in `ui/features/mod.rs` (a `Feature` variant if it has messages, a line
  in each `Features` function that applies, maybe a `Route` variant);
- the CLI's commands in `cli.rs`, calling the methods with
  `Client::call`.

The UI calls the typed API, never the socket, so anything the UI does the
CLI can do too. Update this document by hand.
[`archive/feature-modules.md`](archive/feature-modules.md) records how
the daemon was moved to this shape, feature by feature, and what each step
taught.

## 3. Connection lifecycle

1. **Discovery** (`transport::lan`): UDP broadcast/listen on port 1716.
   Each peer broadcasts a protocol-v8 identity packet with its TCP port,
   the first in `1716-1764` that no other socket holds on its address or
   an overlapping one (macOS lets a 127.0.0.1 listener share a port with
   another process's on the wildcard address); payload listeners pick
   theirs the same way. Malformed, oversized, self, and
   unsupported-version identities are dropped without touching the device
   registry. Where broadcast doesn't reach a peer, its address can be
   given (`POST /discovery` with `address`): the identity goes by unicast
   to that IPv4 address on port 1716, and the peer dials back as after a
   broadcast. An address is a `core::Host`: an IPv4 address or a hostname
   (a tailnet's MagicDNS name, an internal domain), which the system
   resolver (`getaddrinfo` through tokio's `lookup_host`, no DNS client of
   our own) turns into IPv4 addresses each time it is used, with a
   3-second limit per name, so a device whose IP changes is still found.
   Only unicast IPv4 answers are used, and the port and payload are
   fixed, so the endpoint can't be used as a general UDP sender. A paired
   device can also keep addresses (`core.addresses`, a `PerDevice` config
   of hosts, stored as text, in the device snapshot as `addresses`): on
   its announce interval the transport resolves and announces to those
   of every paired device that isn't connected (`Core::fallback_addresses`),
   so a peer that only a tailnet or another subnet reaches is found again
   after a restart or an outage. Broadcast still wins: a connected device's
   addresses aren't used. Adding a device by address is
   `Core::connect_address` (`POST /devices/connect`): it announces to the
   address every two seconds (a name is looked up again each time) until
   a device connects from it (the peer address of a live connection, any
   of the name's addresses), for at most ten seconds, then answers
   with the device, ready to pair, or `504 address_unreachable`. Dropping
   the future cancels it. The address is kept only once the device is
   paired (at once if it already is): until then it waits in memory
   beside the connection, and is dropped if the connection ends first, so
   an address that never led to a paired device is never stored.
2. **Plaintext identity, then TLS** (`transport::tls`): the peer that
   received a UDP announcement dials the announced `tcpPort` (only UDP
   announcements carry it) and sends its identity once in plaintext, with
   `targetDeviceId` and `targetProtocolVersion`; the accepting peer only
   reads it (its identity already arrived over UDP). As in KDE Connect,
   TLS roles are inverted relative to TCP: the dialer is the TLS *server*
   and the acceptor the TLS *client*. The connection then upgrades to a
   TLS 1.2/1.3 handshake (rustls, real signature verification; this
   codebase has no accept-all verifier) and exchanges identity again
   *inside* TLS. Device ID and protocol version must match between the two
   exchanges; a mismatch, or a downgrade from a previously trusted
   protocol version, fails the connection closed.
3. **Trust check**: if the peer's device ID has a pinned certificate in the
   store, the TLS verifier requires an exact match. Unknown peers are
   accepted at the TLS layer so pairing can proceed, but can exchange only
   pairing packets until paired (§4).
4. **Steady state**: a per-connection read/write loop (`transport::lan`)
   hands each incoming packet to the core (`Core::handle_peer_packet`),
   which handles pairing packets itself and routes every other type to the
   plugin that claims it, for paired devices only (§2).

## 4. Pairing state machine

States: `requested → awaiting_confirmation → accepted | rejected | expired | failed`.

- One pairing resource represents both incoming and outgoing requests,
  distinguished by `direction`.
- A pairing always reaches a terminal state; its 30-second timeout timer is
  aborted on every terminal transition, so no task leaks.
- Trust is written to the store only after local user confirmation
  (`POST /pairings/{id}/accept` for incoming, or on receiving the peer's
  accept for outgoing), never before.
- A paired peer's trust record also keeps how it last described itself
  (name, type, capabilities), written at pairing and refreshed whenever it
  connects, never from a UDP announcement. At startup the daemon lists
  paired devices from these records, as `unavailable` until seen.
- `DELETE /pairings/{id}` cancels an in-flight pairing or unpairs/forgets a
  trusted device, removing its pinned certificate.
- Unpairing (`DELETE /devices/{id}`) sends `kdeconnect.pair {pair: false}`
  to a connected peer before closing the connection; the transport writes
  out already-queued packets when a connection is cancelled, so the notice
  isn't lost. A `pair: false` from a paired peer outside a pairing session
  removes its trust, sets `paired: false`, and publishes `device.updated`;
  the connection stays open (as in KDE Connect), so the device stays
  reachable and can be paired again.
- An incoming request's `timestamp` (seconds) must be within 30 minutes of
  the local clock, as in KDE Connect; requests without one, or further
  off, are dropped. This is separate from the 30-second pairing timeout:
  real devices routinely drift by more than 30 seconds.
- Verification codes, certificates and private keys never appear in a
  pairing snapshot or in logs.

## 5. Transfer state machine

States: `queued → connecting → transferring → completed | cancelled | failed`.

- Transfers are a core service (`core::Transfers`) used by every feature
  that moves a file: sharing and browsing (§12). A feature calls
  `PluginContext::transfers().begin(..)` (or `begin_as(..)`, with an id a
  client chose) and gets a `TransferHandle` that owns the state machine,
  records progress, and ends the transfer. A handle dropped without ending
  it fails the transfer (or cancels it if cancellation was asked for), so
  a transfer never outlives its task. The core lists (`GET /transfers`)
  and cancels (`DELETE /transfers/{id}`, a disconnect, shutdown) transfers
  whatever started them.
- Sharing (`plugins::share`) offers one file with
  `kdeconnect.share.request` on the control channel, then streams bytes
  over a **separate** auxiliary TLS payload connection
  (`transport::payload`) with the control channel's TLS material and
  pinning. Plugins open these through `PluginContext::payload_peer`, which
  never hands out the private key. `kdeconnect.share.request.update` isn't
  handled: one file per request. A payload listener waits, up to the
  connect timeout, for a connection that completes the handshake with the
  device's pinned certificate, and drops any other: a dial meant for a
  cancelled or timed-out transfer that had the port before can still
  arrive, and taking it would fail this transfer.
- The same `kdeconnect.share.request` carries text (`{"text": ...}`) or a
  link (`{"url": ...}`) with no payload, as KDE Connect's share sheet
  sends them. The receiver tells them apart as KDE Connect does:
  `filename` (or a payload) first, then `text`, then `url`. Only an `http`
  or `https` link (`share::is_web_link`) is published as a link, which
  the app opens in the browser at once, as KDE Connect's desktop does
  (it opens any scheme; Ferry doesn't); any other link is treated as text,
  which the app copies to the clipboard through the clipboard plugin
  (so it syncs like text copied here). Each is a notification. Neither is
  ever logged.
- Uploads stream from the file (the daemon reads the path a client
  names) straight to the network; downloads stream from the network
  straight to a temporary
  `.{transfer_id}.part` file. Neither buffers a whole file in memory.
- Incoming files: the declared size is checked against a configured
  maximum before dialing the peer; the filename is sanitized to a bare
  `file_name()` (no directory components, no `..`, not empty); the temp
  file is atomically renamed into place only after every declared byte is
  written.
- Progress is monotonic. `transfer.completed` is emitted only after
  durable local finalization (incoming) or a fully acknowledged send
  (outgoing). Every chunk updates the snapshot, but `transfer.progress` is
  published at most every 100 ms per transfer (plus the final byte), so a
  fast link can't overflow the bounded event bus.
- A completed incoming transfer's snapshot carries `savedPath`, the
  absolute path of the saved file (with a ` (n)` suffix when the name was
  taken), so clients can open the file or its folder.
- Cancellation, disconnect and daemon shutdown all remove the partial
  `.part` file and abort the task.
- Only paired devices can initiate or receive transfers.

## 6. Clipboard sync

- `kdeconnect.clipboard` carries `content` and applies unconditionally
  (subject to the duplicate-content guard below).
- `kdeconnect.clipboard.connect` also carries a millisecond `timestamp`
  and applies only if strictly newer than the last known update, so stale
  or replayed packets are ignored.
- Clipboard is a plugin (`src/plugins/clipboard/`): it holds the synced
  text behind its own lock, reads its `clipboard.syncEnabled` key (default
  `true`) from the store when it acts, offers its text to a device from the `connected`
  hook, and sends a change made here (`PUT /clipboard` or a local copy)
  to every device with `PluginContext::broadcast`.
- Text received from a peer is applied but sent to no one, as in KDE
  Connect: forwarding it would let devices pass text around in a loop.
  Identical content is never resent.
- Text is capped at `MAX_CLIPBOARD_TEXT_BYTES` (32 KiB); an oversized
  `PUT` gets a typed `413`, not silent truncation.
- Clipboard contents are never logged, only lengths.
- Backends implement `ClipboardService`. `SystemClipboard` is the desktop
  clipboard (`arboard`; on Linux the Wayland data-control protocol where
  the compositor has it, else X11/XWayland), selected by `RunRequest::
  system_clipboard` (`ferry-cli run --system-clipboard`; the app turns it on
  unless given `--no-system-clipboard`). `InMemoryClipboard` is the
  default, for tests and headless runs, and the fallback (logged as a
  warning) when the desktop clipboard can't be opened.
- `SystemClipboard` owns the clipboard on its own thread: it applies
  writes as they arrive and polls every 500 ms (`POLL_INTERVAL`) for text
  copied by other applications, reporting it through a `watch` channel
  that `ClipboardPlugin::follow_local_changes` feeds into `set_text`, the
  same path as `PUT /clipboard`. Text it wrote itself (e.g. from a peer)
  isn't reported. The follower also drops, as an echo, a change to text
  the clipboard holds or held within the last 5 s (`ECHO_WINDOW`),
  ignoring `\r\n` against `\n`: writing a peer's text can bring the text
  it replaced back (a clipboard manager restoring its entry), and syncing
  that would send old text out for each new one, which with a phone
  connected to two machines alternates their texts on it forever. Text already on the clipboard at start, empty text and
  non-text content (images) aren't reported, and copies made while sync
  is off are dropped.
- Sync is turned on or off with `ClipboardPlugin::set_sync_enabled`
  (`PATCH /clipboard`, `ferry-cli clipboard sync <true|false>`, "Sync
  clipboard" on the app's Settings page). The clipboard's snapshot
  carries it as `syncEnabled`, and a change publishes `clipboard.changed`
  like a change of text.
- Sending to one device on request (`POST /devices/{deviceId}/clipboard`,
  `ferry-cli clipboard send`, "Send clipboard" in the app and tray) covers
  what automatic sync can miss, e.g. text already on the clipboard at
  start or a peer that dropped an update. It reads the clipboard itself
  (falling back to the snapshot), sends a plain `kdeconnect.clipboard`
  even if the text is unchanged, works while sync is off, and leaves the
  snapshot alone.

## 7. Settings

User preferences live in the daemon's store (`ferry.db` in the data
directory), never in a client. Core fields: `deviceName`, `downloadDir`,
and the UI's `closeToTray`, `language` and `appearance` (the daemon
stores them without interpreting them, apart from checking that
`language` looks like a BCP 47 tag and `appearance` is `"light"` or
`"dark"`), under the config keys `core.deviceName`, `core.downloadDir`,
`ui.closeToTray`, `ui.language` and `ui.appearance` (`core::settings`).
Unset fields use their defaults: the host name (first label, trimmed to
a valid KDE Connect name, else "Ferry"), the platform download
directory, `true`, `null` (the system's language, §13) and `null` (the
system's light or dark mode, followed as it changes).

- **Starting on login** is the app's alone, not a daemon setting: the
  switch on the Settings page reads and writes the system's login item
  (`ui::desktop::autostart`), so a change made in the system's own
  settings shows too. On Linux it's an XDG autostart file
  (`$XDG_CONFIG_HOME/autostart`), on macOS a LaunchAgent in
  `~/Library/LaunchAgents`, on Windows a value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (turned off in
  Task Manager, under `StartupApproved`, counts as off). It runs the app
  with `--background`, which starts it in the tray (the window opens
  anyway without a tray), and the `--data-dir` it was given. The entry is
  named `dev.fanchao.Ferry` for the default data dir and
  `dev.fanchao.Ferry-<hash of the data dir>` otherwise, so an instance on
  another data dir never touches the default one's. While it's on, the
  app rewrites it at each start, in case the app moved.

- **Plugins' settings** aren't here: a plugin keeps each as a config key
  of its own and serves it on its own resource (§2), e.g. the clipboard's
  `syncEnabled` on `/clipboard` (§6). The Settings page still shows them,
  as the features' sections (`ui::features::Features::settings_sections`).
- **Precedence.** A start option (`ferry-cli run --device-name` /
  `--download-dir`, or the app's flags of the same names) overrides the
  stored value for that run only and isn't saved. Changing that setting
  through `PATCH /settings` saves it and drops the override for the rest
  of the run. The app passes these options only when given the flag (or
  its environment variable), so normal launches use the stored settings.
- **Changes** are validated (names follow the identity schema: 1–32
  characters, no reserved punctuation; download directories must be
  absolute and are created up front), saved in one transaction, then
  applied, and publish `settings.changed` if anything changed. A stored
  value that can't be read is logged and ignored at start, then
  overwritten on the next change.
- **Renaming** takes effect at once: the LAN transport watches the name,
  re-encodes its identity for new connections, and announces it. Peers
  update the name from any identity they receive, and KDE Connect
  re-dials on an announcement, so connected peers see the new name within
  a moment.
- **Download directory** is read when each incoming transfer starts, so a
  transfer in flight finishes where it began.

## 8. Control socket (JSON-RPC)

[`adr/0005`](adr/0005-control-the-daemon-over-a-unix-socket.md) records
why. A daemon serving it listens on `ferry.sock` in its data directory:
`ferry-cli run` always, the app while Settings → Command line access is on
(§9). Only on Unix (Linux and macOS); the app on Windows has no command
line access yet. The socket is `0600`, and a connection from another user
is closed unanswered: there is no token. One daemon per socket: a daemon
won't serve while another answers on it, and replaces a socket nobody
answers on (a crash's); anything there that isn't a socket is left alone.
A data directory whose socket path would be over 107 bytes (103 on macOS)
can't be served.

**Messages** are JSON-RPC 2.0, one per line (at most 1 MiB; a longer line
closes the connection). A request is `{"jsonrpc": "2.0", "id": 1,
"method": "share.text", "params": {"deviceId": "...", "text": "hi"}}`;
params are an object in camelCase (a method without any may leave them
out), unknown fields are refused. The answer is `{"jsonrpc": "2.0", "id":
1, "result": ...}` or `{..., "error": {"code", "message", "data": {"code",
"detail"?}}}`. `error.code` is JSON-RPC's: `-32700` an unparseable line
(answered with `id: null`), `-32600` not a request, `-32601` an unknown
method, `-32602` invalid params, `-32603` an internal error, and
`-32000` the method's own failure, with the daemon's code in
`data.code` (e.g. `device_not_paired`) and any reason
from the device in `data.detail`. A request without an `id` is run and not
answered.

Requests on one connection run concurrently and are answered as they
finish, not in order. Closing the connection drops the requests it still
has running: interrupting `ferry-cli send` ends its transfer as short
once the device connects for it.

**Streams.** A stream method sends items before it answers, as
`{"jsonrpc": "2.0", "method": "stream", "params": {"id": <request id>,
"item": ...}}`.

| Method | Params | Answer and notes |
| --- | --- | --- |
| `status` | | Version, uptime, local device summary, protocol version. |
| `devices.list` | | Known devices. Each carries `plugins`, an object keyed by plugin id with what that plugin adds to the device; a plugin with nothing to add has no key, so it is often `{}`. So far `battery`: `{"charge": 0-100, "charging": bool}` from the peer's latest `kdeconnect.battery` report, and `connectivity`: `{"subscriptions": [{"id": "6", "networkType": "LTE", "signalStrength": 0-4}]}` from its latest `kdeconnect.connectivity_report` (one entry per SIM, in id order; `networkType` as the phone names it, `"Unknown"` when it doesn't know). Each is present once a paired, connected peer has reported and removed when it disconnects or is unpaired; and `telephony`: `{"state": "ringing"\|"talking", "contactName"?, "phoneNumber"?}`, the call going on on a phone, removed when it ends; and `notifications`: `{"enabled": bool}`. A change publishes `device.updated`. Clients should ignore unknown keys. |
| `devices.get` | `deviceId` | One device; `device_not_found`. |
| `devices.scan` | `address`? | Announce identity now; `null`. With `address` (`"192.168.1.20"`, or a hostname like `"phone.tailnet.ts.net"`), to that unicast IPv4 address or every IPv4 address the name resolves to; `unresolvable_address` for a name that doesn't resolve, invalid params for anything but an IPv4 address or a hostname. |
| `devices.connect` | `address` | Announce to that address, or what the name resolves to, until a device connects from it (up to 10 s), and answer with the device, ready to pair. `address_unreachable` if none did, `unresolvable_address`. The address is saved once that device is paired, or now if it already is; nothing is saved on failure. |
| `devices.setAddresses` | `deviceId`, `addresses` | Replace the addresses a paired device is reached at when broadcast doesn't find it (at most 8, repeats dropped: `too_many_addresses`, `device_not_paired`). Answers with what is saved; the snapshot's `addresses` and `device.updated` carry it. Unpairing removes them. |
| `devices.forget` | `deviceId` | Unpair, remove trust, forget the device. |
| `ping.send` | `deviceId`, `message`? | Send `kdeconnect.ping` to a paired, connected device that advertises receiving it. |
| `findmyphone.ring` | `deviceId` | Send `kdeconnect.findmyphone.request`, making the device ring until dismissed on it. |
| `telephony.call` | `deviceId` | The call going on on a paired, connected phone (`plugins.telephony` of its snapshot), or `null`; `device_not_found`. |
| `telephony.mute` | `deviceId` | Send `kdeconnect.telephony.request_mute` to a phone whose call is ringing, muting its ringer until the call ends; `not_ringing` while no call rings. |
| `pairings.list` | | Every pairing in this daemon session, finished ones included, so a client can find requests still awaiting confirmation after (re)connecting. |
| `pairings.start` | `deviceId` | Start outgoing pairing; answers with the pairing. |
| `pairings.get` | `pairingId` | Pairing state, verification code, expiry; `pairing_not_found`. |
| `pairings.accept` | `pairingId` | Confirm verification codes match (incoming only). |
| `pairings.cancel` | `pairingId` | Reject, cancel or unpair. |
| `share.text` | `deviceId`, `text` | Send text (`kdeconnect.share.request` with `text`, no payload; KDE Connect for Android copies it to its clipboard). Blank is `share_empty`, over 32 KiB `share_too_large`. |
| `share.url` | `deviceId`, `url` | Send a link, trimmed (`kdeconnect.share.request` with `url`; the device opens it). Same errors as `share.text`. |
| `share.file` | `deviceId`, `path` | Send the file at `path`, an absolute path on this machine that the daemon reads, under its own name. Answers with the transfer once the whole file has gone into it, or as soon as the transfer ends if that comes first (cancelled or failed; its `status` says which). `file_unreadable` (with the reason in `detail`) for a path that isn't an absolute path to a readable regular file. |
| `transfers.list` | | Active and recent transfers. |
| `transfers.get` | `transferId` | State, byte counts, safe metadata; `transfer_not_found`. |
| `transfers.cancel` | `transferId` | Cancel an active transfer. |
| `files.list` | `deviceId`, `path`? | List a directory on a paired device, or without `path` the storage roots it shares, as `{path, entries: [{name, path, kind, size?, modifiedAt?}]}`. `kind` is `file`, `directory`, `symlink` or `other`; links show as what they point to. §12. |
| `files.read` | `deviceId`, `path` | Stream a file's bytes, as base64 items of up to 64 KiB each, then answer with the number of bytes. For previews and `ferry-cli files cat`; not a transfer. |
| `files.download` | `deviceId`, `path` | Save the file into the download directory as an incoming transfer; answers with the transfer once started. |
| `files.upload` | `deviceId`, `directory`, `path` | Upload the local file at `path` (as `share.file`) into `directory` on the device, as an outgoing transfer; a taken name gets a ` (n)` suffix. Answers as `share.file`. |
| `files.mkdir` | `deviceId`, `path` | Create a directory; answers with its entry. |
| `files.move` | `deviceId`, `from`, `to` | Move or rename; `file_exists` rather than replacing anything. |
| `files.delete` | `deviceId`, `path` | Delete a file, or a directory and everything in it. Storage roots can't be moved or deleted (`invalid_path`). |
| `notifications.list` | `deviceId` | The notifications a paired, connected device shares, newest first: `[{id, appName, title?, text?, time?, dismissable, repliable, actions, hasIcon}]`. `id` is the device's own (Android's notification key, which holds `\|`). Kept in memory, at most 100 per device, emptied when the device disconnects or is unpaired. `device_not_found`. |
| `notifications.setEnabled` | `deviceId`, `enabled` | Show a paired device's notifications here, or stop (they are on until turned off). Off forgets its notifications, publishing `notification.removed` for each, and ignores what it sends until turned on again, which asks a connected device for the ones it shows. The device's snapshot has it as `plugins.notifications.enabled`, and `device.updated` announces a change. `device_not_found`, `device_not_paired`. |
| `notifications.icon` | `deviceId`, `id` | The notification's icon as base64 PNG once fetched (`hasIcon`), else `icon_not_found`. |
| `notifications.reply` | `deviceId`, `id`, `message` | Answer a notification that takes a reply. `empty_reply`, `notification_not_repliable`. |
| `notifications.action` | `deviceId`, `id`, `action` | Press one of its buttons, by label. `unknown_notification_action`. |
| `notifications.dismiss` | `deviceId`, `id` | Dismiss it on the device; it is removed at once. `notification_not_dismissable`. Besides the device errors, these fail with `notification_not_found` for an id the device doesn't show. |
| `clipboard.get` | | Current synchronized text and metadata: `{text, updatedAt, sourceDeviceId?, syncEnabled}`, the data of `clipboard.changed`. |
| `clipboard.set` | `text` | Set text and send to eligible paired devices; `clipboard_text_too_large`. |
| `clipboard.setSync` | `enabled` | Turn sync with paired devices on or off. Answers with the snapshot; a change publishes `clipboard.changed`. §6. |
| `clipboard.send` | `deviceId` | Send this machine's clipboard text to one paired, connected device now. `clipboard_empty` when there is no text, `unsupported_by_peer` without `kdeconnect.clipboard`. §6. |
| `settings.get` | | The settings in effect (§7): `deviceName`, `downloadDir`, `closeToTray`, `language` (the app's, a BCP 47 tag such as `"de"`, or `null` for the system's), `appearance` (the app's, `"light"` or `"dark"`, or `null` for the system's). |
| `settings.update` | any settings | Change the fields present; `null` resets one to its default, unknown fields are refused. `invalid_device_name` / `invalid_download_dir` / `invalid_settings` (a `language` that isn't a tag) for bad values. Answers with the new settings. |
| `events.subscribe` | | A stream: first `null`, once subscribed, then every event as `{sequence, event: {type, data}}`: `device.discovered/connected/updated/disconnected/forgotten`, `pairing.requested/updated`, `transfer.started/progress/completed/failed`, `clipboard.changed`, `settings.changed`, `notification.posted` (`{deviceId, deviceName, notification, alert}`: a notification posted or changed, including its icon arriving; `alert` is set for news, i.e. new or with new text, and not marked as already shown by the device) and `notification.removed` (`{deviceId, id}`), `ping.received` (`{deviceId, deviceName, message?}` from a paired device; a one-off with no snapshot, so one missed during a gap is lost), `share.received` (`{deviceId, deviceName, kind: "text", text}` or `{..., kind: "link", url}`: text or an `http(s)` link a paired device shared; a one-off like `ping.received`), `call.missed` (`{deviceId, deviceName, contactName?, phoneNumber?}`: a call that rang out unanswered; a one-off like `ping.received`). It never answers, except with `events_lagged` when the client fell behind and missed some. Not durable: a client subscribes, then takes its snapshot, and does both again after a gap. |

Methods that need a network round trip answer once it has started and
are tracked through the resource's own state (poll it or watch
`events.subscribe`); events are notifications, not the source of truth.

Besides the device errors (`device_not_paired`, `device_not_connected`,
`unsupported_by_peer`), the `files.*` methods fail with `files_unavailable`
when the device won't share its files (with its reason in `detail` when it
gave one), `file_not_found`, `file_permission_denied`, `not_a_directory` /
`is_a_directory`, `invalid_path` (not absolute, or a `.`/`..` segment),
`files_host_key_mismatch`, `files_failed` or `files_timed_out`.

## 9. Embedding: the UI runs the daemon in-process

The desktop app (`gui/src/main.rs`) builds a tokio runtime and starts a
`RunningService` with `start_with`, passing a closure that calls
`plugins::builtin_parts`. That builds each plugin once and keeps the
clipboard, browse and notifications instances, which go with the running
core to `ui::run` (`ui::Started`) to build the feature UIs. There is no
FFI and no socket between them:

- **Reads.** `ui::sync` subscribes to the event bus, takes snapshots from
  `Core` (devices, pairings, transfers, settings) into `ui::store`,
  patches them from events, and takes a fresh snapshot after the receiver
  lags.
- **Actions.** The UI calls the core and each plugin's typed Rust API (the
  functions its `rpc.rs` also calls). Anything doing I/O runs as a task
  on the daemon's tokio runtime (`UiOptions::runtime`); iced's executor
  never touches the daemon's sockets.
- **The control socket is opt-in.** The UI doesn't need it, so the
  embedded daemon serves it only while **Settings → Command line access**
  is on (`daemon::ControlMode::Stored`). `daemon::ControlSwitch` starts
  and stops the server while the app runs (each on a child of the
  service's cancellation token) and keeps the choice in the store
  (`core.api`). `ferry-cli` run by the same user finds it in the data
  directory with no setup. If another daemon (a `ferry-cli run` on the
  same data directory) answers on the socket, the switch stays off and
  says why; stored as on, the app starts anyway, with the reason on the
  Settings page. `--cli-access` turns it on for one run, not stored.
  `ferry-cli run` serves it for the whole run instead
  (`ControlMode::Always`), which can't be switched. On Windows the
  setting isn't shown.
- **Lifetime.** The UI starts the daemon (again on Retry after a failed
  start). The app keeps running in the tray with its window closed, so the
  daemon stops only when the user quits
  ([`archive/flutter-adr/0007`](archive/flutter-adr/0007-keep-running-in-the-tray.md),
  carried over by [`adr/0001`](adr/0001-native-ui-in-iced.md)).

## 10. Testing

Integration tests in `tests/` are organized by concern: `protocol.rs`,
`tls.rs`, `lan.rs`, `pairing.rs` / `pairing_e2e.rs`, `ping_e2e.rs`,
`clipboard_e2e.rs`, `transfer_e2e.rs`, `share_text_e2e.rs`,
`browse_e2e.rs`, `notifications_e2e.rs`, `client.rs`, `rpc.rs`. Most
end-to-end tests run two in-process peers (real UDP/TCP/TLS on loopback,
no mocked network) and exercise everything from discovery to encrypted
plugin dispatch.
`browse_e2e.rs` and `notifications_e2e.rs` run against a fake KDE Connect
for Android (`tests/support/fake_phone.rs`), which `examples/fake_phone.rs`
also runs standalone for trying the app without a phone.

`ui_e2e.rs` (needs `gui`) runs the whole desktop app headless in
`iced_test`'s emulator against a second daemon and the fake phone: pairing
both ways, unpairing, ping, clipboard, files both ways, browsing, and
notifications. Each scenario discovers on its own free UDP port, not
1716, so a Ferry or KDE Connect running on the machine doesn't see it
(§2, `transport`). The UI's unit tests sit next to each page and plugin UI
half, over a real core from `core::testing` (on `Store::open_in_memory()`)
with fake desktop services; snapshot tests render each page to PNG in
light and dark when `SNAPSHOT_DIR` is set, and in the en-XA
pseudo-locale and any translations named in `SNAPSHOT_LANGUAGES` (§13).

Before a change counts as done:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p ferry   # the CLI alone, without iced
git diff --check
```

Run `cargo test` under a private display and D-Bus session (see
`CLAUDE.md`).

## 11. Known gaps

Prioritized next work, with notes for each item, is in
[`HANDOFF.md`](HANDOFF.md).

- Interoperability was checked by hand on 2026-09-24 against KDE Connect
  for Android (Pixel 8a, protocol v8) over a real LAN, with the CLI
  daemon. Working both ways: discovery, TLS handshake, pairing with
  matching verification codes, unpairing, clipboard (the phone sends only
  when the user taps "Send clipboard", an Android 10+ restriction), and
  file transfer (3 MB, byte-identical). Ping to the phone works; ping from
  the phone was dropped then and is handled now (`ping.received`), but
  that direction hasn't been rechecked against the phone. Not yet checked
  against KDE Connect on desktop, nor from the desktop app (which embeds
  the same daemon). The check found two bugs, both fixed: the CLI's upload
  omitted the file part's `Content-Length` header, and incoming pair
  requests were dropped when the clocks differed by more than 30 seconds.
- The desktop clipboard was checked live on X11 only (two daemons on
  separate Xvfb displays), not on a Wayland compositor. Compositors
  without data-control (e.g. GNOME) fall back to XWayland, which is
  untested.
- Adding by address and reaching a device at its saved addresses have been
  checked between Ferry instances only, not against KDE Connect or over a
  real tailnet. Saved addresses are announced to on the 30-second announce
  interval, so a peer that comes back is found within that.
- No Bluetooth transport, no multi-file/directory transfer, no durable
  event replay, no remote/LAN exposure of the control socket, no
  command line access on Windows (it needs a named-pipe transport):
  explicit non-goals for the current scope, not oversights.

## 12. Browsing a device's files

KDE Connect for Android shares its storage over SFTP; no other KDE Connect
client serves files. Ferry is a client only: the UI's reasoning is in
[`archive/flutter-adr/0008`](archive/flutter-adr/0008-browse-device-files-in-the-app.md),
which [`adr/0001`](adr/0001-native-ui-in-iced.md) carries over.

- **Offer.** The first file request for a device sends
  `kdeconnect.sftp.request {"startBrowsing": true}` and waits up to 5
  seconds for `kdeconnect.sftp`, which carries `port`, `user`, a one-off
  `password` and the roots (`multiPaths` named by `pathNames`, else
  `path`). An `errorMessage` reply becomes `files_unavailable` with that
  message as `detail`. The `ip` field is ignored: as KDE Connect does, the
  daemon connects to the address of the existing control connection.
- **Connection** (`plugins::browse::ssh`, russh + russh-sftp, 8-second
  deadline). The peer's SSH host key must equal the public key in its
  pinned TLS certificate, since Android uses its KDE Connect key pair as
  the host key (KDE Connect's own clients skip this check). A mismatch
  fails with `files_host_key_mismatch` before any credential is sent. The
  daemon signs in with its own TLS key, which Android accepts from the
  paired device, and falls back to the one-off password. The plugin never
  holds the key: the core signs in for it
  (`PayloadPeer::authenticate_ssh`).
- **Session.** One session per device, opened on demand, shared by
  concurrent requests (opening is serialized per device) and reused. It
  is dropped when the device disconnects, is unpaired or forgotten, when
  the peer sends `{"serverRunning": false}` (Android's plugin reloaded),
  when a request finds the SSH connection closed, at daemon shutdown, and
  after 5 minutes unused. A download or upload in progress keeps it open.
- **Paths.** Every path is the peer's absolute path. The daemon rejects
  relative paths, NUL bytes and `.`/`..` segments, and strips repeated and
  trailing `/`. What a path can reach is up to the peer's server. `/` and
  the roots can't be moved or deleted.
- **Copies.** Downloads are incoming transfers, saved like received files
  (a `.part` file renamed into place, a ` (n)` suffix on collisions).
  Uploads are outgoing transfers into a file created with `EXCLUDE` under
  a free name; SFTP v3 reports "exists" only as a generic failure, so the
  daemon checks first. An upload that fails or is cancelled is removed
  from the peer. Moves and new folders also refuse to replace anything.
- **No events.** Nothing tells the daemon when files change on the device,
  so listings are fetched when needed; there is no `files.*` event.
- **Limits.** A recursive delete runs within the 15-second request
  deadline, so deleting a very large tree can stop partway.
- **Checked on Android.** A Pixel 8a (KDE Connect for Android, 2026-09)
  accepted our ECDSA key, its host key matched its certificate, and it
  offered one root, `/storage/emulated/0` ("Internal shared storage").

## 13. The app's languages

The desktop app is translated; the CLI, the control socket (it reports
error codes, which the app words), logs and the website stay in English. The
completed implementation plan is in
[`archive/PLAN_I18N.md`](archive/PLAN_I18N.md).

- **Messages.** Every word the app shows is a Fluent message in
  `i18n/<lang>/ferry.ftl`, embedded in the binary (`rust-embed`). en-US is
  the source and the fallback: a message a translation lacks shows in
  English. Code gets one with `fl!("key", name = value)`
  (`ui::i18n::fl`, over `i18n-embed-fl`), which checks at compile time
  that the key exists in en-US and is given exactly the arguments its
  message uses. Keys are prefixed by feature or page (`browse-…`,
  `settings-…`), `error-<code>` for the daemon's error codes (`ui::error`).
  Messages are whole sentences; device and file names, paths and numbers
  are arguments, never glued to translated text; every count goes through
  a plural selector (`{ $count -> [one] … *[other] … }`).
- **Choosing the language.** `launch::run` calls
  `i18n::select_system_language` once at start: `FERRY_LANG` if set,
  else the system's preferred languages (`DesktopLanguageRequester`),
  negotiated against the shipped ones (so macOS's `zh-Hans-CN` and
  `de-AT` reach `zh-CN` and `de`), else en-US. Fluent's isolation marks
  wrap each argument, so a right-to-left name can't reorder a sentence.
  Unit tests and `tests/ui_e2e.rs` stay in en-US without the marks.
- **The language setting.** The daemon's `language` setting (§7; Settings'
  list, or `ferry-cli settings --language <tag|system>`) overrides the
  system's. After each message, `App::update` hands it to
  `i18n::follow_setting`, which reloads the loader when it changed (the
  chosen tag, or the system's languages again for `null`; `FERRY_LANG`
  still wins) and puts back isolation and number formatting, which a
  reload drops. The window redraws and the tray menu is sent again with
  the new labels, without a restart; the Linux login item is rewritten
  for its comment. Until the daemon's settings are read at start, the app
  shows the system's language. Settings lists each `i18n/` language by
  its `settings-language-own-name`; en-XA only by tag, from the CLI.
  `tests/i18n_switch.rs` switches the real loader in a process of its own.
- **Numbers and dates** in a message are written by ICU4X in the user's
  locale (`i18n::format`, installed as Fluent's formatter); units and a
  percent sign are the message's, so each language spaces them.
- **Testing.** `ui::i18n::tests` checks that every `i18n/*/ferry.ftl`
  parses and has exactly en-US's keys. en-XA (`i18n::pseudo`, made from
  en-US at run time, never checked in) shows untranslated or clipped text:
  `FERRY_LANG=en-XA` in the app, and in every snapshot. `SNAPSHOT_LANGUAGES=de,zh-CN`
  also renders the snapshots in those translations (`i18n::in_locale`, per
  thread, so parallel tests stay en-US).
- **Outside the app.** `package-*` messages (plain text, one line) are
  what the system shows about the app: `packaging/i18n.sh` copies them into
  the `.desktop` entry, macOS's `<lang>.lproj` and `CFBundleLocalizations`,
  and the Windows installer's languages. A language is shipped by having
  its directory under `i18n/`.
- **Fonts.** The bundled Inter covers Latin, Greek and Cyrillic; other scripts fall back to
  the system's fonts through cosmic-text, which picks CJK fonts by the
  system's locale, not the app's. On macOS bold CJK text mixes fonts:
  PingFang has no bold (700) face, and cosmic-text's fallback only takes an
  exact weight, so it lands on whichever bold font has the glyph.
